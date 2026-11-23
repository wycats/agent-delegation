use serde::Deserialize;
use std::{collections::BTreeMap, env, fs, path::PathBuf};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    targets: BTreeMap<String, Target>,
}

#[derive(Deserialize)]
#[serde(tag = "backend", rename_all = "lowercase", deny_unknown_fields)]
enum Target {
    Ahp {
        code: Option<String>,
        user_data_dir: Option<String>,
        session: String,
        chat: String,
        model: String,
    },
    Claude {
        model: String,
    },
    Google {
        model: String,
    },
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
            Self::Ahp { .. } => "ahp",
            Self::Claude { .. } => "claude",
            Self::Google { .. } => "google",
        }
    }

    fn model(&self) -> &str {
        match self {
            Self::Ahp { model, .. } | Self::Claude { model } | Self::Google { model } => model,
        }
    }

    fn resolve(&self, args: &[String]) -> Result<Invocation, String> {
        let mut call = Invocation::default();
        call.args.push(self.backend().into());
        let Some((command, rest)) = args.split_first() else {
            return Err("a target needs a command".into());
        };
        call.args.push(command.clone());
        if self.model().trim().is_empty() {
            return Err("target model must be nonempty".into());
        }
        if let Self::Ahp {
            code,
            user_data_dir,
            session,
            chat,
            model,
        } = self
        {
            if session.trim().is_empty() || chat.trim().is_empty() {
                return Err("AHP target needs a session and chat".into());
            }
            call.editor = Some(Editor {
                code: code.clone().unwrap_or_else(|| "code".into()),
                profile: user_data_dir.clone(),
            });
            match (command.as_str(), rest) {
                ("models" | "chats", []) => call.args.push(session.clone()),
                ("status", []) => call.args.extend([session.clone(), chat.clone()]),
                ("send" | "steer", [prompt]) => call.args.extend([session.clone(), chat.clone(), model.clone(), prompt.clone()]),
                ("--help" | "-h" | "help", []) => {
                    println!("Named AHP commands: models, chats, status, send <prompt-file>, steer <prompt-file>");
                    call.args.clear();
                }
                _ => return Err("expected models, chats, status, send <prompt-file>, or steer <prompt-file> for an AHP target".into()),
            }
        } else {
            call.native_model = Some(self.model().into());
            call.args.extend_from_slice(rest);
        }
        Ok(call)
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
    if let Some(name) = name {
        let target = config
            .targets
            .get(name)
            .ok_or_else(|| format!("unknown target {name:?} in {}", path.display()))?;
        let call = target.resolve(rest)?;
        return Ok((!call.args.is_empty()).then_some(call));
    }
    for (name, target) in config.targets {
        println!("{}\t{}\t{}", name, target.backend(), target.model());
    }
    Ok(None)
}
