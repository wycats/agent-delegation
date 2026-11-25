//! Attach to a chat on the local VS Code Agent Host.
//! Discovers credentials in memory through VS Code; never writes or prints them.
mod assignment;
mod invocation;
mod settings;
use ahp::{Client, ClientConfig, Transport, TransportError, TransportMessage};
use ahp_types::state::PendingMessageKind;
use invocation::{
    ChatAction, ChatCommand, Editor, HelpTopic, Invocation, ProviderArgs, UsageError,
};
mod targets;
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

const USAGE: &str = "agent-delegation ahp models|chats <session-uri>\nagent-delegation ahp status <session-uri> <chat-uri>\nagent-delegation ahp send|steer <session-uri> <chat-uri> <model-id> <prompt-file>\nagent-delegation ahp check|inspect|watch <config> <receipt>\nagent-delegation ahp submit <config> <receipt> <prompt-file>";

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
    kind: PendingMessageKind,
    model: &str,
) -> Result<ahp_types::actions::StateAction, &'static str> {
    serde_json::from_value(json!({
        "type": "chat/pendingMessageSet", "kind": kind, "id": id,
        "message": {"text": text, "origin": {"kind": "user"}, "model": {"id": model}}
    }))
    .map_err(|_| "invalid typed pending message")
}
fn code_agent(cli: &str, args: &[&str], profile: Option<&str>) -> Result<Command, &'static str> {
    // A compatible distribution can expose the same two discovery commands
    // under a different executable name. No session data goes through a shell.
    #[cfg(windows)]
    {
        let mut command = if cli.to_ascii_lowercase().ends_with(".exe") {
            Command::new(cli)
        } else {
            if cli.is_empty()
                || !cli
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            {
                return Err("use a CLI name on PATH or a full .exe path for AGENT_DELEGATION_CODE");
            }
            let mut cmd = Command::new("cmd.exe");
            cmd.args(["/d", "/v:off", "/c", cli, "agent"]).args(args);
            if let Some(profile) = profile {
                if profile.contains(['"', '\r', '\n']) {
                    return Err("invalid Windows profile path");
                }
                cmd.env("AGENT_DELEGATION_PROFILE_ARG", profile)
                    .arg("--user-data-dir")
                    .raw_arg("\"%AGENT_DELEGATION_PROFILE_ARG%\"");
            }
            cmd.kill_on_drop(true);
            return Ok(cmd);
        };
        command.kill_on_drop(true).arg("agent").args(args);
        if let Some(profile) = profile {
            command.arg("--user-data-dir").arg(profile);
        }
        Ok(command)
    }
    #[cfg(unix)]
    {
        let mut command = Command::new(cli);
        command.kill_on_drop(true).arg("agent").args(args);
        if let Some(profile) = profile {
            command.arg("--user-data-dir").arg(profile);
        }
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

async fn connect_endpoint(ep: &Value) -> Result<WebSocketStream<LocalStream>, &'static str> {
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
    Ok(ws)
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
async fn editor_inventory(editor: &Editor, args: &[&str]) -> Result<Value, &'static str> {
    let output = code_agent(&editor.code, args, editor.profile.as_deref())?
        .env_remove("VSCODE_IPC_HOOK_CLI")
        .output()
        .await
        .map_err(|_| "editor discovery failed")?;
    if !output.status.success() {
        return Err("editor discovery failed");
    }
    serde_json::from_slice(&output.stdout).map_err(|_| "invalid editor inventory")
}

async fn session_client(editor: &Editor, session_uri: &str) -> Result<Client, &'static str> {
    let hosts = editor_inventory(editor, &["ps", "--all", "--json"]).await?;
    let host_pid = session_host(&hosts, session_uri)?;
    let endpoints = editor_inventory(editor, &["endpoints"]).await?;
    let ep = editor_endpoint(&endpoints, host_pid)?;
    let ws = connect_endpoint(ep).await?;
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
    Ok(client)
}

fn chat_catalog(session: &Value) -> Result<&Vec<Value>, &'static str> {
    session["chats"]
        .as_array()
        .ok_or("session chat catalog missing")
}

fn model_catalog<'a>(root: &'a Value, provider: &str) -> Result<&'a Vec<Value>, &'static str> {
    root["agents"]
        .as_array()
        .and_then(|agents| agents.iter().find(|agent| agent["provider"] == provider))
        .and_then(|agent| agent["models"].as_array())
        .ok_or("host did not advertise a model catalog for this session provider")
}

async fn chat_snapshot(
    client: &Client,
    session: &Value,
    chat: &str,
) -> Result<Value, &'static str> {
    if !chat_catalog(session)?
        .iter()
        .any(|entry| entry["resource"] == chat)
    {
        return Err("selected chat does not belong to the session");
    }
    snapshot(client, chat).await
}

fn last_chat_model(state: &Value) -> Option<&str> {
    state["activeTurn"]["message"]["model"]["id"]
        .as_str()
        .or_else(|| state["turns"].as_array()?.last()?["message"]["model"]["id"].as_str())
}

async fn run_chat(
    editor: Editor,
    session_uri: &str,
    command: ChatCommand,
) -> Result<(), &'static str> {
    let client = session_client(&editor, session_uri).await?;
    let session = snapshot(&client, session_uri).await?;
    let (chat, action) = match command {
        ChatCommand::Models => {
            let provider = session["provider"]
                .as_str()
                .ok_or("missing session provider")?;
            let root = snapshot(&client, "ahp-root://").await?;
            let models = model_catalog(&root, provider)?;
            for model in models {
                println!(
                    "{}",
                    json!({"provider": provider, "id": model["id"], "name": model["name"]})
                );
            }
            client.shutdown().await;
            return Ok(());
        }
        ChatCommand::Chats => {
            for chat in chat_catalog(&session)? {
                println!(
                    "{}",
                    json!({"resource": chat["resource"], "title": chat["title"]})
                );
            }
            client.shutdown().await;
            return Ok(());
        }
        ChatCommand::OnChat { chat, action } => (chat, action),
    };
    let chat = chat.as_str();
    let state = chat_snapshot(&client, &session, chat).await?;
    match action {
        ChatAction::Status => {
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
                    "protocolVersion": "1.0.0",
                    "chat": chat,
                    "status": state["status"],
                    "activity": state["activity"],
                    "turns": state["turns"].as_array().map(Vec::len),
                    "activeTurnId": state["activeTurn"]["id"],
                    "lastModelId": last_chat_model(&state),
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
        }
        ChatAction::Message {
            kind,
            model,
            prompt: path,
        } => {
            let text = std::fs::read_to_string(path).map_err(|_| "cannot read prompt file")?;
            if text.trim().is_empty() || text.len() > 32000 {
                return Err("prompt must contain 1..32000 bytes");
            }
            let id = uuid::Uuid::new_v4().to_string();
            eprintln!("{}", json!({"intentMessageId":id,"chat":chat}));
            let action = pending_action(&id, &text, kind, &model)?;
            let mut subscription = client.attach_subscription(chat).await;
            let handle = client
                .dispatch(chat.into(), action)
                .await
                .map_err(|_| "dispatch failed; reconcile before retry")?;
            let echoed = timeout(Duration::from_secs(20), async {
                while let Some(event) = subscription.recv().await {
                    if let ahp::SubscriptionEvent::Action(env) = event {
                        let value =
                            serde_json::to_value(&env).map_err(|_| "invalid acknowledgment")?;
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
    }
    client.shutdown().await;
    Ok(())
}

#[tokio::main]
async fn main() {
    let call = match targets::invocation(std::env::args().skip(1).collect()) {
        Ok(Some(call)) => call,
        Ok(None) => return,
        Err(UsageError::Arguments(error)) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
        Err(UsageError::Chat) => {
            eprintln!("{USAGE}");
            std::process::exit(1);
        }
        Err(UsageError::Assignment) => assignment_error(USAGE),
    };
    match call {
        Invocation::Settings { path, open_browser } => {
            if let Err(error) = settings::run(path, open_browser).await {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
        Invocation::Help(topic) => print_help(topic),
        Invocation::Claude(call) => {
            run_provider(include_bytes!("../providers/claude/runner.mjs"), call)
        }
        Invocation::Google(call) => {
            run_provider(include_bytes!("../providers/google/runner.mjs"), call)
        }
        Invocation::Assignment {
            config,
            receipt,
            command,
        } => {
            if let Err(error) = assignment::run(&config, &receipt, command).await {
                assignment_error(error);
            }
        }
        Invocation::Chat {
            editor,
            session,
            command,
        } => {
            if let Err(error) =
                timeout(Duration::from_secs(45), run_chat(editor, &session, command))
                    .await
                    .unwrap_or(Err("operation timed out; reconcile before retry"))
            {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
    }
}

fn print_help(topic: HelpTopic) {
    match topic {
        HelpTopic::General => println!(
            "agent-delegation [--config PATH] settings [--no-open]\nagent-delegation [--config PATH] targets\nagent-delegation [--config PATH] --target NAME <command> [arguments]\nagent-delegation <ahp|claude|google> <command> [arguments]\n\n{USAGE}\nagent-delegation claude --help\nagent-delegation google --help"
        ),
        HelpTopic::Settings => println!(
            "agent-delegation [--config PATH] settings [--no-open]\nOpen the target editor in your browser. Use --no-open to print the local URL only."
        ),
        HelpTopic::Ahp => println!("{USAGE}"),
        HelpTopic::NamedAhp { assignments } => {
            println!(
                "Named AHP commands: models, chats, status, send <prompt-file>, steer <prompt-file>"
            );
            if assignments {
                println!("Assignment commands: check, submit <prompt-file>, watch, inspect");
            }
        }
    }
}

fn assignment_error(message: &str) -> ! {
    eprintln!("{}", json!({"outcome":"uncertain", "diagnostic":message}));
    std::process::exit(2);
}

fn run_provider(script: &[u8], call: ProviderArgs) -> ! {
    std::process::exit(
        run_node(script, &call.args, call.model.as_deref()).unwrap_or_else(|error| {
            eprintln!("Provider runner requires Node.js 22+ on PATH: {error}");
            1
        }),
    )
}

fn run_node(script: &[u8], args: &[String], model: Option<&str>) -> std::io::Result<i32> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut node = Command::new("node");
    node.env_remove("AGENT_DELEGATION_TARGET_MODEL");
    if let Some(id) = model {
        node.env("AGENT_DELEGATION_TARGET_MODEL", id);
    }
    let mut child = node
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
        let value = serde_json::to_value(
            pending_action("test-id", text, PendingMessageKind::Queued, "test-model").unwrap(),
        )
        .unwrap();
        assert_eq!(value["message"]["text"], text);
        assert_eq!(value["message"]["model"]["id"], "test-model");
        assert_eq!(value["kind"], "queued");
        let steering = serde_json::to_value(
            pending_action("steer-id", text, PendingMessageKind::Steering, "test-model").unwrap(),
        )
        .unwrap();
        assert_eq!(steering["kind"], "steering");
    }
}
