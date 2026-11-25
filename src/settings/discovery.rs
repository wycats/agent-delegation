use crate::{
    assignment, chat_catalog, chat_snapshot, editor_inventory,
    invocation::{AssignmentCommand, ChatAction, ChatCommand, Editor, Invocation, UsageError},
    last_chat_model, model_catalog, session_client, snapshot,
    targets::{AhpTarget, Config, Target},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;
use tokio::time::{Duration, timeout};

const TIME_LIMIT: Duration = Duration::from_secs(45);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Discovery {
    code: Option<String>,
    user_data_dir: Option<String>,
    session: Option<String>,
}

fn choices<'a>(rows: impl Iterator<Item = &'a Value>, id: &str, label: &str) -> Vec<Value> {
    rows.filter_map(|row| {
        let id = row[id].as_str().filter(|v| !v.is_empty())?;
        Some(json!({"id":id, "label":row[label].as_str().filter(|v| !v.is_empty()).unwrap_or(id)}))
    })
    .collect()
}

pub(super) async fn discover(body: Value) -> Result<Value, String> {
    let discovery: Discovery =
        serde_json::from_value(body).map_err(|_| "Invalid discovery request")?;
    let editor = Editor {
        code: discovery
            .code
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "code".to_owned()),
        profile: discovery.user_data_dir.filter(|s| !s.is_empty()),
    };

    timeout(TIME_LIMIT, async {
        if let Some(id) = discovery.session {
            let client = session_client(&editor, &id).await?;
            let session = snapshot(&client, &id).await?;
            let chats = choices(chat_catalog(&session)?.iter(), "resource", "title");
            let provider = session["provider"]
                .as_str()
                .ok_or("missing session provider")?;
            let root = snapshot(&client, "ahp-root://").await?;
            let models = choices(model_catalog(&root, provider)?.iter(), "id", "name");
            client.shutdown().await;
            Ok(json!({"sessions":[], "chats":chats, "models":models}))
        } else {
            let inventory = editor_inventory(&editor, &["ps", "--all", "--json"]).await?;
            let hosts = inventory
                .as_array()
                .ok_or("invalid editor session inventory")?;
            let rows = hosts
                .iter()
                .filter_map(|h| h["sessions"].as_array())
                .flatten();
            Ok(json!({"sessions":choices(rows,"resource","title"), "chats":[], "models":[]}))
        }
    })
    .await
    .map_err(|_| "editor discovery timed out")?
}

pub(super) async fn check(path: &Path, name: &str) -> Result<Value, String> {
    let config = Config::read(path)?;
    let target = config.targets.get(name).ok_or("Unknown target")?;
    let (verb, model) = match target {
        Target::Ahp(AhpTarget::Saved { .. }) => ("check", None),
        Target::Ahp(AhpTarget::Chat(chat)) => ("status", Some(&chat.model)),
        _ => return Err("Check is available for VS Code targets".into()),
    };
    let invocation = target
        .resolve(&[verb.into()], path.parent().unwrap_or(Path::new(".")))
        .map_err(|e| match e {
            UsageError::Arguments(message) => message,
            _ => "Invalid AHP target".to_owned(),
        })?;
    timeout(TIME_LIMIT, async {
        let message = match (invocation, model) {
            (
                Invocation::Assignment {
                    config,
                    receipt,
                    command: AssignmentCommand::Check,
                },
                _,
            ) => {
                let report = assignment::check(&config, &receipt).await?;
                let mut message =
                    format!("Reached {}. Verified model {}", report.chat, report.model);
                if let Some(reasoning) = report.thinking_level {
                    message.push_str(&format!(" with {reasoning} reasoning"));
                }
                message.push('.');
                message
            }
            (
                Invocation::Chat {
                    editor,
                    session,
                    command:
                        ChatCommand::OnChat {
                            chat,
                            action: ChatAction::Status,
                        },
                },
                Some(model),
            ) => {
                let client = session_client(&editor, &session).await?;
                let session = snapshot(&client, &session).await?;
                let state = chat_snapshot(&client, &session, &chat).await?;
                let provider = session["provider"]
                    .as_str()
                    .ok_or("missing session provider")?;
                let root = snapshot(&client, "ahp-root://").await?;
                if !model_catalog(&root, provider)?
                    .iter()
                    .any(|m| m["id"] == model.as_str())
                {
                    return Err(
                        "The configured model is missing from this session provider's catalog"
                            .to_owned(),
                    );
                }
                client.shutdown().await;
                let mut message = format!("Reached {chat}. Model {model} is available.");
                if let Some(last) = last_chat_model(&state) {
                    message.push_str(&format!(" Last turn used {last}."));
                }
                message
            }
            _ => return Err("Expected a read-only AHP invocation".into()),
        };
        Ok(json!({"message":message}))
    })
    .await
    .map_err(|_| "editor check timed out")?
}
