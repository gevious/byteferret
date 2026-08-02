use std::path::Path;

use anyhow::Result;
use serde_json::{json, Value};

use crate::agent::{folder_name, load_context, peer_status, PeerSync, ShareState};
use crate::fsutil::find_files;
use crate::output::{emit, sanitize, say};
use crate::syncthing::process;

/// This machine's hostname (via gethostname), or "" if it can't be read.
fn hostname() -> String {
    let mut buf = [0u8; 256];
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) } == 0;
    if !ok {
        return String::new();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).to_string()
}

fn short(id: &str) -> String {
    id.chars().take(7).collect()
}

/// The token to paste into a `pair`/`unpair` command for a peer: its local alias
/// when one is set — what the user recognises and typed themselves — otherwise a
/// short id prefix. `resolve_peer` accepts either, so both forms work verbatim.
fn peer_ref(config: &crate::config::Config, id: &str) -> String {
    config.alias_for(id).map(str::to_string).unwrap_or_else(|| short(id))
}

/// How a peer's device id appears in the peer list: the full id under `-v`,
/// otherwise just its first segment with an ellipsis.
fn id_display(id: &str, verbose: bool) -> String {
    if verbose {
        id.to_string()
    } else {
        format!("{}…", short(id))
    }
}

/// One registered Syncthing folder, as `status` reports it.
struct FolderInfo {
    id: String,
    label: String,
    path: String,
    /// Device ids the folder is shared with (this device excluded).
    peers: Vec<String>,
    state: String,
    need_bytes: i64,
}

impl FolderInfo {
    /// The user-facing name — the id with its invisible uniqueness suffix removed.
    fn name(&self) -> &str {
        crate::agent::folder_name(&self.id)
    }
}

/// How one (folder, peer) pair renders next to the folder id, e.g.
/// ` (establishing)` or ` (42%)`; empty when fully in sync or offline.
fn share_annotation(state: ShareState, completion: f64) -> String {
    match state {
        ShareState::Sharing => {
            if completion >= 99.5 {
                String::new()
            } else {
                format!(" ({completion:.0}%)")
            }
        }
        ShareState::Establishing => " (establishing)".to_string(),
        ShareState::NotSharingBack => " (peer not sharing back)".to_string(),
        ShareState::Offline => String::new(),
    }
}

/// Per-folder share state between one peer and us: every folder shared with the
/// peer, with the same classification `doctor` uses for the vault. Ordered as
/// (folder id, state, completion).
fn peer_folder_states(
    ctx: &crate::agent::Context,
    folders: &[FolderInfo],
    p: &PeerSync,
) -> Vec<(String, ShareState, f64)> {
    folders
        .iter()
        .filter(|f| f.peers.contains(&p.id))
        .map(|f| {
            if !p.connected {
                return (f.id.clone(), ShareState::Offline, 0.0);
            }
            let (state, completion) = match ctx.client.folder_completion(&f.id, &p.id) {
                Ok(c) => (ShareState::classify(true, &c.remote_state), c.completion),
                Err(_) => (ShareState::Establishing, 0.0),
            };
            (f.id.clone(), state, completion)
        })
        .collect()
}

/// Reports agent + Syncthing health, this machine's identity (hostname, device
/// id), every registered folder and its sync state, connected peers and the
/// folders shared with them, pending pair requests, offered folders, and any
/// conflict files (FR-16, FR-29). This is also where a machine's device id comes
/// from during pairing. Read-only: never starts Syncthing — if it's down, that's
/// the headline.
pub fn status(verbose: bool) -> Result<()> {
    let ctx = load_context()?;
    let host = hostname();
    let key = if ctx.api_key.is_empty() { None } else { Some(ctx.api_key.as_str()) };
    let running = process::is_running(&ctx.paths, &ctx.config.gui_address, key);

    if !running {
        say("agent:      stopped   (run `byteferret start`)");
        if !host.is_empty() {
            say(&format!("hostname:   {host}"));
        }
        emit(&json!({
            "ok": true, "agentRunning": false, "hostname": host,
        }));
        return Ok(());
    }

    let device_id = ctx.client.my_device_id()?;
    let version = ctx.client.version()?;
    let peers = peer_status(&ctx).unwrap_or_default();
    let conns = ctx.client.connections().unwrap_or_default();
    let pending = ctx.client.pending_devices()?;
    let pending_folders = ctx.client.pending_folders()?;

    // Every registered folder — all equal, none privileged.
    let folders: Vec<FolderInfo> = ctx
        .client
        .get_folders()
        .unwrap_or_default()
        .iter()
        .map(|f| {
            let id = f.get("id").and_then(Value::as_str).unwrap_or("").to_string();
            let (state, need_bytes) = ctx
                .client
                .folder_status(&id)
                .map(|s| (s.state, s.need_bytes))
                .unwrap_or_default();
            FolderInfo {
                label: f.get("label").and_then(Value::as_str).unwrap_or("").to_string(),
                path: f.get("path").and_then(Value::as_str).unwrap_or("").to_string(),
                peers: f
                    .get("devices")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|d| d.get("deviceID").and_then(Value::as_str))
                            .filter(|d| *d != device_id)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default(),
                id,
                state,
                need_bytes,
            }
        })
        .collect();

    // Conflict files can appear in any folder, so scan every folder's path.
    let conflicts: Vec<String> = folders
        .iter()
        .filter(|f| !f.path.is_empty())
        .flat_map(|f| find_files(Path::new(&f.path), ".sync-conflict-", 100))
        .map(|p| p.to_string_lossy().to_string())
        .collect();

    // Per-peer folder share states, computed once and used by both the human
    // and the JSON rendering below.
    let shares: Vec<Vec<(String, ShareState, f64)>> =
        peers.iter().map(|p| peer_folder_states(&ctx, &folders, p)).collect();

    // --- human output ---
    say("agent:      running");
    say(&format!("syncthing:  {version}  ({})", ctx.config.gui_address));
    if !host.is_empty() {
        say(&format!("hostname:   {host}"));
    }
    say(&format!("device id:  {device_id}"));
    say("mode:       p2p");
    if folders.is_empty() {
        say("folders:    none — run `byteferret init <path>`");
    } else {
        say("folders:");
        for f in &folders {
            let state = if f.state.is_empty() { "unknown".to_string() } else { f.state.clone() };
            let extra = if f.need_bytes > 0 { format!(" ({} bytes to sync)", f.need_bytes) } else { String::new() };
            say(&format!(
                "  - {}  {}  — {state}{extra}, shared with {} peer(s)",
                sanitize(f.name()),
                sanitize(&f.path),
                f.peers.len(),
            ));
        }
    }
    if peers.is_empty() {
        say("peers:      none — share the device id above and run `byteferret pair --with <id>` on the other machine");
    } else {
        say("peers:");
        for (p, shared) in peers.iter().zip(&shares) {
            let addr = conns
                .get(&p.id)
                .map(|c| c.address.clone())
                .filter(|a| !a.is_empty())
                .map(|a| format!("  {}", sanitize(&a)))
                .unwrap_or_default();
            // Prefer a local alias (trusted) over the peer's self-chosen name.
            let label = ctx.config.alias_for(&p.id).map(str::to_string).unwrap_or_else(|| p.label());
            say(&format!(
                "  - {} ({}): {}{addr}",
                sanitize(&label),
                id_display(&p.id, verbose),
                if p.connected { "connected" } else { "disconnected" },
            ));
            if shared.is_empty() {
                say("      no folders shared");
            } else {
                for (fid, state, completion) in shared {
                    say(&format!(
                        "      {}{}",
                        sanitize(folder_name(fid)),
                        share_annotation(*state, *completion)
                    ));
                }
            }
        }
    }

    // The silent stall: connected, so the list above looks healthy, but the peer
    // never shared back, so nothing will ever transfer.
    if peers.iter().any(PeerSync::stalled) {
        say("");
        say("Note: a connected peer is NOT sharing a folder back — run `byteferret doctor` for the fix.");
    }

    // Requests to pair *with us*. The full device id is printed because that is
    // what `--accept` takes — and because the name beside it is the remote's
    // claim about itself, not something to act on.
    if !pending.is_empty() {
        say(&format!("pending:    {} pairing request(s):", pending.len()));
        for (pid, info) in &pending {
            let name = sanitize(&info.name);
            let named = if name.is_empty() { String::new() } else { format!(" ({name})") };
            say(&format!("  - {pid}{named}"));
            let who = sanitize(&peer_ref(&ctx.config, pid));
            say(&format!("      accept:  byteferret pair {who} --accept"));
            say(&format!("      reject:  byteferret pair {who} --reject"));
        }
    }

    // Folders already-paired peers have offered us but that we have not taken up.
    if !pending_folders.is_empty() {
        say("offered folders:");
        for (fid, pf) in &pending_folders {
            for (did, offer) in &pf.offered_by {
                let label = sanitize(&offer.label);
                let named = if label.is_empty() { String::new() } else { format!(" \"{label}\"") };
                let by = sanitize(&peer_ref(&ctx.config, did));
                say(&format!("  - {}{named} — offered by {}", sanitize(folder_name(fid)), by));
                say(&format!(
                    "      accept:  byteferret pair {} --accept --folder {}",
                    by,
                    sanitize(folder_name(fid))
                ));
            }
        }
    }

    if !conflicts.is_empty() {
        say(&format!("conflicts:  {} sync-conflict file(s) — open both copies, merge, delete the conflict copy:", conflicts.len()));
        // Conflict *filenames* also arrive from a peer — they are whatever the
        // other machine named its copy — so they get the same treatment.
        for f in &conflicts {
            say(&format!("  - {}", sanitize(f)));
        }
    }

    emit(&json!({
        "ok": true,
        "agentRunning": true,
        "hostname": host,
        "deviceId": device_id,
        "syncthingVersion": version,
        "guiAddress": ctx.config.gui_address,
        "mode": "p2p",
        "folders": folders.iter().map(|f| json!({
            "name": f.name(),
            "id": f.id,
            "label": f.label,
            "path": f.path,
            "state": f.state,
            "needBytes": f.need_bytes,
            "peers": f.peers,
        })).collect::<Vec<_>>(),
        "aliases": ctx.config.aliases,
        "peers": peers.iter().zip(&shares).map(|(p, shared)| json!({
            "deviceId": p.id,
            "name": p.name,
            "alias": ctx.config.alias_for(&p.id),
            "connected": p.connected,
            "address": conns.get(&p.id).map(|c| c.address.clone()).unwrap_or_default(),
            "sharing": p.sharing(),
            "shareState": p.share_state().tag(),
            "foldersSynced": shared.iter().map(|(fid, state, completion)| json!({
                "folder": folder_name(fid), "state": state.tag(), "completion": completion,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "pending": pending.iter().map(|(pid, info)| json!({
            "deviceId": pid,
            "name": info.name,
            "address": info.address,
        })).collect::<Vec<_>>(),
        "pendingFolders": pending_folders.iter().flat_map(|(fid, pf)| {
            pf.offered_by.iter().map(move |(did, offer)| json!({
                "name": folder_name(fid),
                "folderId": fid,
                "offeredBy": did,
                "label": offer.label,
                "time": offer.time,
                "receiveEncrypted": offer.receive_encrypted,
                "remoteEncrypted": offer.remote_encrypted,
            }))
        }).collect::<Vec<_>>(),
        "conflicts": conflicts,
    }));
    Ok(())
}
