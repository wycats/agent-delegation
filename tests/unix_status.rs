#![cfg(unix)]

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
};
use tokio::{net::UnixListener, process::Command};
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
async fn status_reads_the_selected_chat_over_a_local_socket() {
    let state = fixture("status").await;
    assert_eq!(state["chat"], "ahp-chat:/fixture");
    assert_eq!(state["status"], 1);
}

#[tokio::test]
async fn models_lists_only_the_sessions_registered_provider() {
    let model = fixture("models").await;
    assert_eq!(
        model,
        json!({"id":"vendor/fixture", "name":"Fixture model","provider":"custom-host"})
    );
}

#[tokio::test]
async fn ahp_queues_the_exact_registered_model_with_google_installed() {
    assert_eq!(fixture("send").await["accepted"], true);
}

async fn fixture(mode: &'static str) -> Value {
    let list_models = mode == "models";
    // Keep the path short enough for macOS sockaddr_un, including on hosted runners.
    let dir = std::path::Path::new("/tmp").join(format!("opus-{}", uuid::Uuid::new_v4()));
    fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
    let socket = dir.join("host.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let session = "ahp-session:/fixture";
    let chat = "ahp-chat:/fixture";
    let hosts = json!([{"host":{"pid":999}, "sessions":[{"resource":"other"}]}, {"host":{"pid":123}, "sessions":[{"resource":session}]}]);
    let endpoints = json!({"endpoints":[{"pid":999,"type":"editor","endpoint":{"type":"socket","path":"/missing"}},{
        "pid":123, "type":"editor", "connectionToken":"fixture-only",
        "endpoint":{"type":"socket", "path":socket}
    }]});
    fs::write(dir.join("hosts.json"), hosts.to_string()).unwrap();
    fs::write(dir.join("endpoints.json"), endpoints.to_string()).unwrap();
    let cli = dir.join("owning editor");
    let profile = dir.join("profile with spaces");
    fs::create_dir(&profile).unwrap();
    fs::write(
        &cli,
        r#"#!/bin/sh
set -eu
case "$1/$2" in
  agent/ps) test "$#" = 6 && test "$3" = --all && test "$4" = --json && test "$5" = --user-data-dir && test "$6" = "$FIXTURE_DIR/profile with spaces" && cat "$FIXTURE_DIR/hosts.json" ;;
  agent/endpoints) test "$#" = 4 && test "$3" = --user-data-dir && test "$4" = "$FIXTURE_DIR/profile with spaces" && cat "$FIXTURE_DIR/endpoints.json" ;;
  *) exit 1 ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&cli, fs::Permissions::from_mode(0o700)).unwrap();
    let agy = dir.join("agy");
    fs::write(
        &agy,
        "#!/bin/sh\ntouch \"$FIXTURE_DIR/google-called\"\nexit 99\n",
    )
    .unwrap();
    fs::set_permissions(&agy, fs::Permissions::from_mode(0o700)).unwrap();
    let model = std::env::var("AHP_TEST_MODEL_ID")
        .unwrap_or_else(|_| "custom-provider/google/gemini-4-neon".into());
    let requested_model = model.clone();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        for (method, channel, result) in [
            (
                "initialize",
                "ahp-root://",
                json!({"protocolVersion":"1.0.0"}),
            ),
            (
                "subscribe",
                session,
                json!({"snapshot":{"resource":session,"state":{"provider":"custom-host","chats":[{"resource":chat}]}}}),
            ),
            (
                "subscribe",
                if list_models { "ahp-root://" } else { chat },
                if list_models {
                    json!({"snapshot":{"resource":"ahp-root://","state":{"agents":[
                        {"provider":"other","models":[{"id":"wrong"}]},
                        {"provider":"custom-host","models":[{"id":"vendor/fixture","name":"Fixture model"}]}
                    ]}}})
                } else {
                    json!({"snapshot":{"resource":chat,"state":{"status":1,"turns":[]}}})
                },
            ),
        ] {
            let msg = ws.next().await.unwrap().unwrap();
            let request: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            assert_eq!(request["method"], method);
            assert_eq!(request["params"]["channel"], channel);
            let reply = json!({"jsonrpc":"2.0","id":request["id"],"result":result});
            ws.send(Message::text(reply.to_string())).await.unwrap();
        }
        if mode == "send" {
            let msg = ws.next().await.unwrap().unwrap();
            let request: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            assert_eq!(request["method"], "dispatchAction");
            let params = &request["params"];
            assert_eq!(params["channel"], chat);
            let action = &params["action"];
            assert_eq!(action["kind"], "queued");
            assert_eq!(action["message"]["text"], "fixture prompt\n");
            assert_eq!(action["message"]["model"], json!({"id":model}));
            let echo = json!({"jsonrpc":"2.0","method":"action","params":{"action":action,
                "channel":chat,"serverSeq":5,"origin":{"clientId":"fixture","clientSeq":params["clientSeq"]}}});
            ws.send(Message::text(echo.to_string())).await.unwrap();
        }
    });

    let mut paths = vec![dir.clone()];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agent-delegation"));
    cmd.args(["ahp", mode, session])
        .env("AGENT_DELEGATION_CODE", cli)
        .env("AGENT_DELEGATION_USER_DATA_DIR", profile);
    if !list_models {
        cmd.arg(chat);
    }
    if mode == "send" {
        let prompt = dir.join("prompt.txt");
        fs::write(&prompt, "fixture prompt\n").unwrap();
        cmd.arg(requested_model).arg(prompt);
    }
    let output = cmd
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env("FIXTURE_DIR", &dir)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.await.unwrap();
    assert!(!dir.join("google-called").exists());
    fs::remove_dir_all(dir).unwrap();
    serde_json::from_slice(&output.stdout).unwrap()
}
