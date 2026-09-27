//! `byteferret rename <folder> <new-name>` — rename this machine's local label.
//!
//! The Syncthing folder id and path are deliberately unchanged. Syncthing uses
//! the id to identify a shared folder, while `label` is local configuration, so
//! another device can choose a different name for the same share.

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::agent::{ensure_started, folder_name, resolve_folder};
use crate::fsutil::safe_dir_name;
use crate::output::{emit, sanitize, say};

pub fn rename(folder: &str, new_name: &str) -> Result<()> {
    let ctx = ensure_started()?;
    let id = resolve_folder(&ctx.client, folder)?;
    let label = safe_dir_name(new_name)
        .ok_or_else(|| anyhow::anyhow!("invalid folder name '{new_name}'"))?;

    for other in ctx.client.get_folders()? {
        let other_id = other.get("id").and_then(Value::as_str).unwrap_or("");
        let other_label = other.get("label").and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| folder_name(other_id));
        if other_id != id && other_label.eq_ignore_ascii_case(&label) {
            bail!("a folder named '{label}' already exists here");
        }
    }

    let mut config = ctx.client.get_folder(&id)?.ok_or_else(|| anyhow::anyhow!("folder '{id}' disappeared"))?;
    let old_name = config.get("label").and_then(Value::as_str)
        .filter(|s| !s.is_empty()).unwrap_or_else(|| folder_name(&id)).to_string();
    config["label"] = json!(label);
    ctx.client.put_folder(&config)?;

    say(&format!("Renamed '{}' to '{}'", sanitize(&old_name), sanitize(&label)));
    say("  the shared folder id and path are unchanged; other devices are unaffected");
    emit(&json!({
        "ok": true, "action": "rename", "from": old_name, "name": label,
        "folderId": id,
    }));
    Ok(())
}
