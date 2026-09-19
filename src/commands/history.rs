//! `byteferret history <folder> [file]` and `byteferret restore <folder> <file>`
//! — browse and roll back the versions Syncthing keeps in each folder's
//! `.stversions/`.
//!
//! A caveat worth stating loudly, because it shapes what you will see: Syncthing
//! only archives a file when *it* overwrites or deletes it to apply a change
//! received from a paired device. Edits you make locally on this machine are
//! never versioned here — their previous copy is archived on the *peer* that
//! pulls them. So `history` on this machine shows changes that arrived *from*
//! your other devices, not your own local iterations, and it is empty until a
//! peer change lands. It is also forward-only: nothing before versioning was
//! enabled exists to recover.
//!
//! Both commands go through Syncthing's `/rest/folder/versions` API rather than
//! reading `.stversions/` directly, so restoring archives the current file first
//! (the restore is itself reversible) and the folder index stays consistent.

use std::io::Write;

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::agent::{ensure_started, folder_name, resolve_folder, Context};
use crate::output::{emit, is_json_mode, sanitize, say};

/// One archived version of a file.
struct Version {
    /// RFC3339 tag Syncthing uses to identify the version on restore.
    tag: String,
    size: i64,
}

/// `byteferret history <folder> [file]` — list archived versions.
pub fn history(folder: &str, file: Option<&str>) -> Result<()> {
    let ctx = ensure_started()?;
    let id = resolve_folder(&ctx.client, folder)?;
    let name = folder_name(&id).to_string();
    let files = load_versions(&ctx, &id)?;

    // Narrow to one file when asked; otherwise show the whole folder.
    let shown: Vec<(&String, &Vec<Version>)> = match file {
        Some(f) => {
            let (key, versions) = find_file(&files, f)?;
            vec![(key, versions)]
        }
        None => files.iter().collect(),
    };

    if shown.is_empty() {
        say(&format!("No history for '{}' yet.", sanitize(&name)));
        say("");
        say("Versions are recorded only when a change arrives from a paired device — your own");
        say("local edits here are not versioned on this machine. It is also forward-only, so");
        say("nothing from before history was enabled can be recovered.");
        emit(&json!({ "ok": true, "action": "history", "name": name, "folderId": id, "files": [] }));
        return Ok(());
    }

    let total: usize = shown.iter().map(|(_, v)| v.len()).sum();
    say(&format!(
        "{} — {} version(s) across {} file(s):",
        sanitize(&name),
        total,
        shown.len()
    ));
    for (path, versions) in &shown {
        say("");
        say(&format!("  {}", sanitize(path)));
        for v in versions.iter() {
            say(&format!("    {}   {}", display_time(&v.tag), human_size(v.size)));
        }
    }
    say("");
    say(&format!(
        "Restore one with:  byteferret restore {} <file> [--at <time>]",
        sanitize(&name)
    ));

    emit(&json!({
        "ok": true, "action": "history", "name": name, "folderId": id,
        "files": shown.iter().map(|(path, versions)| json!({
            "file": path,
            "versions": versions.iter().map(|v| json!({ "time": v.tag, "size": v.size })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    }));
    Ok(())
}

/// `byteferret restore <folder> <file> [--at <time>]` — bring a version back.
pub fn restore(folder: &str, file: &str, at: Option<&str>, yes: bool) -> Result<()> {
    let ctx = ensure_started()?;
    let id = resolve_folder(&ctx.client, folder)?;
    let name = folder_name(&id).to_string();
    let files = load_versions(&ctx, &id)?;

    let (path, versions) = find_file(&files, file)?;
    let pick = choose_version(path, versions, at)?;

    if !yes && !confirm_restore(path, &pick.tag)? {
        say("Aborted — nothing changed.");
        emit(&json!({ "ok": true, "action": "restore", "restored": false }));
        return Ok(());
    }

    // Syncthing takes a map of file → versionTime and archives the current copy
    // before overwriting, so this restore can itself be rolled back afterwards.
    let picks = json!({ path.as_str(): pick.tag });
    let result = ctx.client.restore_versions(&id, &picks)?;
    if let Some(err) = result.get(path.as_str()).and_then(Value::as_str) {
        if !err.is_empty() {
            bail!("Syncthing could not restore '{}': {}", sanitize(path), sanitize(err));
        }
    }

    say(&format!(
        "Restored '{}' in '{}' to its version from {}.",
        sanitize(path),
        sanitize(&name),
        display_time(&pick.tag)
    ));
    say("  the copy it replaced was archived, so you can restore back to it the same way");
    emit(&json!({
        "ok": true, "action": "restore", "restored": true,
        "name": name, "folderId": id, "file": path, "time": pick.tag,
    }));
    Ok(())
}

/// Pull the folder's versions from Syncthing into `path → [newest … oldest]`.
fn load_versions(ctx: &Context, id: &str) -> Result<std::collections::BTreeMap<String, Vec<Version>>> {
    let raw = ctx.client.folder_versions(id)?;
    let mut out = std::collections::BTreeMap::new();
    let Some(obj) = raw.as_object() else { return Ok(out) };
    for (file, list) in obj {
        let Some(arr) = list.as_array() else { continue };
        let mut versions: Vec<Version> = arr
            .iter()
            .filter_map(|v| {
                let tag = v.get("versionTime").and_then(Value::as_str)?.to_string();
                let size = v.get("size").and_then(Value::as_i64).unwrap_or(0);
                Some(Version { tag, size })
            })
            .collect();
        // Newest first. RFC3339 tags sort chronologically by their leading date,
        // which is all the ordering needs.
        versions.sort_by(|a, b| b.tag.cmp(&a.tag));
        if !versions.is_empty() {
            out.insert(file.clone(), versions);
        }
    }
    Ok(out)
}

/// Resolve a user-typed file reference to one entry in the versions map: an exact
/// path first, else a unique match on the file's base name (so `todo.md` finds
/// `sub/todo.md` when there is only one).
fn find_file<'a>(
    files: &'a std::collections::BTreeMap<String, Vec<Version>>,
    wanted: &str,
) -> Result<(&'a String, &'a Vec<Version>)> {
    let norm = wanted.replace('\\', "/");
    let norm = norm.trim_start_matches("./").trim_start_matches('/');

    if let Some((k, v)) = files.get_key_value(norm) {
        return Ok((k, v));
    }
    let by_base: Vec<(&String, &Vec<Version>)> = files
        .iter()
        .filter(|(k, _)| k.rsplit('/').next() == Some(norm))
        .collect();
    match by_base.as_slice() {
        [(k, v)] => Ok((k, v)),
        [] => bail!(
            "no history for '{}' in this folder.{}",
            sanitize(wanted),
            list_hint(files)
        ),
        many => {
            let paths: String = many.iter().map(|(k, _)| format!("\n  {}", sanitize(k))).collect();
            bail!(
                "'{}' matches several files — name the path exactly:{}",
                sanitize(wanted),
                paths
            )
        }
    }
}

/// Pick the version to restore: the one at `--at` (an unambiguous prefix of a
/// listed time, or the raw tag), or the newest when `--at` is omitted.
fn choose_version<'a>(path: &str, versions: &'a [Version], at: Option<&str>) -> Result<&'a Version> {
    let Some(when) = at else {
        // Newest — `versions` is sorted newest-first.
        return versions
            .first()
            .ok_or_else(|| anyhow::anyhow!("'{}' has no archived versions", sanitize(path)));
    };
    let norm = when.trim().replace('T', " ");
    let matches: Vec<&Version> = versions
        .iter()
        .filter(|v| v.tag == when || display_time(&v.tag).starts_with(&norm))
        .collect();
    match matches.as_slice() {
        [one] => Ok(one),
        [] => bail!(
            "no version of '{}' at '{}' — run `byteferret history <folder> {}` to see the times.",
            sanitize(path),
            sanitize(when),
            sanitize(path)
        ),
        many => {
            let times: String = many.iter().map(|v| format!("\n  {}", display_time(&v.tag))).collect();
            bail!("'{}' matches {} versions — be more precise:{}", sanitize(when), many.len(), times)
        }
    }
}

/// Restoring overwrites the file that is there now. Syncthing archives that copy
/// first so it is recoverable, but the change is still real — confirm it. In
/// `--json` mode there is no prompt, so require `--yes` rather than guess.
fn confirm_restore(path: &str, tag: &str) -> Result<bool> {
    if is_json_mode() {
        bail!("refusing to restore '{path}' without --yes (there is no prompt in --json mode)");
    }
    print!(
        "Restore '{}' to its version from {}? The current copy is archived first. [y/N] ",
        sanitize(path),
        display_time(tag)
    );
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim().chars().next(), Some('y') | Some('Y')))
}

/// A hint listing which files have any history, for a "not found" error.
fn list_hint(files: &std::collections::BTreeMap<String, Vec<Version>>) -> String {
    if files.is_empty() {
        return " This folder has no history yet.".to_string();
    }
    let list: String = files.keys().map(|k| format!("\n  {}", sanitize(k))).collect();
    format!(" Files with history:{list}")
}

/// An RFC3339 version tag as a plain local `YYYY-MM-DD HH:MM:SS`. The tag is
/// always `<date>T<time>` before its timezone offset, so the first 19 chars are
/// the wall-clock stamp; anything shorter is shown verbatim.
fn display_time(tag: &str) -> String {
    if tag.len() >= 19 && tag.as_bytes()[10] == b'T' {
        tag[..19].replace('T', " ")
    } else {
        tag.to_string()
    }
}

/// Bytes as a short human string (KB/MB/…), matching how sizes usually read.
fn human_size(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_time_strips_timezone_and_t() {
        assert_eq!(display_time("2026-08-02T14:36:47+02:00"), "2026-08-02 14:36:47");
        assert_eq!(display_time("2026-08-02T14:36:47Z"), "2026-08-02 14:36:47");
        assert_eq!(display_time("weird"), "weird");
    }

    #[test]
    fn human_size_scales_units() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn find_file_matches_exact_then_basename() {
        let mut files = std::collections::BTreeMap::new();
        files.insert("sub/todo.md".to_string(), vec![Version { tag: "t".into(), size: 1 }]);
        files.insert("other.md".to_string(), vec![Version { tag: "t".into(), size: 1 }]);
        assert_eq!(find_file(&files, "sub/todo.md").unwrap().0, "sub/todo.md");
        assert_eq!(find_file(&files, "todo.md").unwrap().0, "sub/todo.md");
        assert_eq!(find_file(&files, "./other.md").unwrap().0, "other.md");
        assert!(find_file(&files, "missing.md").is_err());
    }

    #[test]
    fn choose_version_defaults_to_newest_and_matches_prefix() {
        let versions = vec![
            Version { tag: "2026-08-02T14:36:47+02:00".into(), size: 2 },
            Version { tag: "2026-08-01T09:00:12+02:00".into(), size: 1 },
        ];
        assert_eq!(choose_version("f", &versions, None).unwrap().size, 2);
        assert_eq!(choose_version("f", &versions, Some("2026-08-01")).unwrap().size, 1);
        assert!(choose_version("f", &versions, Some("2026-07")).is_err());
    }
}
