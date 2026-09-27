//! `byteferret disconnect <device>` — forget a paired device completely.

use std::io::Write;

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::agent::{ensure_started, folder_name, resolve_peer, unshare_folder};
use crate::output::{emit, is_json_mode, sanitize, say};

pub fn disconnect(target: &str, yes: bool) -> Result<()> {
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

    // Collect first: confirmation must happen before any folder is changed.
    let folders: Vec<(String, String)> = ctx.client.get_folders()?.into_iter().filter_map(|folder| {
        let id = folder.get("id").and_then(Value::as_str)?.to_string();
        let shared = folder.get("devices").and_then(Value::as_array)
            .is_some_and(|ds| ds.iter().any(|d| d.get("deviceID").and_then(Value::as_str) == Some(device_id.as_str())));
        if !shared { return None; }
        let name = folder.get("label").and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| folder_name(&id).to_string());
        Some((id, name))
    }).collect();

    let peer_name = candidates.iter().find(|(id, _)| id == &device_id)
        .map(|(_, name)| name.as_str()).filter(|name| !name.is_empty())
        .unwrap_or("device");
    if !yes && !confirm_disconnect(peer_name, &folders)? {
        say("Aborted — nothing changed.");
        emit(&json!({ "ok": true, "action": "disconnect", "disconnected": false }));
        return Ok(());
    }

    for (id, _) in &folders {
        unshare_folder(&ctx.client, id, &device_id)?;
    }
    ctx.client.delete_device(&device_id)?;
    let alias_removed = ctx.config.remove_alias(&device_id);
    if alias_removed { ctx.config.save(&ctx.paths)?; }

    say(&format!("Disconnected {} — removed from {} folder(s)", sanitize(peer_name), folders.len()));
    emit(&json!({
        "ok": true, "action": "disconnect", "disconnected": true, "device": device_id,
        "folders": folders.iter().map(|(_, name)| name).collect::<Vec<_>>(),
        "aliasRemoved": alias_removed,
    }));
    Ok(())
}

fn confirm_disconnect(peer: &str, folders: &[(String, String)]) -> Result<bool> {
    if is_json_mode() {
        bail!("refusing to disconnect '{peer}' without --yes (there is no prompt in --json mode)");
    }
    println!("Disconnect '{}' completely?", sanitize(peer));
    if folders.is_empty() {
        println!("  no folders currently shared with this device");
    } else {
        println!("  the following folders will stop syncing:");
        for (_, name) in folders {
            println!("    - {}", sanitize(name));
        }
    }
    print!("  The paired device will be removed. [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim().chars().next(), Some('y') | Some('Y')))
}
