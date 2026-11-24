use super::{Result, plugin::Plugin, profile::Config};
use crate::{LocalStream, code_agent, connect_endpoint, editor_endpoint, session_host};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::time::{Duration, Instant, timeout, timeout_at};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

const RPC_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) struct Connection {
    socket: WebSocketStream<LocalStream>,
    request_id: u64,
    pub session: String,
    pub chat: String,
    pub turn: Option<String>,
    client_id: String,
    pub start_seq: Option<u64>,
    pub terminal: Option<&'static str>,
    pub events: u64,
    pub session_revision: u64,
    pub last_event: Value,
    pub server_seq: u64,
    channel_sequences: [Option<u64>; 2],
    pub snapshots: std::collections::BTreeMap<String, u64>,
    pub plugin: Option<Plugin>,
}

impl Connection {
    pub async fn open(config: &Config, receipt: &Value) -> Result<Self> {
        let target = &config.target;
        let discover = async |args: &[&str]| -> Result<Value> {
            let cli = target.cli.to_str().ok_or("invalid editor CLI path")?;
            let profile = target
                .profile
                .to_str()
                .ok_or("invalid editor profile path")?;
            let output = code_agent(cli, args, Some(profile))?
                .env_remove("VSCODE_IPC_HOOK_CLI")
                .output()
                .await
                .map_err(|_| "editor discovery failed")?;
            if !output.status.success() {
                return Err("editor discovery failed");
            }
            serde_json::from_slice(&output.stdout).map_err(|_| "invalid editor inventory")
        };
        let session = receipt["session"].as_str().ok_or("missing session")?;
        let hosts = discover(&["ps", "--all", "--json"]).await?;
        let pid = session_host(&hosts, session)?;
        let endpoints = discover(&["endpoints"]).await?;
        let endpoint = editor_endpoint(&endpoints, pid)?;
        if endpoint["instanceId"] != target.instance_id {
            return Err("saved editor instance differs from session host");
        }
        let socket = connect_endpoint(endpoint).await?;
        Ok(Self {
            socket,
            request_id: 0,
            session: session.into(),
            chat: receipt["chat"].as_str().ok_or("missing chat")?.into(),
            turn: receipt["attempt"]["turnId"].as_str().map(str::to_owned),
            client_id: receipt["clientId"].as_str().ok_or("missing client")?.into(),
            start_seq: if receipt["attempt"]["outcome"] == "uncertain" {
                receipt["attempt"]["clientSeq"].as_u64()
            } else {
                None
            },
            terminal: None,
            events: 0,
            session_revision: 0,
            channel_sequences: [None, None],
            last_event: Value::Null,
            server_seq: receipt["serverSeq"].as_u64().unwrap_or(0),
            snapshots: Default::default(),
            plugin: None,
        })
    }

    async fn send(&mut self, msg: Value) -> Result<()> {
        timeout(
            RPC_TIMEOUT,
            self.socket.send(Message::text(msg.to_string())),
        )
        .await
        .map_err(|_| "socket write timed out")?
        .map_err(|_| "socket write failed")
    }

    pub async fn dispatch(&mut self, channel: &str, seq: u64, action: Value) -> Result<()> {
        let params = json!({"channel": channel, "clientSeq": seq, "action": action});
        self.send(json!({"jsonrpc": "2.0", "method": "dispatchAction", "params": params}))
            .await
    }

    // A request can race a terminal action or a host read of a plugin file. Handle
    // both on this same socket while waiting for the correlated response.
    pub async fn receive_until(&mut self, deadline: Instant) -> Result<Option<Value>> {
        let Ok(frame) = timeout_at(deadline, self.socket.next()).await else {
            return Ok(None);
        };
        let frame = frame
            .ok_or("host disconnected")?
            .map_err(|_| "host disconnected")?;
        let msg: Value = match frame {
            Message::Text(text) => serde_json::from_slice(text.as_bytes()),
            Message::Binary(bytes) => serde_json::from_slice(&bytes),
            Message::Close(_) => return Err("host disconnected"),
            _ => return Ok(None),
        }
        .map_err(|_| "invalid host message")?;
        if let Some(method) = msg["method"].as_str() {
            if !msg["id"].is_null() {
                let result = self
                    .plugin
                    .as_ref()
                    .and_then(|p| p.resource(method, &msg["params"]));
                let reply = match result {
                    Some(value) => json!({"jsonrpc":"2.0","id":msg["id"],"result":value}),
                    None => {
                        json!({"jsonrpc":"2.0","id":msg["id"],"error":{"code":-32009,"message":"Resource unavailable"}})
                    }
                };
                self.send(reply).await?;
            } else if method == "action" {
                self.observe_action(&msg["params"]);
            }
            Ok(None)
        } else {
            Ok(Some(msg))
        }
    }

    fn observe_action(&mut self, envelope: &Value) {
        let channel = &envelope["channel"];
        let slot = if channel == &self.session {
            0
        } else if channel == &self.chat {
            1
        } else {
            return;
        };
        let action = &envelope["action"];
        let watched =
            slot == 1 && self.turn.is_some() && action["turnId"].as_str() == self.turn.as_deref();
        let rejected = !envelope["rejectionReason"].is_null();
        // A reconnect reply can replay this echo after a newer live event.
        // Match the dispatch itself independently of the ordering cursor.
        if envelope["rejectionReason"].is_string()
            && watched
            && action["type"] == "chat/turnStarted"
            && envelope["origin"]["clientId"] == self.client_id
            && self.start_seq.is_some()
            && envelope["origin"]["clientSeq"].as_u64() == self.start_seq
        {
            self.terminal = Some("rejected");
        }
        if let Some(seq) = envelope["serverSeq"].as_u64() {
            if self.channel_sequences[slot].is_some_and(|previous| previous >= seq) {
                return;
            }
            self.channel_sequences[slot] = Some(seq);
            self.server_seq = self.server_seq.max(seq);
        }
        self.events += 1;
        if slot == 0 {
            self.session_revision += 1;
        }
        self.last_event = json!({"type":action["type"], "at":super::now(), "turnId":action["turnId"], "toolCallId":action["toolCallId"]});
        if watched && !rejected && self.terminal != Some("rejected") {
            match action["type"].as_str() {
                Some("chat/turnComplete") => self.terminal = Some("complete"),
                Some("chat/turnCancelled") => self.terminal = Some("cancelled"),
                Some("chat/error") => self.terminal = Some("error"),
                _ => {}
            }
        }
    }

    async fn rpc(&mut self, method: &str, params: Value) -> Result<Value> {
        self.request_id += 1;
        let id = self.request_id;
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        timeout(RPC_TIMEOUT, async {
            loop {
                if let Some(reply) = self.receive_until(Instant::now() + RPC_TIMEOUT).await?
                    && reply["id"] == id
                {
                    return Ok(reply);
                }
            }
        })
        .await
        .map_err(|_| "host request timed out")?
    }

    pub async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.rpc(method, params)
            .await?
            .get_mut("result")
            .map(Value::take)
            .ok_or("host rejected request")
    }

    pub async fn attach(&mut self, client_id: &str, reconnect: bool) -> Result<()> {
        if reconnect {
            let resume = json!({
                "channel": "ahp-root://", "clientId": client_id,
                "lastSeenServerSeq": self.server_seq, "subscriptions": [self.session, self.chat]
            });
            let reply = self.rpc("reconnect", resume).await?;
            if let Some(result) = reply.get("result") {
                if result["type"] == "replay"
                    && let Some(actions) = result["actions"].as_array()
                {
                    for envelope in actions {
                        self.observe_action(envelope);
                    }
                }
                return Ok(());
            }
            if reply["error"]["code"] != -32008 {
                return Err("cannot reconnect saved client");
            }
        }
        let params = json!({
            "channel": "ahp-root://", "clientId": client_id,
            "protocolVersions": ["1.1.0"], "initialSubscriptions": []
        });
        let init = self.request("initialize", params).await?;
        if init["protocolVersion"] != "1.1.0" {
            return Err("full assignments require AHP 1.1.0");
        }
        self.server_seq = init["serverSeq"].as_u64().unwrap_or(0);
        Ok(())
    }

    pub async fn snapshot(&mut self, channel: &str) -> Result<Value> {
        *self.snapshots.entry(channel.to_owned()).or_default() += 1;
        let mut reply = self
            .request("subscribe", json!({"channel":channel}))
            .await?;
        let snap = &mut reply["snapshot"];
        if (!snap["resource"].is_null() && snap["resource"] != channel)
            || !snap["state"].is_object()
        {
            return Err("unexpected channel snapshot");
        }
        Ok(snap["state"].take())
    }
}
