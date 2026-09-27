//! `byteferret disconnect <device>` — forget a paired device completely.

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::agent::{ensure_started, resolve_peer, unshare_folder};
use crate::output::{emit, sanitize, say};

pub fn disconnect(target: &str) -> Result<()> {
    let mut ctx = ensure_started()?;
    let my_id = ctx.client.my_device_id()?;
    let candidates: Vec<(String, String)> = ctx.client.get_devices()?.into_iter().filter_map(|d| {
        let id = d.get("deviceID").and_then(Value::as_str)?.to_string();
        if id == my_id { return None; }
        Some((id, d.get("name").and_then(Value::as_str).unwrap_or("").to_string()))
    }).collect();
    let device_id = resolve_peer(&ctx.config, target, &candidates)?;
    if device_id == my_id {
        bail!("refusing to disconnect this machine");
    }

    let mut folders = Vec::new();
    for folder in ctx.client.get_folders()? {
        let Some(id) = folder.get("id").and_then(Value::as_str) else { continue };
        let shared = folder.get("devices").and_then(Value::as_array)
            .is_some_and(|ds| ds.iter().any(|d| d.get("deviceID").and_then(Value::as_str) == Some(&device_id)));
        if shared {
            unshare_folder(&ctx.client, id, &device_id)?;
            folders.push(id.to_string());
        }
    }
    ctx.client.delete_device(&device_id)?;
    let alias_removed = ctx.config.remove_alias(&device_id);
    if alias_removed { ctx.config.save(&ctx.paths)?; }

    let label = candidates.iter().find(|(id, _)| id == &device_id)
        .map(|(_, name)| name.as_str()).filter(|name| !name.is_empty())
        .unwrap_or("device");
    say(&format!("Disconnected {} — removed from {} folder(s)", sanitize(label), folders.len()));
    emit(&json!({
        "ok": true, "action": "disconnect", "device": device_id,
        "folders": folders, "aliasRemoved": alias_removed,
    }));
    Ok(())
}
