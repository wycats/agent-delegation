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
    // Keep the path short enough for macOS sockaddr_un, including on hosted runners.
    let dir = std::path::Path::new("/tmp").join(format!("opus-{}", uuid::Uuid::new_v4()));
    fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
    let socket = dir.join("host.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let session = "ahp-session:/fixture";
    let chat = "ahp-chat:/fixture";
    let hosts = json!([{"host":{"pid":123}, "sessions":[{"resource":session}]}]);
    let endpoints = json!({"endpoints":[{
        "pid":123, "type":"editor", "connectionToken":"fixture-only",
        "endpoint":{"type":"socket", "path":socket}
    }]});
    fs::write(dir.join("hosts.json"), hosts.to_string()).unwrap();
    fs::write(dir.join("endpoints.json"), endpoints.to_string()).unwrap();
    let cli = dir.join("code");
    fs::write(
        &cli,
        r#"#!/bin/sh
case "$*" in
  "agent ps --all --json") cat "$FIXTURE_DIR/hosts.json" ;;
  "agent endpoints") cat "$FIXTURE_DIR/endpoints.json" ;;
  *) exit 1 ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&cli, fs::Permissions::from_mode(0o700)).unwrap();

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
                json!({"snapshot":{"resource":session,"state":{"chats":[{"resource":chat}]}}}),
            ),
            (
                "subscribe",
                chat,
                json!({"snapshot":{"resource":chat,"state":{"status":1,"turns":[]}}}),
            ),
        ] {
            let msg = ws.next().await.unwrap().unwrap();
            let request: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            assert_eq!(request["method"], method);
            assert_eq!(request["params"]["channel"], channel);
            let reply = json!({"jsonrpc":"2.0","id":request["id"],"result":result});
            ws.send(Message::text(reply.to_string())).await.unwrap();
        }
    });

    let mut paths = vec![dir.clone()];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let output = Command::new(env!("CARGO_BIN_EXE_opus-channel"))
        .args(["status", session, chat])
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
    let status: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(status["chat"], chat);
    assert_eq!(status["status"], 1);
    fs::remove_dir_all(dir).unwrap();
}
