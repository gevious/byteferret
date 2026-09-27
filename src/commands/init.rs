//! `byteferret init <path> [--label <name>]` — bring a directory into sync.
//!
//! Every directory registers as a folder under an auto-generated id of the form
//! `<name>-<6 hex>`. The name is the visible identifier (from `--label` or the
//! directory name); the hex suffix keeps ids globally unique so two machines
//! that independently create a same-named folder never silently merge, and is an
//! implementation detail the user never sees or types. All folders are equal —
//! there is no privileged "vault". Names must be unique on this machine, so a
//! second folder by the same name is refused. Re-running init on an
//! already-registered path updates that folder in place (preserving its id and
//! peers), so init is always safe to repeat.

use std::fs::{self, File};
use std::io::Read;
use std::path::Path;

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::agent::{ensure_started, folder_config, folder_name, Context};
use crate::fsutil::{expand_path, safe_dir_name};
use crate::output::{emit, sanitize, say};

const STIGNORE: &str = "\
// ByteFerret-managed ignore list
.stversions
.stfolder
*~
.*.swp
.DS_Store
";

/// Write a file only if it does not already exist (never clobber user content).
fn write_if_absent(path: &Path, content: &str) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content)?;
    Ok(())
}

/// Reduce a human name (label or directory name) to a folder-id slug.
fn folder_id_slug(name: &str) -> String {
    safe_dir_name(name)
        .map(|s| s.to_lowercase())
        .unwrap_or_else(|| "folder".to_string())
}

/// A short random suffix so independently created folders never collide on id:
/// two machines both initing `~/recipes` must stay two distinct folders until
/// one is deliberately shared and accepted on the other.
fn random_suffix() -> Result<String> {
    let mut buf = [0u8; 3];
    File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf.iter().map(|b| format!("{:02x}", b)).collect())
}

/// A fresh folder id derived from the folder's name, unique on this machine.
fn generate_folder_id(ctx: &Context, name: &str) -> Result<String> {
    let slug = folder_id_slug(name);
    loop {
        let id = format!("{slug}-{}", random_suffix()?);
        if ctx.client.get_folder(&id)?.is_none() {
            return Ok(id);
        }
    }
}

/// Device ids a registered folder is already shared with.
fn folder_peers(folder: &Value) -> Vec<String> {
    folder
        .get("devices")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|d| d.get("deviceID").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

pub fn init(target: &str, existing: bool, label: Option<&str>) -> Result<()> {
    let path = expand_path(target);
    if existing && !path.exists() {
        bail!("--existing given but {} does not exist", path.display());
    }

    let ctx = ensure_started()?;

    fs::create_dir_all(&path)?;
    let abs = fs::canonicalize(&path)?;
    let path_str = abs.to_string_lossy().to_string();
    let dir_name = abs
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "folder".to_string());

    let existing_folders = ctx.client.get_folders()?;

    // Re-init detection is by *path*, so running init twice on the same
    // directory updates the existing registration instead of duplicating it.
    let registered = existing_folders.iter().find(|f| {
        f.get("path")
            .and_then(Value::as_str)
            .map(|p| fs::canonicalize(p).map(|c| c == abs).unwrap_or(p == path_str))
            .unwrap_or(false)
    });

    // The visible, user-facing name: preserve a local rename when re-running
    // init without an explicit --label; otherwise use the requested label or
    // directory name.
    let name = label
        .map(folder_id_slug)
        .or_else(|| registered.and_then(|f| f.get("label")).and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| folder_id_slug(&dir_name));

    // Folder names are unique on this machine, so the invisible suffix never
    // produces two folders a user can't tell apart. A re-init of the same path
    // is exempt (it *is* that folder); any other folder already using the name
    // is a genuine collision.
    let this_id = registered.and_then(|f| f.get("id").and_then(Value::as_str));
    if let Some(clash) = existing_folders.iter().find(|f| {
        let id = f.get("id").and_then(Value::as_str).unwrap_or("");
        Some(id) != this_id && folder_name(id).eq_ignore_ascii_case(&name)
    }) {
        let at = clash.get("path").and_then(Value::as_str).unwrap_or("");
        bail!(
            "a folder named '{name}' already exists here (at {at}). Folder names must be \
             unique — pass --label <other-name>."
        );
    }

    let (folder_id, peers) = if let Some(f) = registered {
        let id = f.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        (id, folder_peers(f))
    } else {
        (generate_folder_id(&ctx, &name)?, vec![])
    };

    // Never plant files in a directory the user owns — only the managed ignore
    // list, and only if absent.
    write_if_absent(&abs.join(".stignore"), STIGNORE)?;

    let folder = folder_config(&folder_id, &name, &path_str, &peers);
    ctx.client.put_folder(&folder)?;

    say(&format!("Folder '{}' ready at {path_str}", sanitize(&name)));
    say(if registered.is_some() {
        "  updated existing registration (peers unchanged)"
    } else {
        "  registered (no files changed)"
    });
    say("");
    say("Share it with a paired machine:");
    say(&format!("  byteferret pair --with <device-or-alias> --folder {}", sanitize(&name)));
    emit(&json!({
        "ok": true,
        "path": path_str,
        "name": name,
        "folderId": folder_id,
        "updated": registered.is_some(),
    }));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::folder_id_slug;

    #[test]
    fn slugs_are_lowercase_and_safe() {
        assert_eq!(folder_id_slug("Recipes"), "recipes");
        assert_eq!(folder_id_slug("Work Notes"), "work-notes");
        // Peer-influenced or odd names reduce to something safe, never empty.
        assert_eq!(folder_id_slug("../../etc"), "etc");
        assert_eq!(folder_id_slug("///"), "folder");
    }
}
