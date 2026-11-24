use super::{Result, plugin::file_path};
use serde::Deserialize;
use serde_json::Value;
use std::{
    ffi::OsString,
    fs,
    io::Write,
    path::{Path, PathBuf},
};
use tempfile::NamedTempFile;

#[derive(Deserialize, PartialEq)]
pub(super) struct Model {
    pub id: String,
    #[serde(rename = "thinkingLevel")]
    pub thinking: Option<String>,
}

#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(super) struct Target {
    pub cli: PathBuf,
    pub profile: PathBuf,
    pub instance_id: String,
    pub workspace: PathBuf,
    pub model: Model,
    pub plugin_root: Option<PathBuf>,
    pub provider: Option<String>,
    pub max_context_window: Option<u64>,
    // Recognize receipts made by the Mac operator without loading its JS module.
    pub ws_module: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Config {
    #[serde(flatten)]
    pub target: Target,
    #[serde(default)]
    pub required_skills: Vec<String>,
    pub plugin_revision: Option<String>,
    pub plugin_name: Option<String>,
    pub wait_ms: Option<u64>,
}

impl Config {
    pub fn read(path: &Path, receipt: &Value) -> Result<Self> {
        let config: Config =
            serde_json::from_value(read_json(path)?).map_err(|_| "invalid assignment config")?;
        let saved: Target = serde_json::from_value(receipt["target"].clone())
            .map_err(|_| "invalid receipt target")?;
        let t = &config.target;
        if *t != saved {
            return Err("config and receipt identify different targets");
        }
        if !t.cli.is_absolute()
            || !t.profile.is_absolute()
            || !t.workspace.is_absolute()
            || t.instance_id.is_empty()
            || t.model.id.is_empty()
            || t.model.thinking.as_ref().is_some_and(String::is_empty)
        {
            return Err("incomplete assignment target");
        }
        if t.plugin_root.as_ref().is_some_and(|p| !p.is_absolute())
            || (t.plugin_root.is_none() && !config.required_skills.is_empty())
            || (t.plugin_root.is_some()
                && config.plugin_revision.as_deref().unwrap_or("").is_empty())
            || config.required_skills.iter().any(String::is_empty)
        {
            return Err("invalid plugin configuration");
        }
        if !receipt["session"]
            .as_str()
            .is_some_and(|s| s.starts_with("ahp-session:/"))
            || !receipt["chat"]
                .as_str()
                .is_some_and(|s| s.starts_with("ahp-chat:/"))
            || receipt["clientId"].as_str().unwrap_or("").is_empty()
        {
            return Err("missing saved client, session or chat identity");
        }
        for field in ["serverSeq", "clientSeq"] {
            if !receipt[field].is_null() && receipt[field].as_u64().is_none() {
                return Err("invalid saved sequence number");
            }
        }
        if config.wait_ms == Some(0) {
            return Err("waitMs must be positive");
        }
        Ok(config)
    }

    pub fn verify_workspace(&self, directories: &Value) -> Result<()> {
        let dirs = directories.as_array().ok_or("missing workspace")?;
        if dirs.len() != 1 {
            return Err("expected exactly the saved workspace");
        }
        let path = file_path(dirs[0].as_str().ok_or("invalid workspace URI")?)?;
        if path.canonicalize().map_err(|_| "workspace unavailable")?
            != self
                .target
                .workspace
                .canonicalize()
                .map_err(|_| "saved workspace unavailable")?
        {
            return Err("session workspace differs from receipt");
        }
        Ok(())
    }

    pub fn verify_model(&self, root: &Value, provider: &str) -> Result<()> {
        let model = root["agents"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|a| a["provider"] == provider)
            .and_then(|a| a["models"].as_array())
            .into_iter()
            .flatten()
            .find(|m| {
                m["id"] == self.target.model.id
                    && (m["provider"].is_null() || m["provider"] == provider)
            })
            .ok_or("saved model is absent from the session provider catalog")?;
        if let Some(level) = &self.target.model.thinking
            && !model
                .pointer("/configSchema/properties/thinkingLevel/enum")
                .and_then(Value::as_array)
                .is_some_and(|levels| levels.iter().any(|v| v == level))
        {
            return Err("saved reasoning level is unavailable");
        }
        let window =
            self.target
                .max_context_window
                .or(self.target.ws_module.as_ref().map(|_| 1_000_000));
        if window.is_some_and(|window| model["maxContextWindow"] != window) {
            return Err("saved context window differs from advertised capacity");
        }
        Ok(())
    }
}

pub(super) fn read_json(path: &Path) -> Result<Value> {
    serde_json::from_reader(fs::File::open(path).map_err(|_| "cannot open assignment file")?)
        .map_err(|_| "invalid assignment JSON")
}

pub(super) fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name: OsString = path.as_os_str().into();
    name.push(suffix);
    PathBuf::from(name)
}

pub(super) fn save(path: &Path, data: &Value) -> Result<()> {
    let mut file = NamedTempFile::new_in(path.parent().unwrap_or(Path::new(".")))
        .map_err(|_| "cannot create private assignment file")?;
    serde_json::to_writer(&mut file, data).map_err(|_| "cannot write assignment file")?;
    file.write_all(b"\n")
        .and_then(|_| file.as_file().sync_all())
        .map_err(|_| "cannot flush assignment file")?;
    file.persist(path)
        .map_err(|_| "cannot replace assignment file")?;
    Ok(())
}

pub(super) struct ReceiptLock(PathBuf);

impl ReceiptLock {
    pub fn acquire(receipt: &Path) -> Result<Self> {
        let path = sidecar(receipt, ".lock");
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        opts.open(&path)
            .map_err(|_| "cannot acquire receipt lock; inspect its current owner")?;
        Ok(Self(path))
    }
}

impl Drop for ReceiptLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
