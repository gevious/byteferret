//! `byteferret rename <folder> <new-name>` — rename a local folder in place.
//!
//! The directory and local Syncthing label change, but the Syncthing folder id
//! remains unchanged. The id is the shared identity, so other devices are not
//! renamed or reconfigured.

use std::fs;
use std::path::PathBuf;

use anyhow::{bail, Context as _, Result};
use serde_json::{json, Value};

use crate::agent::{ensure_started, folder_name, resolve_folder};
use crate::fsutil::safe_dir_name;
use crate::output::{emit, sanitize, say};

pub fn rename(folder: &str, new_name: &str) -> Result<()> {
    let ctx = ensure_started()?;
    let id = resolve_folder(&ctx.client, folder)?;
    let label = safe_dir_name(new_name)
        .ok_or_else(|| anyhow::anyhow!("invalid folder name '{new_name}'"))?;

    let mut config = ctx.client.get_folder(&id)?.ok_or_else(|| anyhow::anyhow!("folder '{id}' disappeared"))?;
    for other in ctx.client.get_folders()? {
        let other_id = other.get("id").and_then(Value::as_str).unwrap_or("");
        let other_label = other.get("label").and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| folder_name(other_id));
        if other_id != id && other_label.eq_ignore_ascii_case(&label) {
            bail!("a folder named '{label}' already exists here");
        }
    }

    let old_path = PathBuf::from(config.get("path").and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("folder '{id}' has no path"))?);
    let parent = old_path.parent().ok_or_else(|| anyhow::anyhow!("folder path has no parent"))?;
    let new_path = parent.join(&label);
    if old_path != new_path && new_path.exists() {
        bail!("cannot rename to '{}': that directory already exists", new_path.display());
    }
    let old_name = config.get("label").and_then(Value::as_str)
        .filter(|s| !s.is_empty()).unwrap_or_else(|| folder_name(&id)).to_string();

    let moved = old_path != new_path;
    if moved {
        fs::rename(&old_path, &new_path)
            .with_context(|| format!("renaming {} to {}", old_path.display(), new_path.display()))?;
    }
    let new_path = fs::canonicalize(&new_path).unwrap_or(new_path);
    config["label"] = json!(label);
    config["path"] = json!(new_path.to_string_lossy().to_string());
    if let Err(error) = ctx.client.put_folder(&config) {
        // Keep the filesystem and Syncthing configuration consistent if the
        // REST update fails after the directory was moved.
        if moved {
            let _ = fs::rename(&new_path, &old_path);
        }
        return Err(error).context("updating the folder path in Syncthing");
    }

    say(&format!("Renamed '{}' to '{}'", sanitize(&old_name), sanitize(&label)));
    say(&format!("  directory moved to {}", sanitize(&new_path.to_string_lossy())));
    say("  the shared folder id is unchanged; other devices are unaffected");
    emit(&json!({
        "ok": true, "action": "rename", "from": old_name, "name": label,
        "folderId": id, "oldPath": old_path, "path": new_path,
    }));
    Ok(())
}
