use base64::{Engine, engine::general_purpose::STANDARD};
use cap_std::{ambient_authority, fs::Dir};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::{
    io::Read,
    path::{Path, PathBuf},
};
use url::Url;

pub(super) fn file_path(uri: &str) -> Result<PathBuf, &'static str> {
    let url = Url::parse(uri).map_err(|_| "invalid file URI")?;
    if url.scheme() != "file"
        || url.host().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("expected a local file URI");
    }
    url.to_file_path().map_err(|_| "invalid local file path")
}

pub(super) struct Plugin {
    directory: Dir,
    root: PathBuf,
    pub uri: String,
}

impl Plugin {
    pub fn open(path: &Path) -> Result<Self, &'static str> {
        let root = path.canonicalize().map_err(|_| "plugin root unavailable")?;
        let uri = Url::from_file_path(&root)
            .map_err(|_| "invalid plugin root")?
            .to_string();
        let directory = Dir::open_ambient_dir(&root, ambient_authority())
            .map_err(|_| "cannot open plugin root")?;
        Ok(Self {
            root,
            uri,
            directory,
        })
    }

    pub fn resource(&self, method: &str, params: &Value) -> Option<Value> {
        let path = file_path(params["uri"].as_str()?)
            .ok()?
            .canonicalize()
            .ok()?;
        let relative = path.strip_prefix(&self.root).ok()?;
        let relative = if relative.as_os_str().is_empty() {
            Path::new(".")
        } else {
            relative
        };
        // Every operation is relative to the directory handle, including symlink traversal.
        let stat = self.directory.metadata(relative).ok()?;
        if !stat.is_file() && !stat.is_dir() {
            return None;
        }
        match method {
            "resourceRead" if stat.is_file() => {
                const LIMIT: u64 = 8 * 1024 * 1024;
                if stat.len() > LIMIT {
                    return None;
                }
                let mut bytes = Vec::new();
                self.directory
                    .open(relative)
                    .ok()?
                    .take(LIMIT + 1)
                    .read_to_end(&mut bytes)
                    .ok()?;
                (bytes.len() as u64 <= LIMIT).then(|| json!({"data": STANDARD.encode(bytes)}))
            }
            "resourceList" if stat.is_dir() => {
                let mut entries = Vec::new();
                for entry in self.directory.read_dir(relative).ok()? {
                    let entry = entry.ok()?;
                    let ty = entry.file_type().ok()?;
                    if ty.is_dir() || ty.is_file() {
                        entries.push(json!({"name":entry.file_name().to_str()?, "type":if ty.is_dir() {"directory"} else {"file"}}));
                    }
                }
                Some(json!({"entries":entries}))
            }
            "resourceResolve" => {
                let canonical = self.directory.canonicalize(relative).ok()?;
                let modified = DateTime::<Utc>::from(stat.modified().ok()?.into_std());
                let created = stat
                    .created()
                    .ok()
                    .map(|t| DateTime::<Utc>::from(t.into_std()))
                    .unwrap_or(modified);
                Some(
                    json!({"uri":Url::from_file_path(self.root.join(canonical)).ok()?.as_str(),
                    "type":if stat.is_dir() {"directory"} else {"file"},
                    "size":stat.len(), "mtime":modified.to_rfc3339(), "ctime":created.to_rfc3339()}),
                )
            }
            _ => None,
        }
    }

    pub fn manifest_name(&self) -> Option<String> {
        let file = self.directory.open("plugin.json").ok()?;
        let manifest: Value = serde_json::from_reader(file.take(1024 * 1024)).ok()?;
        manifest["name"].as_str().map(str::to_owned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn serves_the_selected_root_and_rejects_other_resources() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap().join("skills");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("two words"), "fixture").unwrap();
        fs::write(root.parent().unwrap().join("adjacent"), "private").unwrap();
        let plugin = Plugin::open(&root).unwrap();
        let params = json!({"uri":format!("{}/two%20words",plugin.uri)});
        assert_eq!(
            plugin.resource("resourceRead", &params).unwrap()["data"],
            "Zml4dHVyZQ=="
        );
        let root_params = json!({"uri":plugin.uri});
        assert_eq!(
            plugin.resource("resourceList", &root_params).unwrap()["entries"][0]["name"],
            "two words"
        );
        assert_eq!(
            plugin.resource("resourceResolve", &params).unwrap()["size"],
            7
        );
        assert!(plugin.resource("resourceWrite", &params).is_none());
        assert!(
            plugin
                .resource(
                    "resourceRead",
                    &json!({"uri":"https://example.invalid/file"})
                )
                .is_none()
        );
        assert!(
            plugin
                .resource(
                    "resourceRead",
                    &json!({"uri":format!("{}/../adjacent",plugin.uri)})
                )
                .is_none()
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("../adjacent", root.join("link")).unwrap();
            assert!(
                plugin
                    .resource(
                        "resourceRead",
                        &json!({"uri":format!("{}/link",plugin.uri)})
                    )
                    .is_none()
            );
        }
    }
}
