use crate::invocation::{
    AssignmentCommand, ChatAction, ChatCommand, Editor, HelpTopic, Invocation, ProviderArgs,
    UsageError,
};
use ahp_types::state::PendingMessageKind;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub(crate) targets: BTreeMap<String, Target>,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "backend", rename_all = "lowercase", deny_unknown_fields)]
pub(crate) enum Target {
    Ahp(AhpTarget),
    Claude { model: String },
    Google { model: String },
}

#[derive(Deserialize, Serialize)]
#[serde(untagged, deny_unknown_fields)]
pub(crate) enum AhpTarget {
    Saved { assignment: AssignmentFiles },
    Chat(ChatTarget),
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AssignmentFiles {
    pub(crate) config: PathBuf,
    pub(crate) receipt: PathBuf,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ChatTarget {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_data_dir: Option<String>,
    pub session: String,
    pub chat: String,
    pub model: String,
}

impl Config {
    pub(crate) fn read(path: &Path) -> Result<Self, String> {
        let source =
            fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        Self::parse(path, &source)
    }

    pub(crate) fn parse(path: &Path, source: &str) -> Result<Self, String> {
        toml::from_str(source)
            .map_err(|e: toml::de::Error| format!("invalid {}: {}", path.display(), e.message()))
    }
}

impl Target {
    fn backend(&self) -> &'static str {
        match self {
            Self::Ahp(_) => "ahp",
            Self::Claude { .. } => "claude",
            Self::Google { .. } => "google",
        }
    }

    fn model(&self, directory: &Path) -> Result<String, String> {
        match self {
            Self::Ahp(AhpTarget::Saved { assignment }) => Ok(assignment.chat(directory)?.model),
            Self::Ahp(AhpTarget::Chat(chat)) => Ok(chat.model.clone()),
            Self::Claude { model } | Self::Google { model } => Ok(model.clone()),
        }
    }

    pub(crate) fn resolve(
        &self,
        args: &[String],
        directory: &Path,
    ) -> Result<Invocation, UsageError> {
        if args.is_empty() {
            return Err("a target needs a command".into());
        }
        match self {
            Self::Ahp(target) => ahp(args, Some((target, directory))),
            Self::Claude { model } => Ok(Invocation::Claude(provider_args(args, Some(model))?)),
            Self::Google { model } => Ok(Invocation::Google(provider_args(args, Some(model))?)),
        }
    }
}

fn provider_args(args: &[String], model: Option<&String>) -> Result<ProviderArgs, UsageError> {
    if model.is_some_and(|id| id.trim().is_empty()) {
        return Err("target model must be nonempty".into());
    }
    Ok(ProviderArgs {
        args: args.to_vec(),
        model: model.cloned(),
    })
}

impl AssignmentFiles {
    pub(crate) fn chat(&self, directory: &Path) -> Result<ChatTarget, String> {
        crate::assignment::saved_chat(
            &directory.join(&self.config),
            &directory.join(&self.receipt),
        )
        .map_err(str::to_owned)
    }
}

fn ahp(args: &[String], target: Option<(&AhpTarget, &Path)>) -> Result<Invocation, UsageError> {
    let (verb, operands) = args.split_first().ok_or(UsageError::Chat)?;
    if operands.is_empty()
        && (matches!(verb.as_str(), "--help" | "-h") || (target.is_some() && verb == "help"))
    {
        let topic = match target {
            None => HelpTopic::Ahp,
            Some((target, _)) => HelpTopic::NamedAhp {
                assignments: matches!(target, AhpTarget::Saved { .. }),
            },
        };
        return Ok(Invocation::Help(topic));
    }
    if matches!(verb.as_str(), "check" | "inspect" | "watch" | "submit") {
        return assignment_call(verb, operands, target);
    }
    let saved_chat;
    let bound = match target {
        Some((AhpTarget::Chat(chat), _)) => Some(chat),
        Some((AhpTarget::Saved { assignment }, directory)) => {
            saved_chat = assignment.chat(directory)?;
            Some(&saved_chat)
        }
        None => None,
    };
    chat_call(verb, operands, bound)
}

fn assignment_call(
    verb: &str,
    operands: &[String],
    target: Option<(&AhpTarget, &Path)>,
) -> Result<Invocation, UsageError> {
    let (config, receipt, remaining) = match target {
        Some((AhpTarget::Saved { assignment }, directory)) => (
            directory.join(&assignment.config),
            directory.join(&assignment.receipt),
            operands,
        ),
        Some((AhpTarget::Chat(_), _)) => {
            return Err("this AHP target needs an assignment config and receipt".into());
        }
        None => {
            let [config, receipt, tail @ ..] = operands else {
                return Err(UsageError::Assignment);
            };
            (PathBuf::from(config), PathBuf::from(receipt), tail)
        }
    };
    let command = match (verb, remaining) {
        ("check", []) => AssignmentCommand::Check,
        ("inspect", []) => AssignmentCommand::Inspect,
        ("watch", []) => AssignmentCommand::Watch,
        ("submit", [prompt]) => AssignmentCommand::Submit(prompt.into()),
        _ if target.is_some() => {
            return Err(
                "submit takes one prompt file; check, watch and inspect take no arguments".into(),
            );
        }
        _ => return Err(UsageError::Assignment),
    };
    Ok(Invocation::Assignment {
        config,
        receipt,
        command,
    })
}

fn chat_call(
    verb: &str,
    operands: &[String],
    bound: Option<&ChatTarget>,
) -> Result<Invocation, UsageError> {
    if bound.is_some_and(|chat| {
        [&chat.session, &chat.chat, &chat.model]
            .iter()
            .any(|s| s.trim().is_empty())
    }) {
        return Err("AHP target needs a session, chat and model".into());
    }
    let usage = || {
        if bound.is_some() {
            UsageError::from("unsupported named AHP command or arguments; use --help")
        } else {
            UsageError::Chat
        }
    };
    let mut rest = operands.iter();
    // Named commands bind their destination fields from TOML. Direct commands
    // consume the same fields from positional arguments.
    let mut field =
        |value: Option<&String>| value.or_else(|| rest.next()).cloned().ok_or_else(usage);
    let session = field(bound.map(|t| &t.session))?;
    let command = match verb {
        "chats" => ChatCommand::Chats,
        "models" => ChatCommand::Models,
        "status" | "send" | "steer" => {
            let chat = field(bound.map(|t| &t.chat))?;
            let action = if verb == "status" {
                ChatAction::Status
            } else {
                ChatAction::Message {
                    kind: if verb == "steer" {
                        PendingMessageKind::Steering
                    } else {
                        PendingMessageKind::Queued
                    },
                    model: field(bound.map(|t| &t.model))?,
                    prompt: field(None)?.into(),
                }
            };
            ChatCommand::OnChat { chat, action }
        }
        _ => return Err(usage()),
    };
    if rest.next().is_some() {
        return Err(usage());
    }
    let (code, profile) = match bound {
        Some(chat) => (chat.code.clone(), chat.user_data_dir.clone()),
        None => (
            env::var("AGENT_DELEGATION_CODE").ok(),
            env::var("AGENT_DELEGATION_USER_DATA_DIR").ok(),
        ),
    };
    let editor = Editor {
        code: code.unwrap_or_else(|| "code".into()),
        profile,
    };
    Ok(Invocation::Chat {
        editor,
        session,
        command,
    })
}

fn direct(args: &[String]) -> Result<Invocation, UsageError> {
    let Some((first, rest)) = args.split_first() else {
        return Ok(Invocation::Help(HelpTopic::General));
    };
    match first.as_str() {
        "help" | "--help" | "-h" => Ok(Invocation::Help(HelpTopic::General)),
        "claude" => Ok(Invocation::Claude(provider_args(rest, None)?)),
        "google" => Ok(Invocation::Google(provider_args(rest, None)?)),
        "ahp" => ahp(rest, None),
        _ => ahp(args, None), // Original unprefixed AHP commands.
    }
}

fn config_path(explicit: Option<&str>) -> Result<PathBuf, String> {
    if let Some(path) = explicit
        .map(PathBuf::from)
        .or_else(|| env::var_os("AGENT_DELEGATION_CONFIG").map(PathBuf::from))
    {
        return Ok(path);
    }
    #[cfg(windows)]
    let root = env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let root = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| env::var_os("HOME").map(|p| PathBuf::from(p).join(".config")));
    Ok(root
        .ok_or("supply --config PATH to locate targets.toml")?
        .join("agent-delegation")
        .join("targets.toml"))
}

pub fn invocation(args: Vec<String>) -> Result<Option<Invocation>, UsageError> {
    let mut config = None;
    let mut name = None;
    let mut rest = args.as_slice();
    while let Some((option, tail)) = rest.split_first() {
        let slot = match option.as_str() {
            "--config" => &mut config,
            "--target" => &mut name,
            _ => break,
        };
        let (value, remaining) = tail
            .split_first()
            .ok_or_else(|| format!("{option} requires a value"))?;
        if value.trim().is_empty()
            || value.starts_with("--")
            || slot.replace(value.as_str()).is_some()
        {
            return Err(format!("supply {option} once with a value").into());
        }
        rest = remaining;
    }
    if name.is_none() && rest.first().is_some_and(|verb| verb == "settings") {
        let open_browser = match &rest[1..] {
            [] => true,
            [flag] if flag == "--no-open" => false,
            [flag] if flag == "--help" || flag == "-h" => {
                return Ok(Some(Invocation::Help(HelpTopic::Settings)));
            }
            _ => return Err("usage: agent-delegation [--config PATH] settings [--no-open]".into()),
        };
        let path = config_path(config)?;
        return Ok(Some(Invocation::Settings { path, open_browser }));
    }
    if name.is_none() && rest != ["targets"] {
        if config.is_some() || rest.first().is_some_and(|arg| arg == "targets") {
            return Err(
                "use [--config PATH] targets or [--config PATH] --target NAME <command>".into(),
            );
        }
        return direct(rest).map(Some);
    }
    let path = config_path(config)?;
    let config = Config::read(&path)?;
    let directory = path.parent().unwrap_or(Path::new("."));
    if let Some(name) = name {
        let target = config
            .targets
            .get(name)
            .ok_or_else(|| format!("unknown target {name:?} in {}", path.display()))?;
        return target.resolve(rest, directory).map(Some);
    }
    for (name, target) in config.targets {
        println!(
            "{}\t{}\t{}",
            name,
            target.backend(),
            target.model(directory)?
        );
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn named_and_direct_ahp_forms_resolve_to_identical_requests() {
        let registry = Path::new("registry");
        let saved = Target::Ahp(AhpTarget::Saved {
            assignment: AssignmentFiles {
                config: "config.json".into(),
                receipt: "receipt.json".into(),
            },
        });
        let config = registry.join("config.json");
        let receipt = registry.join("receipt.json");
        let named = saved
            .resolve(&argv(&["submit", "task.txt"]), registry)
            .unwrap();
        let operands = argv(&[
            "submit",
            config.to_str().unwrap(),
            receipt.to_str().unwrap(),
            "task.txt",
        ]);
        assert_eq!(direct(&operands).unwrap(), named);
        let mut prefixed = argv(&["ahp"]);
        prefixed.extend(operands);
        assert_eq!(direct(&prefixed).unwrap(), named);

        let chat = Target::Ahp(AhpTarget::Chat(ChatTarget {
            code: env::var("AGENT_DELEGATION_CODE").ok(),
            user_data_dir: env::var("AGENT_DELEGATION_USER_DATA_DIR").ok(),
            session: "session".into(),
            chat: "chat".into(),
            model: "provider/model".into(),
        }));
        for verb in ["send", "steer"] {
            let named = chat.resolve(&argv(&[verb, "task.txt"]), registry).unwrap();
            assert_eq!(
                direct(&argv(&[
                    "ahp",
                    verb,
                    "session",
                    "chat",
                    "provider/model",
                    "task.txt"
                ]))
                .unwrap(),
                named
            );
        }
    }
}
