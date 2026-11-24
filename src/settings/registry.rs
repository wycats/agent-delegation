use crate::targets::{AhpTarget, Config, Target};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
};
use tempfile::NamedTempFile;
use toml_edit::{DocumentMut, InlineTable, Item, Key, Table, TableLike};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Edit {
    revision: String,
    original: Option<String>,
    name: Option<String>,
    target: Option<Target>,
    #[serde(default)]
    delete: bool,
}

fn directory(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

fn read(path: &Path) -> Result<Option<String>, String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("Cannot read {}: {e}", path.display())),
    }
}

fn revision(source: Option<&str>) -> String {
    source.map_or_else(
        || "absent".into(),
        |s| format!("{:x}", Sha256::digest(s.as_bytes())),
    )
}

fn config(path: &Path, source: Option<&str>) -> Result<Config, String> {
    Config::parse(path, source.unwrap_or("[targets]\n"))
}

fn snapshot(path: &Path, source: Option<&str>) -> Result<Value, String> {
    let mut rows = Vec::new();
    for (name, target) in config(path, source)?.targets {
        let mut row = json!({"name":name, "target":target});
        if let Target::Ahp(AhpTarget::Saved { assignment }) = &target {
            match assignment.chat(directory(path)) {
                Ok(chat) => {
                    row["model"] = json!(chat.model);
                    row["resolved"] = json!(chat);
                }
                Err(e) => row["error"] = json!(e),
            }
        } else {
            row["model"] = row["target"]["model"].clone();
        }
        rows.push(row);
    }
    Ok(json!({"path":path.display().to_string(), "revision":revision(source), "targets":rows}))
}

pub(super) fn list(path: &Path) -> Result<Value, String> {
    snapshot(path, read(path)?.as_deref())
}

/// Reconcile only changed fields, retaining key decoration, comments and the
/// original table/inline-table representation of the operator's TOML.
fn update_fields(table: &mut dyn TableLike, fields: &Map<String, Value>) -> Result<(), String> {
    let removed: Vec<_> = table
        .iter()
        .filter(|(k, _)| !fields.contains_key(*k))
        .map(|(k, _)| k.to_owned())
        .collect();
    for key in removed {
        table.remove(&key);
    }
    for (key, field) in fields {
        if let Some(children) = field.as_object() {
            let item = table
                .entry(key)
                .or_insert_with(|| Item::Value(InlineTable::new().into()));
            let nested = item.as_table_like_mut().ok_or("Expected a table field")?;
            update_fields(nested, children)?;
        } else {
            let text = field.as_str().ok_or("Expected a text field")?;
            if text.trim().is_empty() {
                return Err(format!("{key} must not be blank"));
            }
            let item = table.entry(key).or_insert(Item::None);
            if item.as_str() != Some(text) {
                let mut new = toml_edit::Value::from(text);
                if let Some(old) = item.as_value() {
                    *new.decor_mut() = old.decor().clone();
                }
                *item = Item::Value(new);
            }
        }
    }
    Ok(())
}

// Both TOML table forms append inserted keys. Reinsert the suffix in its
// original order so an inline target keeps its position and surrounding text.
fn rename(table: &mut Item, old: &str, new: &str) {
    let entries = table.as_table_like().unwrap();
    let keys: Vec<Key> = entries
        .iter()
        .skip_while(|(name, _)| *name != old)
        .map(|(name, _)| entries.key(name).unwrap().clone())
        .collect();
    for key in keys {
        let destination = if key.get() == old {
            Key::new(new)
                .with_leaf_decor(key.leaf_decor().clone())
                .with_dotted_decor(key.dotted_decor().clone())
        } else {
            key.clone()
        };
        if let Some(table) = table.as_inline_table_mut() {
            let item = table.remove(key.get()).unwrap();
            table.insert_formatted(&destination, item);
        } else {
            let table = table.as_table_mut().unwrap();
            let item = table.remove(key.get()).unwrap();
            table.insert_formatted(&destination, item);
        }
    }
}

impl Edit {
    fn apply(self, document: &mut DocumentMut) -> Result<(), String> {
        let targets_item = document.get_mut("targets").ok_or("Missing targets table")?;
        let inline = targets_item.is_inline_table();
        let targets = targets_item
            .as_table_like_mut()
            .ok_or("Invalid targets table")?;
        if let Some(original) = &self.original
            && !targets.contains_key(original)
        {
            return Err("Original target does not exist".into());
        }
        if self.delete {
            if self.target.is_some() || self.name.is_some() {
                return Err("Remove takes only the original target name".into());
            }
            targets.remove(&self.original.ok_or("Missing original target name")?);
            if let Some(table) = targets_item.as_table_mut()
                && table.is_empty()
            {
                table.set_implicit(false);
                table.set_dotted(false);
            }
            return Ok(());
        }
        let name = self
            .name
            .as_deref()
            .or(self.original.as_deref())
            .ok_or("Missing target name")?;
        if name.trim().is_empty() || name.starts_with("--") || name.chars().any(char::is_control) {
            return Err("Use a nonempty target name accepted by --target".into());
        }
        if self.original.as_deref() != Some(name) && targets.contains_key(name) {
            return Err("A target already has that name".into());
        }
        if let Some(original) = &self.original {
            if original != name {
                rename(targets_item, original, name);
            }
        } else {
            if self.target.is_none() {
                return Err("A new target needs settings".into());
            }
            let entry = if inline {
                Item::Value(InlineTable::new().into())
            } else {
                Item::Table(Table::new())
            };
            targets.insert(name, entry);
        }
        if let Some(target) = self.target {
            let fields = serde_json::to_value(target).map_err(|e| e.to_string())?;
            let entry = targets_item
                .as_table_like_mut()
                .unwrap()
                .get_mut(name)
                .and_then(Item::as_table_like_mut)
                .ok_or("Invalid target entry")?;
            update_fields(entry, fields.as_object().ok_or("Invalid target fields")?)?;
        }
        Ok(())
    }
}

// Preserve a config symlink while writing the file it points at. Relative
// assignment paths still resolve relative to the selected registry path.
fn write_path(path: &Path) -> Result<PathBuf, String> {
    match fs::canonicalize(path) {
        Ok(resolved) => Ok(resolved),
        Err(e) if e.kind() == ErrorKind::NotFound && fs::symlink_metadata(path).is_err() => {
            Ok(path.to_owned())
        }
        Err(e) => Err(format!("Cannot resolve {}: {e}", path.display())),
    }
}

pub(super) fn save(path: &Path, body: Value) -> Result<Value, String> {
    let edit: Edit =
        serde_json::from_value(body).map_err(|e| format!("Invalid settings edit: {e}"))?;
    let destination = write_path(path)?;
    let parent = directory(&destination);
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    // The config inode changes on replacement. A persistent sidecar lets
    // multiple settings processes synchronize across successive revisions.
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .read(true)
        .open(destination.with_added_extension("settings.lock"))
        .map_err(|e| e.to_string())?;
    lock.lock().map_err(|e| e.to_string())?;
    let source = read(&destination)?;
    let conflict = "conflict: The settings file changed. Reload it before saving.";
    if revision(source.as_deref()) != edit.revision {
        return Err(conflict.into());
    }
    config(path, source.as_deref())?;
    let text = source.as_deref().unwrap_or("[targets]\n");
    let mut document: DocumentMut = text
        .parse()
        .map_err(|e: toml_edit::TomlError| e.message().to_owned())?;
    edit.apply(&mut document)?;
    let mut next = document.to_string();
    if text.contains("\r\n") && !text.replace("\r\n", "").contains('\n') {
        next = next.replace("\r\n", "\n").replace('\n', "\r\n");
    }
    let result = snapshot(path, Some(&next))?;
    let mut file = NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
    if source.is_some() {
        let permissions = fs::metadata(&destination)
            .map_err(|e| e.to_string())?
            .permissions();
        file.as_file()
            .set_permissions(permissions)
            .map_err(|e| e.to_string())?;
    }
    file.write_all(next.as_bytes())
        .and_then(|()| file.as_file().sync_all())
        .map_err(|e| e.to_string())?;
    // Also catch intervening edits from ordinary text editors.
    if read(&destination)? != source {
        return Err(conflict.into());
    }
    if source.is_some() {
        file.persist(&destination).map_err(|e| e.to_string())?;
    } else {
        file.persist_noclobber(&destination).map_err(|e| {
            if e.error.kind() == ErrorKind::AlreadyExists {
                conflict.into()
            } else {
                e.to_string()
            }
        })?;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn provider_edit_preserves_comments_format_and_crlf() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("targets.toml");
        let source = "# machine targets\r\n[targets.fable]\r\nbackend = 'claude'\r\nmodel = 'old' # model ID\r\n\r\n[targets.neon] # another host\r\nbackend = 'google'\r\nmodel = 'old'\r\n";
        fs::write(&path, source).unwrap();
        let rev = list(&path).unwrap()["revision"].clone();
        let updated = save(
            &path,
            json!({"revision":rev,"original":"fable","target":{"backend":"claude","model":"new"}}),
        )
        .unwrap();
        assert_eq!(updated["targets"][0]["model"], "new");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            source.replacen("'old'", "\"new\"", 1)
        );
        assert!(
            save(
                &path,
                json!({"revision":rev,"original":"neon","delete":true})
            )
            .unwrap_err()
            .starts_with("conflict:")
        );
    }

    #[test]
    fn rename_and_remove_last_implicit_entry() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("targets.toml");
        let source = "# source\n[targets.old] # selected\nbackend = 'ahp'\nassignment = { config = 'profiles/one', receipt = 'profiles/two' }\n";
        fs::write(&path, source).unwrap();
        let rev = list(&path).unwrap()["revision"].clone();
        let result = save(&path, json!({"revision":rev,"original":"old","name":"new"})).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            source.replace("[targets.old]", "[targets.new]")
        );
        assert_eq!(
            result["targets"][0]["target"]["assignment"]["config"],
            "profiles/one"
        );
        assert!(result["targets"][0]["error"].is_string());
        let empty = save(
            &path,
            json!({"revision":result["revision"],"original":"new","delete":true}),
        )
        .unwrap();
        assert_eq!(empty["targets"], json!([]));
        assert_eq!(empty, list(&path).unwrap());
    }

    #[test]
    fn inline_registry_and_existing_invalid_file_are_supported_safely() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("targets.toml");
        let source = "targets = {   a = { backend = 'google', model = 'old' } , 'z' = { model = 'z', backend = 'claude' } }\n";
        fs::write(&path, source).unwrap();
        let rev = list(&path).unwrap()["revision"].clone();
        let result = save(&path, json!({"revision":rev,"original":"a","name":"a.b","target":{"backend":"google","model":"new"}})).unwrap();
        assert_eq!(result["targets"][0]["name"], "a.b");
        assert_eq!(result["targets"][0]["model"], "new");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            source
                .replace(" a =", " \"a.b\" =")
                .replace("'old'", "\"new\"")
        );
        let invalid = "nonsense = 1\n";
        fs::write(&path, invalid).unwrap();
        assert!(list(&path).is_err());
        assert!(
            save(
                &path,
                json!({"revision":revision(Some(invalid)),"original":"a","delete":true})
            )
            .is_err()
        );
        assert_eq!(fs::read_to_string(path).unwrap(), invalid);
    }

    #[test]
    fn simultaneous_creates_preserve_the_winner() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("targets.toml");
        let revision = list(&path).unwrap()["revision"].clone();
        let (first, second) = std::thread::scope(|scope| {
            let add = |name| {
                save(
                    &path,
                    json!({"revision":revision, "name":name, "target":{"backend":"claude", "model":"fable"}}),
                )
            };
            let first = scope.spawn(move || add("one"));
            let second = scope.spawn(move || add("two"));
            (first.join().unwrap(), second.join().unwrap())
        });
        let (Ok(winner), Err(error)) = (if first.is_ok() {
            (first, second)
        } else {
            (second, first)
        }) else {
            panic!("Exactly one writer should succeed")
        };
        assert!(error.starts_with("conflict:"));
        assert_eq!(list(&path).unwrap(), winner);
        assert!(dir.path().join("targets.toml").is_file());
    }
}
