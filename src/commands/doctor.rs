//! `byteferret doctor` — a one-shot health check over the agent's moving parts
//! (FR-27): the managed Syncthing binary, config + secrets (and their
//! permissions), the running process + REST endpoint, the vault folder, peer
//! connectivity, and any sync-conflict files. With `--fix` it applies the safe,
//! obvious repairs (tighten secrets permissions, start a stopped agent, enable
//! local file history on folders that lack it).
//!
//! Exit status is non-zero when any check FAILs, so `doctor` is usable in
//! scripts and CI as a readiness gate.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::Result;
use serde_json::json;

use crate::agent::{ensure_started, load_context};
use crate::config::Secrets;
use crate::fsutil::find_files;
use crate::output::{emit, sanitize, say};
use crate::paths::{Paths, SYNCTHING_VERSION};
use crate::syncthing::process;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Level {
    Ok,
    Warn,
    Fail,
}

impl Level {
    fn tag(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Warn => "warn",
            Level::Fail => "FAIL",
        }
    }
    fn glyph(self) -> char {
        match self {
            Level::Ok => '✓',
            Level::Warn => '!',
            Level::Fail => '✗',
        }
    }
}

struct Check {
    level: Level,
    name: String,
    detail: String,
    hint: Option<String>,
}

impl Check {
    fn new(level: Level, name: &str, detail: impl Into<String>) -> Check {
        Check {
            level,
            name: name.to_string(),
            detail: detail.into(),
            hint: None,
        }
    }
    fn with_hint(mut self, hint: &str) -> Check {
        self.hint = Some(hint.to_string());
        self
    }
}

/// A secrets file is well-permissioned iff no group/other bits are set (0600).
fn perms_ok(mode: u32) -> bool {
    mode & 0o077 == 0
}

/// The worst level in a set — drives the exit status and headline.
fn worst(levels: impl IntoIterator<Item = Level>) -> Level {
    levels.into_iter().fold(Level::Ok, |acc, l| match (acc, l) {
        (Level::Fail, _) | (_, Level::Fail) => Level::Fail,
        (Level::Warn, _) | (_, Level::Warn) => Level::Warn,
        _ => Level::Ok,
    })
}

/// Apply the safe, obvious repairs and return a human list of what was done.
fn apply_fixes(paths: &Paths) -> Vec<String> {
    let mut done = Vec::new();

    // 1. Tighten secrets permissions to 0600.
    if let Ok(meta) = fs::metadata(&paths.secrets_file) {
        let mode = meta.permissions().mode() & 0o777;
        if !perms_ok(mode)
            && fs::set_permissions(&paths.secrets_file, fs::Permissions::from_mode(0o600)).is_ok()
        {
            done.push(format!(
                "tightened {} to 0600",
                paths.secrets_file.display()
            ));
        }
    }

    // 2. Start the agent if it is not running.
    let key = Secrets::load(paths).ok().and_then(|s| s.syncthing_api_key);
    let cfg = crate::config::Config::load(paths).ok();
    let running = match (&cfg, &key) {
        (Some(c), Some(k)) => process::is_running(paths, &c.gui_address, Some(k)),
        _ => false,
    };
    if !running && ensure_started().is_ok() {
        done.push("started the agent".to_string());
    }

    // 3. Turn on local file history for folders created before it was enabled.
    if let Ok(ctx) = ensure_started() {
        if let Ok(upgraded) = crate::agent::enable_history_on_folders(&ctx.client) {
            for name in upgraded {
                done.push(format!("enabled file history on '{}'", sanitize(&name)));
            }
        }
    }
    done
}

fn build_checks(paths: &Paths) -> Vec<Check> {
    let mut checks = Vec::new();

    // --- Syncthing binary ---
    if paths.syncthing_bin.exists() {
        checks.push(Check::new(
            Level::Ok,
            "syncthing binary",
            format!("present (pinned v{SYNCTHING_VERSION})"),
        ));
    } else {
        checks.push(
            Check::new(Level::Fail, "syncthing binary", "not downloaded yet")
                .with_hint("run `byteferret start` — it downloads the pinned Syncthing on first use"),
        );
    }

    // --- config + secrets ---
    if paths.config_file.exists() {
        checks.push(Check::new(
            Level::Ok,
            "config",
            format!("{}", paths.config_file.display()),
        ));
    } else {
        checks.push(
            Check::new(Level::Warn, "config", "no config.toml yet")
                .with_hint("run `byteferret init <path>` to register a folder"),
        );
    }

    match fs::metadata(&paths.secrets_file) {
        Ok(meta) => {
            let mode = meta.permissions().mode() & 0o777;
            if perms_ok(mode) {
                checks.push(Check::new(Level::Ok, "secrets perms", "0600"));
            } else {
                checks.push(
                    Check::new(
                        Level::Fail,
                        "secrets perms",
                        format!("{:04o} (should be 0600)", mode),
                    )
                    .with_hint("run `byteferret doctor --fix` to correct it"),
                );
            }
        }
        Err(_) => checks.push(Check::new(
            Level::Warn,
            "secrets",
            "not created yet (agent never started)",
        )),
    }

    // --- running process + REST ---
    let Ok(ctx) = load_context() else {
        checks.push(Check::new(
            Level::Fail,
            "state",
            "could not load agent state",
        ));
        return checks;
    };
    let key = if ctx.api_key.is_empty() {
        None
    } else {
        Some(ctx.api_key.as_str())
    };
    let running = process::is_running(&ctx.paths, &ctx.config.gui_address, key);
    if !running {
        checks.push(
            Check::new(Level::Warn, "agent", "stopped")
                .with_hint("run `byteferret start` (or `byteferret doctor --fix`)"),
        );
        return checks; // downstream checks need a live REST endpoint
    }
    checks.push(Check::new(
        Level::Ok,
        "agent",
        format!("running ({})", ctx.config.gui_address),
    ));

    match ctx.client.version() {
        Ok(v) => checks.push(Check::new(
            Level::Ok,
            "syncthing REST",
            format!("reachable (v{v})"),
        )),
        Err(e) => {
            checks.push(Check::new(
                Level::Fail,
                "syncthing REST",
                format!("unreachable: {e}"),
            ));
            return checks;
        }
    }

    // --- folders ---
    let folders = ctx.client.get_folders().unwrap_or_default();
    if folders.is_empty() {
        checks.push(
            Check::new(Level::Warn, "folders", "none registered")
                .with_hint("run `byteferret init <path>` to sync a directory"),
        );
    } else {
        let missing: Vec<String> = folders
            .iter()
            .filter(|f| {
                f.get("path")
                    .and_then(|p| p.as_str())
                    .map(|p| !Path::new(p).is_dir())
                    .unwrap_or(false)
            })
            .filter_map(|f| f.get("id").and_then(|v| v.as_str()).map(|id| crate::agent::folder_name(id).to_string()))
            .collect();
        if missing.is_empty() {
            checks.push(Check::new(
                Level::Ok,
                "folders",
                format!("{} registered", folders.len()),
            ));

            // Folders created before versioning was enabled keep no history.
            let no_history: Vec<String> = folders
                .iter()
                .filter(|f| {
                    f.get("versioning")
                        .and_then(|v| v.get("type"))
                        .and_then(|t| t.as_str())
                        .is_none_or(|t| t.is_empty())
                })
                .filter_map(|f| f.get("id").and_then(|v| v.as_str()).map(|id| crate::agent::folder_name(id).to_string()))
                .collect();
            if no_history.is_empty() {
                checks.push(Check::new(Level::Ok, "file history", "staggered on all folders"));
            } else {
                checks.push(
                    Check::new(
                        Level::Warn,
                        "file history",
                        format!("{} folder(s) keep no local history: {}", no_history.len(), no_history.join(", ")),
                    )
                    .with_hint("run `byteferret doctor --fix` to enable staggered versioning"),
                );
            }
        } else {
            checks.push(
                Check::new(
                    Level::Fail,
                    "folders",
                    format!("{} folder(s) point at a missing directory: {}", missing.len(), missing.join(", ")),
                )
                .with_hint("re-create the directory or run `byteferret init <path> --existing`"),
            );
        }
    }

    // --- peers / connectivity + folder sharing ---
    let peers = crate::agent::peer_status(&ctx).unwrap_or_default();
    if peers.is_empty() {
        checks.push(
            Check::new(Level::Warn, "peers", "none paired")
                .with_hint("get this device's id with `byteferret status`, then `byteferret pair --with <id>` on the other machine"),
        );
    } else {
        let connected = peers.iter().filter(|p| p.connected).count();
        let lvl = if connected > 0 { Level::Ok } else { Level::Warn };
        checks.push(Check::new(
            lvl,
            "peers",
            format!("{connected}/{} connected", peers.len()),
        ));

        // The check that catches the silent stall: a peer is connected, so
        // `peers` looks fine, but it never shared a folder back — so nothing
        // ever syncs. Uses the same `share_state()` classification as
        // `status` so the two commands can never disagree (previously
        // `doctor` ignored the "establishing"/"unknown" state and reported
        // "shared both ways ✓" while the peer listing showed "NOT sharing ✗").
        if connected > 0 {
            use crate::agent::ShareState;
            let by = |s: ShareState| -> Vec<&crate::agent::PeerSync> {
                peers.iter().filter(|p| p.share_state() == s).collect()
            };
            let stalled = by(ShareState::NotSharingBack);
            let establishing = by(ShareState::Establishing);
            // Peer labels are the remote's own claim about itself, so they are
            // sanitized before reaching the terminal.
            let names = |ps: &[&crate::agent::PeerSync]| {
                ps.iter().map(|p| sanitize(&p.label())).collect::<Vec<_>>().join(", ")
            };
            if !stalled.is_empty() {
                checks.push(
                    Check::new(
                        Level::Warn,
                        "folder share",
                        format!(
                            "{} connected peer(s) not sharing a folder back: {}",
                            stalled.len(),
                            names(&stalled)
                        ),
                    )
                    .with_hint(
                        "on that machine run `byteferret pair --with <this-device-id> --folder <name>` \
                         to share the folder back",
                    ),
                );
            } else if !establishing.is_empty() {
                checks.push(
                    Check::new(
                        Level::Warn,
                        "folder share",
                        format!(
                            "{} peer(s) still establishing the share (connected, not yet in sync): {}",
                            establishing.len(),
                            names(&establishing)
                        ),
                    )
                    .with_hint(
                        "give it a moment and re-check `byteferret status`; if it persists, run \
                         `byteferret pair --with <this-device-id> --folder <name>` on that machine",
                    ),
                );
            } else {
                checks.push(Check::new(
                    Level::Ok,
                    "folder share",
                    format!("shared both ways with {connected} peer(s)"),
                ));
            }
        }
    }

    // --- conflicts (across every folder) ---
    let conflict_count: usize = folders
        .iter()
        .filter_map(|f| f.get("path").and_then(|p| p.as_str()))
        .map(|p| find_files(Path::new(p), ".sync-conflict-", 100).len())
        .sum();
    if conflict_count > 0 {
        checks.push(
            Check::new(
                Level::Warn,
                "conflicts",
                format!("{conflict_count} sync-conflict file(s)"),
            )
            .with_hint("open both copies, merge, then delete the *.sync-conflict-* file"),
        );
    }

    checks
}

pub fn doctor(fix: bool) -> Result<()> {
    let paths = Paths::resolve();

    let fixes = if fix { apply_fixes(&paths) } else { Vec::new() };
    if fix {
        if fixes.is_empty() {
            say("--fix: nothing to repair.");
        } else {
            say("--fix applied:");
            for f in &fixes {
                say(&format!("  - {f}"));
            }
            say("");
        }
    }

    let checks = build_checks(&paths);
    let overall = worst(checks.iter().map(|c| c.level));

    for c in &checks {
        say(&format!(
            "  {} {:<16} {} — {}",
            c.level.glyph(),
            c.name,
            c.level.tag(),
            c.detail
        ));
        if c.level != Level::Ok {
            if let Some(h) = &c.hint {
                say(&format!("      ↳ {h}"));
            }
        }
    }
    say("");
    say(&format!(
        "overall: {}",
        match overall {
            Level::Ok => "healthy",
            Level::Warn => "healthy with warnings",
            Level::Fail => "problems found",
        }
    ));

    emit(&json!({
        "ok": overall != Level::Fail,
        "overall": overall.tag(),
        "fixesApplied": fixes,
        "checks": checks.iter().map(|c| json!({
            "name": c.name, "level": c.level.tag(), "detail": c.detail, "hint": c.hint,
        })).collect::<Vec<_>>(),
    }));

    if overall == Level::Fail {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perms_ok_only_for_owner_only_modes() {
        assert!(perms_ok(0o600));
        assert!(perms_ok(0o400));
        assert!(!perms_ok(0o640)); // group-readable
        assert!(!perms_ok(0o604)); // world-readable
        assert!(!perms_ok(0o666));
    }

    #[test]
    fn worst_picks_the_most_severe() {
        assert_eq!(worst([Level::Ok, Level::Ok]), Level::Ok);
        assert_eq!(worst([Level::Ok, Level::Warn]), Level::Warn);
        assert_eq!(worst([Level::Warn, Level::Fail, Level::Ok]), Level::Fail);
        assert_eq!(worst([]), Level::Ok);
    }
}
