//! Full assignments attach to a saved AHP 1.1 session for the lifetime of its turn.
mod connection;
mod plugin;
mod profile;

use connection::Connection;
use plugin::Plugin;
use profile::{Config, ReceiptLock, read_json, save, sidecar};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
};
use tokio::time::{Duration, Instant, timeout};
use uuid::Uuid;

type Result<T> = std::result::Result<T, &'static str>;
fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn saved_outcome(state: &Value, id: &str) -> Option<&'static str> {
    let turn = state["turns"].as_array()?.iter().find(|t| t["id"] == id)?;
    match turn["state"].as_str()? {
        "complete" => Some("complete"),
        "error" => Some("error"),
        "cancelled" => Some("cancelled"),
        _ => None,
    }
}

fn report_rejected(receipt: &Value) {
    println!(
        "{}",
        json!({
            "outcome": "rejected", "turnId": receipt["attempt"]["turnId"],
            "clientId": receipt["clientId"], "session": receipt["session"], "chat": receipt["chat"]
        })
    );
}

struct Assignment {
    config: Config,
    receipt_path: PathBuf,
    receipt: Value,
    conn: Connection,
}

impl Assignment {
    fn persist(&mut self) -> Result<()> {
        if self.conn.terminal == Some("rejected") {
            self.receipt["attempt"]["outcome"] = "rejected".into();
        }
        self.receipt["serverSeq"] = self.conn.server_seq.into();
        save(&self.receipt_path, &self.receipt)
    }

    fn progress(&self, connected: bool) -> Result<()> {
        let conn = &self.conn;
        save(
            &sidecar(&self.receipt_path, ".status.json"),
            &json!({
                "pid": std::process::id(), "connected": connected,
                "session": conn.session, "chat": conn.chat, "attempt": self.receipt["attempt"],
                "events": conn.events, "lastEvent": conn.last_event,
                "observedTerminal": conn.terminal, "snapshots": conn.snapshots, "updatedAt": now()
            }),
        )
    }

    async fn dispatch(&mut self, channel: &str, action: Value) -> Result<()> {
        let seq = self.receipt["clientSeq"]
            .as_u64()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or("invalid client sequence")?;
        self.receipt["clientSeq"] = seq.into();
        if action["type"] == "chat/turnStarted" {
            self.conn.turn = action["turnId"].as_str().map(str::to_owned);
            self.conn.start_seq = Some(seq);
            self.conn.terminal = None;
            self.receipt["attempt"]["clientSeq"] = seq.into();
        }
        self.persist()?;
        self.conn.dispatch(channel, seq, action).await
    }

    fn skills_loaded(&self, state: &Value, uri: &str) -> bool {
        state["customizations"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|p| {
                p["type"] == "plugin"
                    && p["id"] == uri
                    && p["clientId"] == self.receipt["clientId"]
                    && p["load"]["kind"] == "loaded"
                    && self.config.required_skills.iter().all(|name| {
                        p["children"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .any(|s| s["type"] == "skill" && &s["name"] == name)
                    })
            })
    }

    async fn restore(&mut self, session: &str, provider: &str) -> Result<()> {
        self.config
            .verify_model(&self.conn.snapshot("ahp-root://").await?, provider)?;
        let Some(path) = &self.config.target.plugin_root else {
            return Ok(());
        };
        if self.conn.plugin.is_none() {
            self.conn.plugin = Some(Plugin::open(path)?);
        }
        let plugin = self.conn.plugin.as_ref().ok_or("plugin unavailable")?;
        let uri = plugin.uri.clone();
        let manifest_name = plugin.manifest_name();
        let name = self
            .config
            .plugin_name
            .as_deref()
            .or(manifest_name.as_deref())
            .or_else(|| path.file_name()?.to_str())
            .unwrap_or("skills");
        let action = json!({
            "type": "session/activeClientSet",
            "activeClient": {
                "clientId": self.receipt["clientId"], "displayName": "Codex delegation", "tools": [],
                "customizations": [{
                    "type": "plugin", "id": uri, "uri": uri, "name": name,
                    "enabled": true, "nonce": self.config.plugin_revision
                }]
            }
        });
        self.dispatch(session, action).await?;
        let deadline = Instant::now() + Duration::from_secs(30);
        tokio::time::timeout_at(deadline, async {
            loop {
                let revision = self.conn.session_revision;
                let state = self.conn.snapshot(session).await?;
                if self.skills_loaded(&state, &uri) {
                    return Ok(());
                }
                // Session actions announce catalog changes. Keep serving the
                // plugin even if the file request precedes the next change.
                while self.conn.session_revision == revision {
                    if Instant::now() >= deadline {
                        return Err("required plugin skills did not load");
                    }
                    self.conn.receive_until(deadline).await?;
                }
            }
        })
        .await
        .map_err(|_| "required plugin skills did not load")?
    }

    fn finish(&mut self, turn: &str, outcome: &str, state: &Value) -> Result<()> {
        let path = sidecar(&self.receipt_path, ".chat.json");
        save(&path, state)?;
        self.receipt["attempt"]["outcome"] = outcome.into();
        self.persist()?;
        println!(
            "{}",
            json!({"clientId":self.receipt["clientId"],"session":self.conn.session,"chat":self.conn.chat,
            "outcome":outcome,"turnId":turn,"transcript":path,"snapshots":self.conn.snapshots})
        );
        Ok(())
    }

    async fn wait(&mut self, chat: &str, turn: &str) -> Result<()> {
        let delay = Duration::from_millis(self.config.wait_ms.unwrap_or(240_000));
        let mut waiting = Some(Instant::now().checked_add(delay).ok_or("invalid waitMs")?);
        let mut progress = Instant::now() + Duration::from_secs(5);
        let mut ping = Instant::now() + Duration::from_secs(60);
        let mut events = self.conn.events;
        loop {
            if self.conn.terminal == Some("rejected") {
                return Ok(());
            }
            if let Some(expected) = self.conn.terminal {
                let state = self.conn.snapshot(chat).await?;
                if saved_outcome(&state, turn) != Some(expected) {
                    return Err("terminal event and saved turn disagree");
                }
                return self.finish(turn, expected, &state);
            }
            let now = Instant::now();
            if waiting.is_some_and(|d| d <= now) {
                println!(
                    "{}",
                    json!({"outcome":"waiting","turnId":turn,"session":self.conn.session,"chat":chat})
                );
                waiting = None;
            }
            if now >= progress {
                if self.conn.events != events {
                    self.progress(true)?;
                    events = self.conn.events;
                    ping = now + Duration::from_secs(60);
                }
                progress = now + Duration::from_secs(5);
            }
            if now >= ping {
                self.conn.request("ping", json!({})).await?;
                ping = Instant::now() + Duration::from_secs(60);
            }
            self.conn
                .receive_until(waiting.unwrap_or(progress).min(progress).min(ping))
                .await?;
        }
    }

    async fn operate(&mut self, mode: &str, prompt: Option<String>) -> Result<()> {
        let session = self.conn.session.clone();
        let chat = self.conn.chat.clone();
        let previous = self.receipt["attempt"]["turnId"]
            .as_str()
            .map(str::to_owned);
        let turn = match mode {
            "submit" => Some(Uuid::new_v4().to_string()),
            "watch" => Some(previous.clone().ok_or("no recorded attempt to watch")?),
            _ => None,
        };
        // A resuming host can request a skill as soon as it recognizes its
        // original client ID, before the reconnect response arrives.
        if matches!(mode, "watch" | "inspect")
            && let Some(path) = &self.config.target.plugin_root
        {
            self.conn.plugin = Some(Plugin::open(path)?);
        }
        let read_only = mode == "check";
        let client_id = if read_only {
            Uuid::new_v4().to_string()
        } else {
            self.receipt["clientId"].as_str().unwrap().to_owned()
        };
        self.conn.attach(&client_id, !read_only).await?;
        let session_state = self.conn.snapshot(&session).await?;
        self.config
            .verify_workspace(&session_state["workingDirectories"])?;
        let target = &self.config.target;
        let provider = session_state["provider"]
            .as_str()
            .ok_or("session provider missing")?;
        let expected_provider = target
            .provider
            .as_deref()
            .or(target.ws_module.as_ref().map(|_| "copilotcli"));
        if expected_provider.is_some_and(|p| p != provider) {
            return Err("saved session provider differs");
        }
        let default_chat = session_state["defaultChat"] == chat;
        if !default_chat
            && (target.ws_module.is_some()
                || !session_state["chats"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|c| c["resource"] == chat))
        {
            return Err("saved chat is not available in the session");
        }
        let mut state = self.conn.snapshot(&chat).await?;
        if !state["workingDirectories"].is_null() {
            self.config.verify_workspace(&state["workingDirectories"])?;
        }
        if read_only {
            self.config
                .verify_model(&self.conn.snapshot("ahp-root://").await?, provider)?;
            let model = state["activeTurn"]
                .pointer("/message/model/id")
                .or_else(|| {
                    state["turns"]
                        .as_array()?
                        .last()?
                        .pointer("/message/model/id")
                })
                .and_then(Value::as_str);
            if model.is_some_and(|m| m != target.model.id) {
                return Err("the reported turn model differs from the saved target");
            }
            println!(
                "{}",
                json!({
                    "status": state["status"], "turnActive": !state["activeTurn"].is_null(),
                    "queuedMessages": state["queuedMessages"].as_array().map(Vec::len),
                    "lastModelId": model
                })
            );
            return Ok(());
        }
        if self.conn.terminal == Some("rejected") {
            self.persist()?;
        } else if let Some(ref id) = previous
            && let Some(outcome) = saved_outcome(&state, id)
        {
            self.receipt["attempt"]["outcome"] = outcome.into();
        }
        self.progress(true)?;
        if mode == "inspect" {
            let path = sidecar(&self.receipt_path, ".chat.json");
            save(&path, &state)?;
            self.persist()?;
            let uri = target_plugin_uri(&self.config);
            println!(
                "{}",
                json!({
                    "session": session, "chat": chat, "clientId": client_id,
                    "status": state["status"], "activeTurnId": state["activeTurn"]["id"],
                    "attempt": self.receipt["attempt"],
                    "skillsAvailable": uri.is_some_and(|uri| self.skills_loaded(&session_state, &uri)),
                    "transcript": path, "snapshots": self.conn.snapshots
                })
            );
            return Ok(());
        }
        let turn = turn.ok_or("missing turn")?;
        let can_submit = self.receipt["attempt"].is_null()
            || matches!(
                self.receipt["attempt"]["outcome"].as_str(),
                Some("complete" | "error" | "cancelled" | "rejected")
            );
        if mode == "submit" {
            if !can_submit || !state["activeTurn"].is_null() {
                return Err("a turn is active or a previous attempt is unresolved");
            }
            // The previous attempt is settled. Subsequent events belong to the
            // new assignment only once its own start dispatch is recorded.
            self.conn.turn = None;
            self.conn.start_seq = None;
            self.conn.terminal = None;
        } else {
            if self.conn.terminal == Some("rejected") {
                return Ok(());
            }
            if let Some(outcome) = saved_outcome(&state, &turn) {
                return self.finish(&turn, outcome, &state);
            }
            if state["activeTurn"]["id"] != turn && self.conn.terminal.is_none() {
                return Err(
                    "recorded turn is neither active nor terminal; inspect before continuing",
                );
            }
        }
        self.restore(&session, provider).await?;
        if let Some(prompt) = prompt {
            // Another client may have started work while the host loaded the plugin.
            state = self.conn.snapshot(&chat).await?;
            if !state["activeTurn"].is_null() {
                return Err("another turn started during setup");
            }
            let start = now();
            self.receipt["attempt"] = json!({
                "turnId": turn, "submittedAt": start, "outcome": "uncertain",
                "promptSha256": format!("{:x}", Sha256::digest(prompt.as_bytes()))
            });
            let model = &self.config.target.model;
            let mut wire_model = json!({"id": model.id});
            if let Some(level) = &model.thinking {
                wire_model["config"] = json!({"thinkingLevel": level});
            }
            let action = json!({
                "type": "chat/turnStarted", "turnId": turn, "startedAt": start,
                "message": {
                    "origin": {"kind": "user"}, "text": prompt,
                    "model": wire_model
                }
            });
            self.dispatch(&chat, action).await?;
            self.progress(true)?;
        }
        self.wait(&chat, &turn).await
    }
}

fn target_plugin_uri(config: &Config) -> Option<String> {
    url::Url::from_file_path(config.target.plugin_root.as_ref()?.canonicalize().ok()?)
        .ok()
        .map(String::from)
}

pub(super) async fn run(args: &[String]) -> Result<()> {
    let mode = args[0].as_str();
    if args.len() != if mode == "submit" { 4 } else { 3 } {
        return Err(super::USAGE);
    }
    let prompt = if mode == "submit" {
        let text = fs::read_to_string(&args[3]).map_err(|_| "cannot read UTF-8 prompt file")?;
        if text.trim().is_empty() {
            return Err("empty prompt");
        }
        Some(text)
    } else {
        None
    };
    let path = Path::new(&args[2])
        .canonicalize()
        .map_err(|_| "receipt unavailable")?;
    let _lock = if mode != "check" {
        Some(ReceiptLock::acquire(&path)?)
    } else {
        None
    };
    let receipt = read_json(&path)?;
    let config = Config::read(Path::new(&args[1]), &receipt)?;
    let previous_turn = receipt["attempt"]["turnId"].clone();
    if mode == "watch" && receipt["attempt"]["outcome"] == "rejected" {
        report_rejected(&receipt);
        return Ok(());
    }
    let conn = timeout(Duration::from_secs(30), Connection::open(&config, &receipt))
        .await
        .map_err(|_| "editor connection timed out")??;
    let mut assignment = Assignment {
        config,
        receipt,
        receipt_path: path,
        conn,
    };
    let result = assignment.operate(mode, prompt).await;
    if mode != "check" {
        assignment.persist()?;
        assignment.progress(false)?;
        // The host can disconnect immediately after replaying the rejected
        // dispatch. That proof survives even if a later snapshot failed.
        let watching_this_attempt = mode == "watch"
            || (mode == "submit" && assignment.receipt["attempt"]["turnId"] != previous_turn);
        if watching_this_attempt && assignment.conn.terminal == Some("rejected") {
            report_rejected(&assignment.receipt);
            return Ok(());
        }
    }
    result
}

/// Resolve low-level named chat operations from the same files used for full turns.
/// Assignment commands themselves load the receipt after acquiring its owner lock.
pub(crate) fn saved_chat(
    config_path: &Path,
    receipt_path: &Path,
) -> Result<crate::targets::ChatTarget> {
    let receipt = read_json(receipt_path)?;
    let target = Config::read(config_path, &receipt)?.target;
    Ok(crate::targets::ChatTarget {
        code: Some(
            target
                .cli
                .into_os_string()
                .into_string()
                .map_err(|_| "invalid editor CLI path")?,
        ),
        user_data_dir: Some(
            target
                .profile
                .into_os_string()
                .into_string()
                .map_err(|_| "invalid editor profile path")?,
        ),
        session: receipt["session"]
            .as_str()
            .ok_or("missing session")?
            .to_owned(),
        chat: receipt["chat"].as_str().ok_or("missing chat")?.to_owned(),
        model: target.model.id,
    })
}
