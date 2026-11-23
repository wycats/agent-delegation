//! Attach to a chat on the local VS Code Agent Host.
//! Discovers credentials in memory through VS Code; never writes or prints them.
use ahp::{Client, ClientConfig, Transport, TransportError, TransportMessage};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
#[cfg(unix)]
use tokio::net::UnixStream as LocalStream;
#[cfg(windows)]
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient as LocalStream};
use tokio::{
    process::Command,
    time::{Duration, timeout},
};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

const USAGE: &str = "agent-delegation ahp models|chats <session-uri>\nagent-delegation ahp status <session-uri> <chat-uri>\nagent-delegation ahp send|steer <session-uri> <chat-uri> <model-id> <prompt-file>";

fn matches_ack(value: &Value, chat: &str, id: &str, client_seq: i64) -> bool {
    value["action"]["id"] == id
        && value["action"]["type"] == "chat/pendingMessageSet"
        && value["channel"] == chat
        && value["origin"]["clientSeq"] == client_seq
}
fn session_host(hosts: &Value, session: &str) -> Result<u64, &'static str> {
    let matches: Vec<_> = hosts
        .as_array()
        .into_iter()
        .flatten()
        .filter(|host| {
            host["sessions"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|s| s["resource"] == session)
        })
        .filter_map(|host| host["host"]["pid"].as_u64())
        .collect();
    if matches.len() != 1 {
        return Err("session host missing or ambiguous");
    }
    Ok(matches[0])
}

fn editor_endpoint(endpoints: &Value, pid: u64) -> Result<&Value, &'static str> {
    let entries = endpoints["endpoints"]
        .as_array()
        .ok_or("missing endpoint inventory")?;
    let mut matches = entries
        .iter()
        .filter(|entry| entry["pid"] == pid && entry["type"] == "editor");
    let endpoint = matches.next().ok_or("editor endpoint missing")?;
    if matches.next().is_some() {
        return Err("ambiguous editor endpoint");
    }
    Ok(endpoint)
}
fn pending_action(
    id: &str,
    text: &str,
    kind: &str,
    model: &str,
) -> Result<ahp_types::actions::StateAction, &'static str> {
    if !matches!(kind, "queued" | "steering") {
        return Err("unsupported pending message kind");
    }
    serde_json::from_value(json!({
        "type": "chat/pendingMessageSet", "kind": kind, "id": id,
        "message": {"text": text, "origin": {"kind": "user"}, "model": {"id": model}}
    }))
    .map_err(|_| "invalid typed pending message")
}
fn code_agent() -> Result<Command, &'static str> {
    // A compatible distribution can expose the same two discovery commands
    // under a different executable name. No session data goes through a shell.
    let cli = std::env::var("AGENT_DELEGATION_CODE").unwrap_or_else(|_| "code".into());
    #[cfg(windows)]
    {
        let mut command = if cli.to_ascii_lowercase().ends_with(".exe") {
            Command::new(&cli)
        } else {
            if cli.is_empty()
                || !cli
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            {
                return Err("use a CLI name on PATH or a full .exe path for AGENT_DELEGATION_CODE");
            }
            let mut cmd = Command::new("cmd.exe");
            cmd.args(["/d", "/c", &cli]);
            cmd
        };
        command.kill_on_drop(true).arg("agent");
        Ok(command)
    }
    #[cfg(unix)]
    {
        let mut command = Command::new(cli);
        command.kill_on_drop(true).arg("agent");
        Ok(command)
    }
}

#[cfg(windows)]
async fn local_stream(path: &str) -> Result<LocalStream, &'static str> {
    if !path.starts_with(r"\\.\pipe\vscode-agent-host-") {
        return Err("unexpected pipe namespace");
    }
    ClientOptions::new()
        .open(path)
        .map_err(|_| "local pipe connection failed")
}

#[cfg(unix)]
async fn local_stream(path: &str) -> Result<LocalStream, &'static str> {
    use std::os::unix::fs::FileTypeExt;
    if !std::path::Path::new(path).is_absolute() {
        return Err("expected absolute local socket path");
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|_| "cannot inspect local socket")?;
    if !metadata.file_type().is_socket() {
        return Err("expected a Unix socket");
    }
    let stream = LocalStream::connect(path)
        .await
        .map_err(|_| "local socket connection failed")?;
    let peer = stream
        .peer_cred()
        .map_err(|_| "cannot verify socket peer")?;
    // SAFETY: geteuid takes no arguments and has no preconditions.
    if peer.uid() != unsafe { libc::geteuid() } {
        return Err("socket peer belongs to a different user");
    }
    Ok(stream)
}

struct LocalWs(WebSocketStream<LocalStream>);
impl Transport for LocalWs {
    async fn send(&mut self, m: TransportMessage) -> Result<(), TransportError> {
        let text = match m {
            TransportMessage::Text(s) => s,
            TransportMessage::Binary(b) => {
                String::from_utf8(b).map_err(|_| TransportError::Closed)?
            }
            TransportMessage::Parsed(p) => {
                serde_json::to_string(&p).map_err(|_| TransportError::Closed)?
            }
        };
        self.0
            .send(Message::Text(text.into()))
            .await
            .map_err(|_| TransportError::Closed)
    }
    async fn recv(&mut self) -> Result<Option<TransportMessage>, TransportError> {
        while let Some(m) = self.0.next().await {
            match m.map_err(|_| TransportError::Closed)? {
                Message::Text(t) => return Ok(Some(TransportMessage::Text(t.to_string()))),
                Message::Binary(b) => return Ok(Some(TransportMessage::Binary(b.to_vec()))),
                Message::Close(_) => return Ok(None),
                _ => {}
            }
        }
        Ok(None)
    }
}
async fn snapshot(client: &Client, channel: &str) -> Result<Value, &'static str> {
    let response: Value = client
        .request("subscribe", json!({"channel":channel}))
        .await
        .map_err(|_| "channel subscription failed")?;
    let snapshot = &response["snapshot"];
    if snapshot["resource"] != channel || !snapshot["state"].is_object() {
        return Err("unexpected channel snapshot");
    }
    Ok(snapshot["state"].clone())
}
async fn run(args: &[String]) -> Result<(), &'static str> {
    if args == ["--help"] || args == ["-h"] {
        println!("{USAGE}");
        return Ok(());
    }
    let mode = args.first().map(String::as_str).unwrap_or("");
    match (mode, args.len()) {
        ("chats" | "models", 2) | ("status", 3) | ("send" | "steer", 5) => {}
        _ => return Err(USAGE),
    }
    let session_uri = &args[1];
    let hosts = code_agent()?
        .args(["ps", "--all", "--json"])
        .output()
        .await
        .map_err(|_| "host discovery failed")?;
    if !hosts.status.success() {
        return Err("host discovery failed");
    }
    let host_pid = session_host(
        &serde_json::from_slice(&hosts.stdout)
            .map_err(|_| "no host inventory; check code agent ps --all --json")?,
        session_uri,
    )?;
    let output = code_agent()?
        .arg("endpoints")
        .output()
        .await
        .map_err(|_| "endpoint discovery failed")?;
    if !output.status.success() {
        return Err("endpoint discovery failed");
    }
    let endpoints: Value = serde_json::from_slice(&output.stdout)
        .map_err(|_| "invalid endpoint discovery response")?;
    let ep = editor_endpoint(&endpoints, host_pid)?;
    if ep["endpoint"]["type"] != "socket" {
        return Err("expected a local socket endpoint");
    }
    let path = ep["endpoint"]["path"]
        .as_str()
        .ok_or("missing endpoint path")?;
    let token = ep["connectionToken"]
        .as_str()
        .ok_or("missing existing token")?;
    if token.is_empty()
        || !token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err("unexpected token encoding");
    }
    let stream = local_stream(path).await?;
    let (ws, _) = tokio_tungstenite::client_async(format!("ws://localhost/?tkn={token}"), stream)
        .await
        .map_err(|_| "authenticated local handshake failed")?;
    let client = Client::connect(LocalWs(ws), ClientConfig::default())
        .await
        .map_err(|_| "client initialization failed")?;
    let init: Value = client.request("initialize", json!({
            "channel":"ahp-root://", "clientId":format!("opus-channel-{}", uuid::Uuid::new_v4()),
            "protocolVersions":["1.0.0"], "initialSubscriptions":[]
        }))
        .await
        .map_err(|_| "protocol handshake failed")?;
    if init["protocolVersion"] != "1.0.0" {
        return Err("host did not negotiate AHP 1.0.0");
    }
    let session = snapshot(&client, session_uri).await?;
    if mode == "models" {
        let provider = session["provider"]
            .as_str()
            .ok_or("missing session provider")?;
        let root = snapshot(&client, "ahp-root://").await?;
        let models = root["agents"]
            .as_array()
            .and_then(|agents| agents.iter().find(|agent| agent["provider"] == provider))
            .and_then(|agent| agent["models"].as_array())
            .ok_or("host did not advertise a model catalog for this session provider")?;
        for model in models {
            println!(
                "{}",
                json!({"provider": provider, "id": model["id"], "name": model["name"]})
            );
        }
        client.shutdown().await;
        return Ok(());
    }
    let chats = session["chats"]
        .as_array()
        .ok_or("session chat catalog missing")?;
    if mode == "chats" {
        for chat in chats {
            println!(
                "{}",
                json!({"resource": chat["resource"], "title": chat["title"]})
            );
        }
        client.shutdown().await;
        return Ok(());
    }
    let chat = args[2].as_str();
    if !chats.iter().any(|entry| entry["resource"] == chat) {
        return Err("selected chat does not belong to the session");
    }
    let state = snapshot(&client, chat).await?;
    if mode == "status" {
        let active = &state["activeTurn"]["responseParts"];
        let active_text = active
            .as_array()
            .into_iter()
            .flatten()
            .filter(|p| p["kind"] == "markdown")
            .filter_map(|p| p["content"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let parts: Vec<_> = active
            .as_array()
            .into_iter()
            .flatten()
            .rev()
            .take(8)
            .map(|p| json!({"kind":p["kind"],"toolStatus":p["toolCall"]["status"]}))
            .collect();
        let last = state["turns"].as_array().and_then(|t| t.last());
        let last_text: Vec<_> = last
            .and_then(|t| t["responseParts"].as_array())
            .into_iter()
            .flatten()
            .filter(|p| p["kind"] == "markdown")
            .filter_map(|p| p["content"].as_str())
            .collect();
        println!(
            "{}",
            json!({
                "protocolVersion": init["protocolVersion"],
                "chat": chat,
                "status": state["status"],
                "activity": state["activity"],
                "turns": state["turns"].as_array().map(Vec::len),
                "activeTurnId": state["activeTurn"]["id"],
                "lastModelId": state["activeTurn"]["message"]["model"]["id"].as_str()
                    .or_else(|| last.and_then(|turn| turn["message"]["model"]["id"].as_str())),
                "activeParts": parts,
                "activeText": active_text.chars().rev().take(2400).collect::<String>().chars().rev().collect::<String>(),
                "lastCompletedText": last_text.join("\n").chars().take(4000).collect::<String>(),
                "queuedMessages": state["queuedMessages"].as_array().map(Vec::len),
                "steeringMessageId": state["steeringMessage"]["id"],
                "queuedMessageIds": state["queuedMessages"].as_array().map(|messages| {
                    messages.iter().map(|message| &message["id"]).collect::<Vec<_>>()
                }),
            })
        );
    } else {
        let path = &args[4];
        let text = std::fs::read_to_string(path).map_err(|_| "cannot read prompt file")?;
        if text.trim().is_empty() || text.len() > 32000 {
            return Err("prompt must contain 1..32000 bytes");
        }
        let id = uuid::Uuid::new_v4().to_string();
        eprintln!("{}", json!({"intentMessageId":id,"chat":chat}));
        let action = pending_action(
            &id,
            &text,
            if mode == "steer" {
                "steering"
            } else {
                "queued"
            },
            &args[3],
        )?;
        let mut subscription = client.attach_subscription(chat).await;
        let handle = client
            .dispatch(chat.into(), action)
            .await
            .map_err(|_| "dispatch failed; reconcile before retry")?;
        let echoed = timeout(Duration::from_secs(20), async {
            while let Some(event) = subscription.recv().await {
                if let ahp::SubscriptionEvent::Action(env) = event {
                    let value = serde_json::to_value(&env).map_err(|_| "invalid acknowledgment")?;
                    if matches_ack(&value, chat, &id, handle.client_seq) {
                        if !value["rejectionReason"].is_null() {
                            return Err("host rejected message");
                        }
                        return Ok(value);
                    }
                }
            }
            Err("channel closed; reconcile before retry")
        })
        .await
        .map_err(|_| "acknowledgment timeout; reconcile before retry")??;
        println!(
            "{}",
            json!({"accepted":true,"messageId":id,"clientSeq":handle.client_seq,"serverSeq":echoed["serverSeq"],"hostKind":echoed["action"]["kind"],"chat":chat})
        );
    }
    client.shutdown().await;
    Ok(())
}

#[tokio::main]
async fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let (channel, rest) = args
        .split_first()
        .map(|(first, rest)| (first.as_str(), rest))
        .unwrap_or(("--help", &[]));
    let args = match channel {
        "--help" | "-h" | "help" => {
            println!(
                "agent-delegation <ahp|claude|google> <command> [arguments]\n\n{USAGE}\nagent-delegation claude --help\nagent-delegation google --help"
            );
            return;
        }
        "claude" | "google" => {
            // Ship the exact provider implementation inside the executable. Provider
            // arguments stay in argv, while Node reads the module itself on stdin.
            let script: &[u8] = match channel {
                "claude" => include_bytes!("../providers/claude/runner.mjs"),
                _ => include_bytes!("../providers/google/runner.mjs"),
            };
            let result = run_node(script, rest);
            std::process::exit(result.unwrap_or_else(|error| {
                eprintln!("Provider runner requires Node.js 22+ on PATH: {error}");
                1
            }));
        }
        "ahp" => rest,
        _ => args.as_slice(), // Preserve the original positional AHP commands.
    };
    match timeout(Duration::from_secs(45), run(args)).await {
        Ok(Ok(())) => {}
        Ok(Err(message)) => {
            eprintln!("{message}");
            std::process::exit(1)
        }
        Err(_) => {
            eprintln!("operation timed out; reconcile before retry");
            std::process::exit(1)
        }
    }
}

fn run_node(script: &[u8], args: &[String]) -> std::io::Result<i32> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("node")
        .args(["--input-type=module", "-"])
        .args(args)
        .stdin(Stdio::piped())
        .spawn()?;
    let sent = child.stdin.take().expect("piped stdin").write_all(script);
    let status = child.wait()?;
    sent?;
    Ok(status.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    const CHAT: &str = "ahp-chat:/a";
    const SESSION: &str = "ahp-session:/s";

    #[test]
    fn endpoint_requires_one_editor_for_selected_pid() {
        let editor = json!({"pid":12, "type":"editor"});
        let endpoints = json!({"endpoints":[{"pid":5,"type":"editor"}, {"pid":12,"type":"standalone"}, editor]});
        assert_eq!(editor_endpoint(&endpoints, 12).unwrap(), &editor);
        assert!(editor_endpoint(&endpoints, 42).is_err());
        assert!(editor_endpoint(&json!({"endpoints":[editor, editor]}), 12).is_err());
    }
    #[test]
    fn host_echo_uses_nested_origin_sequence() {
        let mut value = json!({"channel":CHAT,"action":{"type":"chat/pendingMessageSet","id":"one"},"origin":{"clientSeq":7},"serverSeq":1});
        assert!(matches_ack(&value, CHAT, "one", 7));
        assert!(!matches_ack(&value, CHAT, "two", 7));
        assert!(!matches_ack(&value, CHAT, "one", 8));
        value["channel"] = json!("other");
        assert!(!matches_ack(&value, CHAT, "one", 7));
        value["channel"] = json!(CHAT);
        value["origin"] = Value::Null;
        value["clientSeq"] = json!(7);
        assert!(!matches_ack(&value, CHAT, "one", 7));
    }
    #[test]
    fn host_discovery_uses_the_exact_existing_session() {
        let data = json!([{"host":{"pid":12},"sessions":[{"resource":"other"}]},{"host":{"pid":34},"sessions":[{"resource":SESSION}]}]);
        assert_eq!(session_host(&data, SESSION), Ok(34));
        assert!(session_host(&json!([]), SESSION).is_err());
        assert!(session_host(&json!([{"host":{"pid":1},"sessions":[{"resource":SESSION}]},{"host":{"pid":2},"sessions":[{"resource":SESSION}]}]), SESSION).is_err());
    }
    #[test]
    fn pending_message_preserves_text_and_selected_model() {
        let text = " first  line\n\nsecond\tline\r\n";
        let value =
            serde_json::to_value(pending_action("test-id", text, "queued", "test-model").unwrap())
                .unwrap();
        assert_eq!(value["message"]["text"], text);
        assert_eq!(value["message"]["model"]["id"], "test-model");
        assert_eq!(value["kind"], "queued");
        let steering = serde_json::to_value(
            pending_action("steer-id", text, "steering", "test-model").unwrap(),
        )
        .unwrap();
        assert_eq!(steering["kind"], "steering");
        assert!(pending_action("x", text, "unsupported", "test-model").is_err());
    }
}
