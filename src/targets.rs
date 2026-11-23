use serde::Deserialize;
use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    targets: BTreeMap<String, Target>,
}

#[derive(Deserialize)]
#[serde(tag = "backend", rename_all = "lowercase", deny_unknown_fields)]
enum Target {
    Ahp(AhpTarget),
    Claude { model: String },
    Google { model: String },
}

#[derive(Deserialize)]
#[serde(untagged, deny_unknown_fields)]
enum AhpTarget {
    Saved { assignment: AssignmentFiles },
    Chat(ChatTarget),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AssignmentFiles {
    config: PathBuf,
    receipt: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ChatTarget {
    pub code: Option<String>,
    pub user_data_dir: Option<String>,
    pub session: String,
    pub chat: String,
    pub model: String,
}

pub struct Editor {
    pub code: String,
    pub profile: Option<String>,
}

#[derive(Default)]
pub struct Invocation {
    pub args: Vec<String>,
    pub editor: Option<Editor>,
    pub native_model: Option<String>,
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

    fn resolve(&self, args: &[String], directory: &Path) -> Result<Invocation, String> {
        let Some((command, rest)) = args.split_first() else {
            return Err("a target needs a command".into());
        };
        let mut call = Invocation {
            args: vec![self.backend().into(), command.clone()],
            ..Default::default()
        };
        if let Self::Ahp(target) = self {
            let saved = match target {
                AhpTarget::Saved { assignment } => Some(assignment),
                AhpTarget::Chat(_) => None,
            };
            if matches!(command.as_str(), "--help" | "-h" | "help") && rest.is_empty() {
                println!(
                    "Named AHP commands: models, chats, status, send <prompt-file>, steer <prompt-file>"
                );
                if saved.is_some() {
                    println!("Assignment commands: check, submit <prompt-file>, watch, inspect");
                }
                call.args.clear();
            } else if matches!(command.as_str(), "check" | "submit" | "watch" | "inspect") {
                let files =
                    saved.ok_or("this AHP target needs an assignment config and receipt")?;
                if rest.len() != usize::from(command == "submit") {
                    return Err(
                        "submit takes one prompt file; check, watch and inspect take no arguments"
                            .into(),
                    );
                }
                for path in [&files.config, &files.receipt] {
                    let path = directory.join(path);
                    call.args.push(
                        path.into_os_string()
                            .into_string()
                            .map_err(|_| "invalid assignment path")?,
                    );
                }
                call.args.extend_from_slice(rest);
            } else {
                match target {
                    AhpTarget::Chat(chat) => chat.route(command, rest, &mut call)?,
                    AhpTarget::Saved { assignment } => assignment
                        .chat(directory)?
                        .route(command, rest, &mut call)?,
                }
            }
        } else {
            let model = self.model(directory)?;
            if model.trim().is_empty() {
                return Err("target model must be nonempty".into());
            }
            call.native_model = Some(model);
            call.args.extend_from_slice(rest);
        }
        Ok(call)
    }
}

impl AssignmentFiles {
    fn chat(&self, directory: &Path) -> Result<ChatTarget, String> {
        crate::assignment::saved_chat(
            &directory.join(&self.config),
            &directory.join(&self.receipt),
        )
        .map_err(str::to_owned)
    }
}

impl ChatTarget {
    fn route(&self, command: &str, rest: &[String], call: &mut Invocation) -> Result<(), String> {
        if self.session.trim().is_empty()
            || self.chat.trim().is_empty()
            || self.model.trim().is_empty()
        {
            return Err("AHP target needs a session, chat and model".into());
        }
        call.editor = Some(Editor {
            code: self.code.clone().unwrap_or_else(|| "code".into()),
            profile: self.user_data_dir.clone(),
        });
        match (command, rest) {
            ("models" | "chats", []) => call.args.push(self.session.clone()),
            ("status", []) => call.args.extend([self.session.clone(), self.chat.clone()]),
            ("send" | "steer", [prompt]) => call.args.extend([
                self.session.clone(),
                self.chat.clone(),
                self.model.clone(),
                prompt.clone(),
            ]),
            _ => return Err("unsupported named AHP command or arguments; use --help".into()),
        }
        Ok(())
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

pub fn invocation(args: Vec<String>) -> Result<Option<Invocation>, String> {
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
            return Err(format!("supply {option} once with a value"));
        }
        rest = remaining;
    }
    if name.is_none() && rest != ["targets"] {
        if config.is_some() || rest.first().is_some_and(|arg| arg == "targets") {
            return Err(
                "use [--config PATH] targets or [--config PATH] --target NAME <command>".into(),
            );
        }
        return Ok(Some(Invocation {
            args,
            ..Default::default()
        }));
    }
    let path = config_path(config)?;
    let source =
        fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let config: Config = toml::from_str(&source)
        .map_err(|e: toml::de::Error| format!("invalid {}: {}", path.display(), e.message()))?;
    let directory = path.parent().unwrap_or(Path::new("."));
    if let Some(name) = name {
        let target = config
            .targets
            .get(name)
            .ok_or_else(|| format!("unknown target {name:?} in {}", path.display()))?;
        let call = target.resolve(rest, directory)?;
        return Ok((!call.args.is_empty()).then_some(call));
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
