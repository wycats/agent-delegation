#![cfg(unix)]

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};
use tempfile::TempDir;
use tokio::{
    net::UnixListener,
    process::Command,
    time::{Duration, timeout},
};
use tokio_tungstenite::tungstenite::Message;
use url::Url;

const SESSION: &str = "ahp-session:/fixture";
const CHAT: &str = "ahp-chat:/fixture";
const MODEL: &str = "vscode-vercel/google/gemini-4-neon";
const PROMPT: &str = "  first line\n\nsecond line\n";

struct Fixture {
    _temp: TempDir,
    dir: PathBuf,
    listener: UnixListener,
    plugin_uri: String,
    named: bool,
}

fn write(path: impl AsRef<Path>, value: &Value) {
    fs::write(path, value.to_string()).unwrap();
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new_in("/tmp").unwrap();
        let dir = temp.path().canonicalize().unwrap();
        let socket = dir.join("sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let cli = dir.join("test editor");
        fs::write(
            &cli,
            r#"#!/bin/sh
test -z "${VSCODE_IPC_HOOK_CLI+x}" || exit 1
case "$*" in
  "agent ps --all --json --user-data-dir $FIXTURE") exec /bin/cat "$FIXTURE/hosts" ;;
  "agent endpoints --user-data-dir $FIXTURE") exec /bin/cat "$FIXTURE/endpoints" ;;
  *) exit 1 ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(&cli, fs::Permissions::from_mode(0o700)).unwrap();
        write(
            dir.join("hosts"),
            &json!([{"host":{"pid":12},"sessions":[{"resource":SESSION}]}]),
        );
        write(
            dir.join("endpoints"),
            &json!({"endpoints":[{"type":"editor","pid":12,"instanceId":"editor","connectionToken":"fixture-only","endpoint":{"type":"socket","path":socket}}]}),
        );
        let plugin = dir.join("plugin");
        fs::create_dir(&plugin).unwrap();
        fs::write(plugin.join("SKILL.md"), "skill").unwrap();
        let plugin_uri = Url::from_file_path(&plugin).unwrap().to_string();
        let target = json!({"cli":cli,"profile":dir,"instanceId":"editor","workspace":dir,"pluginRoot":plugin,"provider":"example","model":{"id":MODEL,"thinkingLevel":"high"}});
        let mut config = target.clone();
        config["requiredSkills"] = json!(["review"]);
        config["pluginRevision"] = json!("fixture");
        config["waitMs"] = json!(10);
        write(dir.join("config"), &config);
        write(
            dir.join("receipt"),
            &json!({"clientId":"saved-client","session":SESSION,"chat":CHAT,"clientSeq":8,"serverSeq":9,"target":target,"privateExtras":"preserved"}),
        );
        fs::write(dir.join("prompt"), PROMPT).unwrap();
        Self {
            _temp: temp,
            dir,
            listener,
            plugin_uri,
            named: false,
        }
    }

    fn named() -> Self {
        let mut fixture = Self::new();
        fs::write(fixture.dir.join("targets.toml"), "[targets.work-neon]\nbackend = 'ahp'\nassignment = { config = 'config', receipt = 'receipt' }\n").unwrap();
        fixture.named = true;
        fixture
    }

    fn receipt(&self) -> Value {
        serde_json::from_slice(&fs::read(self.dir.join("receipt")).unwrap()).unwrap()
    }

    fn command(&self, mode: &str) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agent-delegation"));
        if self.named {
            cmd.arg("--config")
                .arg(self.dir.join("targets.toml"))
                .args(["--target", "work-neon", mode]);
        } else {
            cmd.args(["ahp", mode])
                .arg(self.dir.join("config"))
                .arg(self.dir.join("receipt"));
        }
        cmd.env("FIXTURE", &self.dir)
            .env("PATH", "")
            .env("VSCODE_IPC_HOOK_CLI", "wrong-editor")
            .kill_on_drop(true);
        if mode == "submit" {
            cmd.arg(self.dir.join("prompt"));
        }
        cmd
    }

    async fn host(&self, mode: &str, end: End, previous: Option<&str>) {
        let (socket, _) = self.listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
        let workspace = Url::from_file_path(&self.dir).unwrap();
        let mut registered = false;
        let mut started = previous.map(str::to_owned);
        let mut completed = matches!(end, End::AlreadyDone);
        let mut read_skill = false;
        let mut saw_submit = false;
        let (outcome, action_type) = match end {
            End::Cancelled => ("cancelled", "chat/turnCancelled"),
            End::Error => ("error", "chat/error"),
            _ => ("complete", "chat/turnComplete"),
        };
        while let Some(Ok(msg)) = ws.next().await {
            let Ok(body) = msg.to_text() else {
                continue;
            };
            let Ok(m) = serde_json::from_str::<Value>(body) else {
                continue;
            };
            let p = &m["params"];
            let response = match m["method"].as_str() {
                Some("reconnect") => {
                    assert_eq!(p["clientId"], "saved-client");
                    assert_eq!(p["subscriptions"], json!([SESSION, CHAT]));
                    if mode == "inspect" {
                        let request = json!({"jsonrpc":"2.0","id":89,"method":"resourceRead","params":{"uri":format!("{}/SKILL.md", self.plugin_uri)}});
                        ws.send(Message::text(request.to_string())).await.unwrap();
                        let reply = ws.next().await.unwrap().unwrap();
                        let reply: Value = serde_json::from_str(reply.to_text().unwrap()).unwrap();
                        assert_eq!(reply["id"], 89);
                        assert_eq!(reply["result"], json!({"data": "c2tpbGw="}));
                    }
                    ws.send(Message::text(json!({"jsonrpc":"2.0","id":m["id"],"error":{"code":-32008,"message":"expired"}}).to_string())).await.unwrap();
                    continue;
                }
                Some("initialize") => {
                    assert_eq!(p["protocolVersions"], json!(["1.1.0"]));
                    assert_eq!(p["clientId"] == "saved-client", mode != "check");
                    json!({"protocolVersion":"1.1.0","serverSeq":9})
                }
                Some("subscribe") => {
                    let channel = p["channel"].as_str().unwrap();
                    let state = match channel {
                        "ahp-root://" => {
                            let id = if matches!(end, End::WrongModel) {
                                "a-different-model"
                            } else {
                                MODEL
                            };
                            json!({"agents":[{"provider":"example","models":[{"id":id,"provider":"example","configSchema":{"properties":{"thinkingLevel":{"enum":["high"]}}}}]}]})
                        }
                        SESSION => {
                            let skills = if matches!(end, End::MissingSkill) {
                                json!([])
                            } else {
                                json!([{"type":"skill","name":"review"}])
                            };
                            json!({"provider":"example","chats":[{"resource":CHAT}],"workingDirectories":[workspace.as_str()],"customizations":
                                if registered {json!([{"id":self.plugin_uri,"type":"plugin","clientId":"saved-client","load":{"kind":"loaded"},"children":skills}])}else{json!([])}})
                        }
                        CHAT => {
                            let active = if completed {
                                Value::Null
                            } else {
                                started
                                    .as_ref()
                                    .map(|id| json!({"id":id}))
                                    .unwrap_or_default()
                            };
                            json!({"status":1,"workingDirectories":[workspace.as_str()],"activeTurn":active,"turns":if completed && !matches!(end,End::Mismatch) {json!([{"id":started,"state":outcome,"responseParts":[{"kind":"markdown","content":"fixture answer"}]}])}else{json!([])}})
                        }
                        _ => panic!("unexpected channel"),
                    };
                    json!({"snapshot":{"resource":channel,"state":state}})
                }
                Some("dispatchAction") => {
                    let action = &p["action"];
                    match action["type"].as_str().unwrap() {
                        "session/activeClientSet" => {
                            assert_eq!(p["channel"], SESSION);
                            assert_eq!(
                                action["activeClient"]["customizations"][0]["uri"],
                                self.plugin_uri
                            );
                            registered = true;
                            if mode == "watch" && previous.is_some() {
                                ws.send(Message::text(json!({"jsonrpc":"2.0","method":"resourceRead","id":88,"params":{"uri":format!("{}/SKILL.md",self.plugin_uri)}}).to_string())).await.unwrap();
                            }
                        }
                        "chat/turnStarted" => {
                            assert!(registered && mode == "submit" && !saw_submit);
                            assert_eq!(p["channel"], CHAT);
                            assert_eq!(
                                action["message"]["model"],
                                json!({"id":MODEL,"config":{"thinkingLevel":"high"}})
                            );
                            assert_eq!(action["message"]["text"], PROMPT);
                            started = Some(action["turnId"].as_str().unwrap().into());
                            saw_submit = true;
                            assert_eq!(self.receipt()["attempt"]["turnId"], action["turnId"]);
                            assert_eq!(self.receipt()["attempt"]["outcome"], "uncertain");
                            assert_eq!(self.receipt()["clientSeq"], p["clientSeq"]);
                            if matches!(end, End::Disconnect) {
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            ws.send(Message::text(json!({"jsonrpc":"2.0","method":"resourceRead","id":88,"params":{"uri":format!("{}/SKILL.md",self.plugin_uri)}}).to_string())).await.unwrap();
                        }
                        _ => panic!("unexpected action"),
                    }
                    continue;
                }
                None if m["id"] == 88 => {
                    assert_eq!(m["result"], json!({"data":"c2tpbGw="}));
                    read_skill = true;
                    for (seq, (channel, turn)) in (20..).zip([
                        ("ahp-chat:/unrelated", started.as_deref().unwrap()),
                        (CHAT, "unrelated-turn"),
                        (CHAT, started.as_deref().unwrap()),
                    ]) {
                        ws.send(Message::text(json!({"jsonrpc":"2.0","method":"action","params":{"channel":channel,"serverSeq":seq,"action":{"type":action_type,"turnId":turn}}}).to_string())).await.unwrap();
                    }
                    completed = true;
                    continue;
                }
                _ => panic!("unexpected fixture message"),
            };
            ws.send(Message::text(
                json!({"jsonrpc":"2.0","id":m["id"],"result":response}).to_string(),
            ))
            .await
            .unwrap();
            if matches!(end, End::MissingSkill)
                && registered
                && m["method"] == "subscribe"
                && p["channel"] == SESSION
            {
                // Host drops out after confirming that this client's plugin lacks the required skill.
                tokio::time::sleep(Duration::from_millis(30)).await;
                break;
            }
        }
        if matches!(end, End::Complete | End::Mismatch) {
            assert!(read_skill);
        }
        if matches!(end, End::AlreadyDone | End::WrongModel | End::Check) {
            assert!(!registered);
        }
    }

    async fn invoke(&self, mode: &str, end: End, previous: Option<&str>) -> (i32, Vec<Value>) {
        let mut cmd = self.command(mode);
        let (o, ()) = timeout(Duration::from_secs(10), async {
            tokio::join!(cmd.output(), self.host(mode, end, previous))
        })
        .await
        .unwrap();
        let o = o.unwrap();
        let records = String::from_utf8(o.stdout)
            .unwrap()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        (o.status.code().unwrap(), records)
    }
}

#[derive(Clone, Copy)]
enum End {
    Complete,
    Disconnect,
    Mismatch,
    AlreadyDone,
    WrongModel,
    Check,
    MissingSkill,
    Cancelled,
    Error,
}

#[tokio::test]
async fn full_assignment_serves_skills_through_waiting_and_saves_exact_result() {
    let f = Fixture::new();
    let (code, records) = f.invoke("submit", End::Complete, None).await;
    assert_eq!(code, 0);
    assert!(records.iter().any(|r| r["outcome"] == "waiting"));
    let last = records.last().unwrap();
    assert_eq!(last["outcome"], "complete");
    let state: Value =
        serde_json::from_slice(&fs::read(last["transcript"].as_str().unwrap()).unwrap()).unwrap();
    assert_eq!(state["turns"][0]["id"], last["turnId"]);
    assert_eq!(
        state["turns"][0]["responseParts"][0]["content"],
        "fixture answer"
    );
    assert_eq!(f.receipt()["privateExtras"], "preserved");
    assert_eq!(
        fs::metadata(last["transcript"].as_str().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(!f.dir.join("receipt.lock").exists());
}

#[tokio::test]
async fn watch_recovers_the_recorded_turn_after_disconnect_without_another_submit() {
    let f = Fixture::new();
    assert_eq!(f.invoke("submit", End::Disconnect, None).await.0, 2);
    let receipt = f.receipt();
    let turn = receipt["attempt"]["turnId"].as_str().unwrap();
    assert_eq!(receipt["attempt"]["outcome"], "uncertain");
    let (code, records) = f.invoke("watch", End::Complete, Some(turn)).await;
    assert_eq!(code, 0);
    assert_eq!(records.last().unwrap()["turnId"], turn);
    assert_eq!(f.invoke("watch", End::AlreadyDone, Some(turn)).await.0, 0);
}

#[tokio::test]
async fn terminal_notification_needs_a_matching_saved_turn() {
    let f = Fixture::new();
    assert_eq!(f.invoke("submit", End::Mismatch, None).await.0, 2);
    assert_eq!(f.receipt()["attempt"]["outcome"], "uncertain");
}

#[tokio::test]
async fn other_operators_lock_is_respected() {
    let f = Fixture::new();
    fs::write(f.dir.join("receipt.lock"), "existing").unwrap();
    let before = f.receipt();
    assert_eq!(
        f.command("submit").output().await.unwrap().status.code(),
        Some(2)
    );
    assert_eq!(f.receipt(), before);
    assert_eq!(
        fs::read_to_string(f.dir.join("receipt.lock")).unwrap(),
        "existing"
    );
}

#[tokio::test]
async fn missing_model_stops_before_plugin_registration_and_dispatch() {
    let f = Fixture::new();
    assert_eq!(f.invoke("submit", End::WrongModel, None).await.0, 2);
    assert!(f.receipt()["attempt"].is_null());
}

#[tokio::test]
async fn check_uses_a_fresh_client_and_leaves_private_records_untouched() {
    let f = Fixture::new();
    let before = fs::read(f.dir.join("receipt")).unwrap();
    let (code, records) = f.invoke("check", End::Check, None).await;
    assert_eq!(code, 0);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["turnActive"], false);
    assert_eq!(fs::read(f.dir.join("receipt")).unwrap(), before);
    assert!(!f.dir.join("receipt.chat.json").exists());
    assert!(!f.dir.join("receipt.status.json").exists());
}

#[tokio::test]
async fn unavailable_skills_never_start_the_turn() {
    let f = Fixture::new();
    assert_eq!(f.invoke("submit", End::MissingSkill, None).await.0, 2);
    assert!(f.receipt()["attempt"].is_null());
}

#[tokio::test]
async fn unsuccessful_terminal_results_keep_the_hosts_outcome() {
    for (end, outcome) in [(End::Cancelled, "cancelled"), (End::Error, "error")] {
        let f = Fixture::new();
        let (code, records) = f.invoke("submit", end, None).await;
        assert_eq!(code, 0);
        assert_eq!(records.last().unwrap()["outcome"], outcome);
        assert_eq!(f.receipt()["attempt"]["outcome"], outcome);
    }
}

#[tokio::test]
async fn named_target_uses_one_receipt_from_preflight_through_recovery() {
    let f = Fixture::named();
    let receipt = f.receipt();
    assert_eq!(f.invoke("check", End::Check, None).await.0, 0);
    assert_eq!(f.receipt(), receipt);
    assert_eq!(f.invoke("submit", End::Disconnect, None).await.0, 2);
    let turn = f.receipt()["attempt"]["turnId"]
        .as_str()
        .unwrap()
        .to_owned();
    let (code, inspected) = f.invoke("inspect", End::Check, Some(&turn)).await;
    assert_eq!(code, 0);
    assert_eq!(inspected.last().unwrap()["activeTurnId"], turn);
    let (code, watched) = f.invoke("watch", End::Complete, Some(&turn)).await;
    assert_eq!(code, 0);
    assert_eq!(watched.last().unwrap()["turnId"], turn);
    assert_eq!(f.receipt()["attempt"]["outcome"], "complete");
    assert_eq!(f.receipt()["privateExtras"], "preserved");
}
