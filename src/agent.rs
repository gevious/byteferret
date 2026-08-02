//! Orchestration facade the CLI commands build on. Owns the wiring between the
//! agent's persisted state (config + secrets), the managed Syncthing process,
//! and the REST client — so commands express intent, not plumbing.

// `Context` is imported anonymously: the trait's `.with_context()` is what we
// want, and the name itself belongs to this module's own `Context` struct.
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context as _, Result};
use serde_json::{json, Value};

use crate::config::{Config, Secrets};
use crate::paths::Paths;
use crate::syncthing::process;
use crate::syncthing::rest::Client;

pub struct Context {
    pub paths: Paths,
    pub config: Config,
    pub api_key: String,
    pub client: Client,
}

/// Ensures Syncthing is up and returns a ready Context. Persists the generated
/// API key (secrets, 0600) and effective settings (config) so later commands
/// and restarts converge to the same instance (FR-4 idempotency).
pub fn ensure_started() -> Result<Context> {
    let paths = Paths::resolve();
    let config = Config::load(&paths)?;
    let secrets = Secrets::load(&paths)?;

    let res = process::start(&paths, &config.gui_address, secrets.syncthing_api_key.clone())?;

    if secrets.syncthing_api_key.as_deref() != Some(res.api_key.as_str()) {
        Secrets { syncthing_api_key: Some(res.api_key.clone()) }.save(&paths)?;
    }
    config.save(&paths)?;

    let client = Client::new(&res.gui_address, &res.api_key);
    if !res.already_running {
        apply_runtime_toggles(&client)?;
    }
    Ok(Context { paths, config, api_key: res.api_key, client })
}

/// Load a Context without (re)starting — for read-only commands.
pub fn load_context() -> Result<Context> {
    let paths = Paths::resolve();
    let config = Config::load(&paths)?;
    let secrets = Secrets::load(&paths)?;
    let api_key = secrets.syncthing_api_key.unwrap_or_default();
    let client = Client::new(&config.gui_address, &api_key);
    Ok(Context { paths, config, api_key, client })
}

/// Applies privacy / connectivity toggles that map to documented options:
///  - BYTEFERRET_SYNC_ADDRESS       → pin Syncthing's sync listen address (also
///    lets two isolated agents coexist on one host for testing).
///  - BYTEFERRET_DISCOVERY_PUBLIC=false → the getting-started `discovery.public`
///    off switch: no global/local announce, no relays (LAN/VPN only).
///
/// Both are no-ops when unset, so real desktops keep Syncthing's defaults.
fn apply_runtime_toggles(client: &Client) -> Result<()> {
    let sync_addr = std::env::var("BYTEFERRET_SYNC_ADDRESS").ok().filter(|s| !s.is_empty());
    let public = std::env::var("BYTEFERRET_DISCOVERY_PUBLIC").ok();
    if sync_addr.is_none() && public.is_none() {
        return Ok(());
    }
    let mut opts = client.get_options()?;
    if let Some(addr) = sync_addr {
        opts["listenAddresses"] = json!([addr]);
    }
    if public.as_deref() == Some("false") {
        opts["globalAnnounceEnabled"] = json!(false);
        opts["localAnnounceEnabled"] = json!(false);
        opts["relaysEnabled"] = json!(false);
    }
    client.put_options(&opts)?;
    Ok(())
}

/// Adds (or updates) a peer device — the *connection* half of pairing, with no
/// folder consequences. Sharing a folder is a separate, explicit decision, so
/// that approving a machine and granting it a document folder are never the same
/// keystroke. Idempotent.
pub fn add_device(
    client: &Client,
    device_id: &str,
    name: Option<&str>,
    address: Option<&str>,
) -> Result<()> {
    let existing = client
        .get_devices()?
        .into_iter()
        .find(|d| d.get("deviceID").and_then(Value::as_str) == Some(device_id));

    let name = name
        .map(str::to_string)
        .or_else(|| existing.as_ref().and_then(|d| d.get("name")).and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| device_id.chars().take(7).collect());

    let addresses: Vec<String> = match address {
        Some(a) => vec![a.to_string()],
        None => existing
            .as_ref()
            .and_then(|d| d.get("addresses"))
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .filter(|v: &Vec<String>| !v.is_empty())
            .unwrap_or_else(|| vec!["dynamic".to_string()]),
    };

    // FR-7: never auto-accept unknown folders; never act as introducer. Both
    // would let a peer widen its own access after we approved the connection.
    let device = json!({
        "deviceID": device_id,
        "name": name,
        "addresses": addresses,
        "autoAcceptFolders": false,
        "introducer": false,
    });
    client.put_device(&device)?;
    Ok(())
}

/// Shares a folder we already have with a peer. Returns false when the peer was
/// already a member (nothing to do). Errors if the folder isn't configured here.
pub fn share_folder(client: &Client, folder_id: &str, device_id: &str) -> Result<bool> {
    let mut folder = client
        .get_folder(folder_id)?
        .ok_or_else(|| anyhow!("no folder '{folder_id}' is configured on this machine"))?;
    let already = folder
        .get("devices")
        .and_then(Value::as_array)
        .map(|a| a.iter().any(|d| d.get("deviceID").and_then(Value::as_str) == Some(device_id)))
        .unwrap_or(false);
    if already {
        return Ok(false);
    }
    let Some(arr) = folder.get_mut("devices").and_then(Value::as_array_mut) else {
        bail!("folder '{folder_id}' has no device list to extend");
    };
    arr.push(json!({ "deviceID": device_id }));
    client.put_folder(&folder)?;
    Ok(true)
}

/// Stops sharing a folder with a peer, leaving both the folder and the peer in
/// place. This is what withdrawing one folder from one machine looks like.
/// Returns false when the peer was not a member.
pub fn unshare_folder(client: &Client, folder_id: &str, device_id: &str) -> Result<bool> {
    let Some(mut folder) = client.get_folder(folder_id)? else {
        return Ok(false);
    };
    let Some(arr) = folder.get_mut("devices").and_then(Value::as_array_mut) else {
        return Ok(false);
    };
    let before = arr.len();
    arr.retain(|d| d.get("deviceID").and_then(Value::as_str) != Some(device_id));
    if arr.len() == before {
        return Ok(false);
    }
    client.put_folder(&folder)?;
    Ok(true)
}

/// The id of a *different* folder already registered at `dir`, if any.
///
/// Two folders that point at the same directory are a data hazard: both are
/// `sendreceive`, so each reconciles the shared directory against its own peer's
/// index and deletes or overwrites whatever the other put there. This is the
/// check that turns "second folder silently overwrites the first" into a refusal.
/// Paths are compared canonically where they exist, so `~/a`, `a/`, and a
/// symlinked route to the same directory all count as the same place.
pub fn folder_registered_at(client: &Client, dir: &Path) -> Result<Option<String>> {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let target = canon(dir);
    for f in client.get_folders()? {
        let Some(id) = f.get("id").and_then(Value::as_str) else { continue };
        let Some(p) = f.get("path").and_then(Value::as_str) else { continue };
        if canon(&PathBuf::from(p)) == target {
            return Ok(Some(id.to_string()));
        }
    }
    Ok(None)
}

/// Creates a folder a peer offered us, at `path`, shared with that peer.
/// `label` is only ever used as display text — the caller decides the path.
pub fn create_shared_folder(
    client: &Client,
    folder_id: &str,
    label: &str,
    path: &str,
    device_id: &str,
) -> Result<()> {
    // Last line of defence against merging two folders into one directory. The
    // caller is expected to have chosen a free path already; this guarantees no
    // code path can ever register a second folder on top of an existing one.
    if let Some(other) = folder_registered_at(client, Path::new(path))? {
        if other != folder_id {
            bail!(
                "refusing to create folder '{folder_id}' at {path}: folder '{other}' already \
                 lives there, and two folders in one directory would overwrite each other"
            );
        }
    }
    std::fs::create_dir_all(path).with_context(|| format!("creating {path}"))?;
    let folder = folder_config(folder_id, label, path, &[device_id.to_string()]);
    client.put_folder(&folder)
}

/// Syncthing device ids are uppercase base32 whose dashes are only visual
/// grouping, so neither case nor dashes are significant when comparing.
pub fn normalize_id(s: &str) -> String {
    s.chars().filter(|c| *c != '-').flat_map(char::to_uppercase).collect()
}

/// Shortest device-id prefix we will act on: one full Syncthing group, which is
/// what `status` prints and still 35 bits of the peer's certificate hash.
const MIN_TARGET_LEN: usize = 7;

/// Resolve a user-typed pairing target to exactly one device id.
///
/// Matching is on the **device id only**. Every peer also carries a `name`, but
/// that name is chosen by the remote machine: allowing it to match would let a
/// stranger call itself `laptop` and be approved by a user who meant their own
/// laptop. The device id is the one identifier a peer cannot forge — it is a
/// hash of its TLS certificate.
///
/// An ambiguous prefix is an error rather than a guess, and never expands to
/// "all of them": accepting is what grants access, so it acts on exactly one
/// peer, named deliberately.
pub fn resolve_device_target(target: &str, candidates: &[(String, String)]) -> Result<String> {
    let norm = normalize_id(target);
    if norm.len() < MIN_TARGET_LEN {
        bail!(
            "'{target}' is too short to identify a device — use at least {MIN_TARGET_LEN} \
             characters of its device id (see `byteferret status`)"
        );
    }

    let matches: Vec<&(String, String)> = candidates
        .iter()
        .filter(|(id, _)| normalize_id(id).starts_with(&norm))
        .collect();

    match matches.as_slice() {
        [(id, _)] => Ok(id.clone()),
        [] => bail!(
            "no device id starts with '{target}'.{}",
            list_candidates("Known devices", candidates)
        ),
        many => bail!(
            "'{target}' matches {} devices — use more of the device id.{}",
            many.len(),
            list_candidates(
                "Matches",
                &many.iter().map(|(a, b)| (a.clone(), b.clone())).collect::<Vec<_>>()
            )
        ),
    }
}

/// The user-facing name of a folder: its Syncthing id with the random
/// uniqueness suffix (`-` + six hex digits, added by `init`) stripped off. The
/// suffix keeps ids globally unique so two machines that independently create a
/// same-named folder never silently merge; it is an implementation detail the
/// user never sees or types. Ids without such a suffix (e.g. a legacy
/// `byteferret-vault`) are returned unchanged.
pub fn folder_name(id: &str) -> &str {
    if let Some((base, suffix)) = id.rsplit_once('-') {
        if suffix.len() == 6 && suffix.chars().all(|c| c.is_ascii_hexdigit()) && !base.is_empty() {
            return base;
        }
    }
    id
}

/// Resolve a user-typed folder reference — its visible name, full id, or an
/// unambiguous name/id prefix — to the real Syncthing folder id of a folder that
/// exists on this machine. Names are unique, so an exact name match is decisive;
/// otherwise we accept an unambiguous prefix and refuse an ambiguous one.
pub fn resolve_folder(client: &Client, wanted: &str) -> Result<String> {
    let ids: Vec<String> = client
        .get_folders()?
        .iter()
        .filter_map(|f| f.get("id").and_then(Value::as_str).map(str::to_string))
        .collect();

    // Exact match on the visible name or the full id wins outright.
    if let Some(id) = ids
        .iter()
        .find(|id| folder_name(id).eq_ignore_ascii_case(wanted) || id.eq_ignore_ascii_case(wanted))
    {
        return Ok(id.clone());
    }

    // Otherwise an unambiguous prefix of a name (or id) is enough.
    let matches: Vec<&String> = ids
        .iter()
        .filter(|id| {
            folder_name(id).to_lowercase().starts_with(&wanted.to_lowercase())
                || id.to_lowercase().starts_with(&wanted.to_lowercase())
        })
        .collect();
    match matches.as_slice() {
        [one] => Ok((*one).clone()),
        [] => bail!(
            "no folder here is named '{}'.{}",
            wanted,
            list_folders(&ids)
        ),
        many => bail!(
            "'{}' matches {} folders — be more specific.{}",
            wanted,
            many.len(),
            list_folders(&many.iter().map(|s| (*s).clone()).collect::<Vec<_>>())
        ),
    }
}

/// Names of the folders registered here, for an error hint.
fn list_folders(ids: &[String]) -> String {
    if ids.is_empty() {
        return "\nNo folders here yet — run `byteferret init <path>`.".to_string();
    }
    let mut s = String::from("\nFolders here:");
    for id in ids {
        s.push_str(&format!("\n  {}", folder_name(id)));
    }
    s
}

/// Resolve a pairing target that may be a user-set alias *or* a device id
/// (prefix). A local alias is trusted — the user set it themselves — so an exact
/// alias match wins outright; otherwise we fall back to id-prefix matching, which
/// deliberately never matches a peer-chosen name (see `resolve_device_target`).
pub fn resolve_peer(config: &Config, target: &str, candidates: &[(String, String)]) -> Result<String> {
    if let Some(id) = config.device_for_alias(target) {
        return Ok(id.to_string());
    }
    resolve_device_target(target, candidates)
}

/// Render candidates for an error message. Names come from the peer, so they are
/// sanitized and shown only as a hint — the id is what the user must type.
fn list_candidates(heading: &str, candidates: &[(String, String)]) -> String {
    if candidates.is_empty() {
        return String::new();
    }
    let mut s = format!("\n{heading}:");
    for (id, name) in candidates {
        let name = crate::output::sanitize(name);
        if name.is_empty() {
            s.push_str(&format!("\n  {id}"));
        } else {
            s.push_str(&format!("\n  {id}  ({name})"));
        }
    }
    s
}

/// A peer's connectivity plus whether the folders we share with it are actually
/// reciprocated. `share` is the aggregate across every folder shared with the
/// peer, so it still catches the silent stall where two machines are connected
/// but one never shared a folder back and nothing ever transfers.
pub struct PeerSync {
    pub id: String,
    pub name: String,
    pub connected: bool,
    /// Worst share state across every folder shared with this peer (`Offline`
    /// when disconnected; `Sharing` when there is nothing amiss, including a bare
    /// connection with no folders shared yet).
    pub share: ShareState,
}

/// Order two share states by severity, keeping the worse one — used to fold a
/// peer's per-folder states into a single verdict.
fn worse(a: ShareState, b: ShareState) -> ShareState {
    fn rank(s: ShareState) -> u8 {
        match s {
            ShareState::Sharing => 0,
            ShareState::Offline => 1,
            ShareState::Establishing => 2,
            ShareState::NotSharingBack => 3,
        }
    }
    if rank(b) > rank(a) { b } else { a }
}

/// How a connected peer's copy of the vault relates to ours. This is the single
/// classification that both `status` and `doctor` render, so the two can
/// never disagree about whether a peer is sharing (the bug where one said
/// "NOT sharing ✗" while `doctor` reported "shared both ways ✓" for the same
/// `remote_state == "unknown"` peer).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShareState {
    /// Peer is offline — sharing can't be assessed.
    Offline,
    /// Connected and actively sharing the vault back (`remote_state == "valid"`).
    Sharing,
    /// Connected but the peer has NOT shared the vault back
    /// (`remote_state` "notSharing"/"paused") — the silent stall.
    NotSharingBack,
    /// Connected, but the share isn't confirmed yet (`remote_state` "unknown" or
    /// unavailable). Normal for a moment right after a link comes up; if it
    /// persists it means the peer still hasn't shared the vault back.
    Establishing,
}

impl ShareState {
    /// Classify one (folder, peer) pair from Syncthing's `remoteState` label.
    /// Used per-folder by `status` and via `PeerSync::share_state` for the vault,
    /// so every rendering of "is this being shared back?" agrees.
    pub fn classify(connected: bool, remote_state: &str) -> ShareState {
        if !connected {
            ShareState::Offline
        } else if remote_state == "valid" {
            ShareState::Sharing
        } else if remote_state == "notSharing" || remote_state == "paused" {
            ShareState::NotSharingBack
        } else {
            ShareState::Establishing
        }
    }

    /// Stable machine-readable tag for `--json` consumers.
    pub fn tag(self) -> &'static str {
        match self {
            ShareState::Offline => "offline",
            ShareState::Sharing => "sharing",
            ShareState::NotSharingBack => "notSharingBack",
            ShareState::Establishing => "establishing",
        }
    }
}

impl PeerSync {
    /// Human label: the device name, or a short id prefix when it has no name.
    pub fn label(&self) -> String {
        if self.name.is_empty() {
            self.id.chars().take(7).collect()
        } else {
            self.name.clone()
        }
    }

    /// The single source of truth for a peer's share status. Every command
    /// derives its verdict from this so their reports always agree.
    pub fn share_state(&self) -> ShareState {
        self.share
    }

    /// True when nothing is amiss: connected and every shared folder reciprocated
    /// (also true for a bare connection with no folders shared yet).
    pub fn sharing(&self) -> bool {
        self.share == ShareState::Sharing
    }

    /// A connected peer that has not shared a folder back (the stall case).
    /// Excludes the transient "unknown"/establishing state to avoid false alarms
    /// right after a connection comes up.
    pub fn stalled(&self) -> bool {
        self.share == ShareState::NotSharingBack
    }
}

/// Summarize every paired peer (excluding this device): connection state and,
/// for connected peers, whether the folders shared with them are reciprocated —
/// aggregated across all such folders. Shared by `status` and `doctor` so both
/// surface the same truth.
pub fn peer_status(ctx: &Context) -> Result<Vec<PeerSync>> {
    let my_id = ctx.client.my_device_id()?;
    let conns = ctx.client.connections()?;
    let folders = ctx.client.get_folders()?;
    let mut out = Vec::new();
    for d in ctx.client.get_devices()? {
        let id = match d.get("deviceID").and_then(Value::as_str) {
            Some(i) if i != my_id => i.to_string(),
            _ => continue, // skip self / malformed entries
        };
        let name = d.get("name").and_then(Value::as_str).unwrap_or("").to_string();
        let connected = conns.get(&id).map(|c| c.connected).unwrap_or(false);

        // Folders we share with this peer, so the verdict spans every folder
        // rather than one privileged "vault".
        let shared_ids: Vec<String> = folders
            .iter()
            .filter(|f| {
                f.get("devices")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().any(|dv| dv.get("deviceID").and_then(Value::as_str) == Some(&id)))
                    .unwrap_or(false)
            })
            .filter_map(|f| f.get("id").and_then(Value::as_str).map(str::to_string))
            .collect();

        // Asking the remote-state question is meaningless (and a wasted round
        // trip) when offline; a bare connection with nothing shared is fine.
        let share = if !connected {
            ShareState::Offline
        } else {
            shared_ids.iter().fold(ShareState::Sharing, |acc, fid| {
                let st = match ctx.client.folder_completion(fid, &id) {
                    Ok(c) => ShareState::classify(true, &c.remote_state),
                    Err(_) => ShareState::Establishing,
                };
                worse(acc, st)
            })
        };
        out.push(PeerSync { id, name, connected, share });
    }
    Ok(out)
}

/// Standard folder settings (near-real-time sync, FR-6/FR-9/FR-17).
pub fn folder_config(id: &str, label: &str, path: &str, peers: &[String]) -> Value {
    let devices: Vec<Value> = peers.iter().map(|p| json!({ "deviceID": p })).collect();
    json!({
        "id": id,
        "label": label,
        "path": path,
        "type": "sendreceive",
        "fsWatcherEnabled": true,
        "fsWatcherDelayS": 5,
        "rescanIntervalS": 60,
        "devices": devices,
        "versioning": staggered_versioning(),
    })
}

/// Local file history, kept in each folder's `.stversions/`.
///
/// Syncthing's staggered scheme thins versions out as they age. The *bands are
/// fixed in Syncthing and cannot be tuned* — they are always: one version per
/// 30s for the first hour, per hour for the first day, per day for the first
/// 30 days, then per week up to `maxAge`. `maxAge = 0` keeps them forever.
///
/// This is the closest built-in fit to "iteration-level recently, thinning to
/// daily / weekly / effectively-monthly as it ages, retained indefinitely". It
/// differs from an ideal curve in two ways worth knowing: sub-hour granularity
/// lasts only the first hour (not a full week), and the coarsest retained
/// cadence is weekly (there is no monthly band).
fn staggered_versioning() -> Value {
    json!({
        "type": "staggered",
        "params": { "maxAge": "0" }, // 0 = keep versions forever
    })
}

/// Turn on staggered local history for any registered folder that has none.
///
/// `folder_config` only runs when a folder is first created, so folders made
/// before history was enabled (or with it explicitly off) keep nothing. This
/// upgrades them in place — reading each folder, setting the staggered scheme,
/// and writing it back — while leaving any folder that already versions
/// untouched. Returns the display names of the folders that were changed.
pub fn enable_history_on_folders(client: &Client) -> Result<Vec<String>> {
    let mut changed = Vec::new();
    for mut folder in client.get_folders()? {
        let versions = folder
            .get("versioning")
            .and_then(|v| v.get("type"))
            .and_then(Value::as_str)
            .is_some_and(|t| !t.is_empty());
        if versions {
            continue;
        }
        let Some(id) = folder.get("id").and_then(Value::as_str).map(str::to_string) else {
            continue;
        };
        folder["versioning"] = staggered_versioning();
        client.put_folder(&folder)?;
        changed.push(folder_name(&id).to_string());
    }
    Ok(changed)
}

#[cfg(test)]
mod resolve_peer_tests {
    use super::resolve_peer;
    use crate::config::Config;
    use std::collections::BTreeMap;

    const A: &str = "AAAAAAA-BBBBBBB-CCCCCCC-DDDDDDD-EEEEEEE-FFFFFFF-GGGGGGG-HHHHHHH";
    const C: &str = "QQQQQQQ-BBBBBBB-CCCCCCC-DDDDDDD-EEEEEEE-FFFFFFF-GGGGGGG-HHHHHHH";

    fn cfg_with(alias: &str, id: &str) -> Config {
        let mut aliases = BTreeMap::new();
        aliases.insert(id.to_string(), alias.to_string());
        Config { gui_address: String::new(), aliases }
    }

    #[test]
    fn a_trusted_alias_resolves_even_with_no_candidates() {
        let cfg = cfg_with("laptop", A);
        // No candidate list at all — the alias alone identifies the device.
        assert_eq!(resolve_peer(&cfg, "laptop", &[]).unwrap(), A);
        assert_eq!(resolve_peer(&cfg, "LAPTOP", &[]).unwrap(), A); // case-insensitive
    }

    #[test]
    fn falls_back_to_id_prefix_when_no_alias_matches() {
        let cfg = cfg_with("laptop", A);
        let cands = vec![(C.to_string(), "desktop".to_string())];
        assert_eq!(resolve_peer(&cfg, "QQQQQQQ", &cands).unwrap(), C);
    }
}

#[cfg(test)]
mod target_tests {
    use super::resolve_device_target;

    const A: &str = "AAAAAAA-BBBBBBB-CCCCCCC-DDDDDDD-EEEEEEE-FFFFFFF-GGGGGGG-HHHHHHH";
    const B: &str = "AAAAAAA-ZZZZZZZ-CCCCCCC-DDDDDDD-EEEEEEE-FFFFFFF-GGGGGGG-HHHHHHH";
    const C: &str = "QQQQQQQ-BBBBBBB-CCCCCCC-DDDDDDD-EEEEEEE-FFFFFFF-GGGGGGG-HHHHHHH";

    fn cands() -> Vec<(String, String)> {
        vec![
            (A.to_string(), "my-laptop".to_string()),
            (B.to_string(), "my-laptop".to_string()), // same name on purpose
            (C.to_string(), "desktop".to_string()),
        ]
    }

    #[test]
    fn resolves_a_full_id_and_an_unambiguous_prefix() {
        assert_eq!(resolve_device_target(C, &cands()).unwrap(), C);
        assert_eq!(resolve_device_target("QQQQQQQ", &cands()).unwrap(), C);
        // dashes and case are cosmetic
        assert_eq!(resolve_device_target("qqqqqqq", &cands()).unwrap(), C);
        assert_eq!(resolve_device_target("QQQQQQQBBBBBBB", &cands()).unwrap(), C);
    }

    #[test]
    fn a_peer_chosen_name_never_matches() {
        // Two peers claim the name "my-laptop"; a name must not select either,
        // or a stranger could impersonate the machine the user meant.
        let err = resolve_device_target("my-laptop", &cands()).unwrap_err().to_string();
        assert!(err.contains("too short") || err.contains("no device id"), "{err}");
        assert!(resolve_device_target("desktop", &cands()).is_err());
    }

    #[test]
    fn ambiguous_prefixes_are_refused_not_guessed() {
        // Both A and B start with AAAAAAA — acting on either would be a coin flip.
        let err = resolve_device_target("AAAAAAA", &cands()).unwrap_err().to_string();
        assert!(err.contains("matches 2 devices"), "{err}");
        // Extending the prefix past the shared part disambiguates.
        assert_eq!(resolve_device_target("AAAAAAAZ", &cands()).unwrap(), B);
    }

    #[test]
    fn short_targets_are_refused() {
        assert!(resolve_device_target("A", &cands()).is_err());
        assert!(resolve_device_target("AAAAAA", &cands()).is_err()); // 6 chars
        assert!(resolve_device_target("", &cands()).is_err());
    }

    #[test]
    fn unknown_target_errors_rather_than_matching_nothing_silently() {
        assert!(resolve_device_target("ZZZZZZZZ", &cands()).is_err());
    }
}

#[cfg(test)]
mod tests {
    use super::{PeerSync, ShareState};

    // Build a single-folder peer whose aggregate share state is what that one
    // folder's `remote_state` classifies to — the pre-multi-folder behaviour.
    fn peer(name: &str, connected: bool, remote_state: &str) -> PeerSync {
        PeerSync {
            id: "ABCDEFG-HIJKLMN".to_string(),
            name: name.to_string(),
            connected,
            share: ShareState::classify(connected, remote_state),
        }
    }

    #[test]
    fn sharing_requires_connected_and_valid() {
        assert!(peer("a", true, "valid").sharing());
        assert!(!peer("a", true, "notSharing").sharing());
        assert!(!peer("a", false, "valid").sharing()); // offline
    }

    #[test]
    fn stalled_flags_connected_but_not_sharing() {
        assert!(peer("a", true, "notSharing").stalled());
        assert!(peer("a", true, "paused").stalled());
        assert!(!peer("a", true, "valid").stalled());
        assert!(!peer("a", true, "unknown").stalled()); // transient, not a stall
        assert!(!peer("a", false, "notSharing").stalled()); // offline isn't a stall
    }

    #[test]
    fn share_state_classifies_every_remote_state() {
        use super::ShareState::*;
        assert_eq!(peer("a", true, "valid").share_state(), Sharing);
        assert_eq!(peer("a", true, "notSharing").share_state(), NotSharingBack);
        assert_eq!(peer("a", true, "paused").share_state(), NotSharingBack);
        assert_eq!(peer("a", true, "unknown").share_state(), Establishing);
        assert_eq!(peer("a", true, "").share_state(), Establishing); // completion call errored
        assert_eq!(peer("a", false, "valid").share_state(), Offline);
    }

    #[test]
    fn label_falls_back_to_short_id() {
        assert_eq!(peer("laptop", true, "valid").label(), "laptop");
        assert_eq!(peer("", true, "valid").label(), "ABCDEFG");
    }
}

#[cfg(test)]
mod folder_name_tests {
    use super::folder_name;

    #[test]
    fn strips_the_random_hex_suffix() {
        assert_eq!(folder_name("recipes-a3f9c1"), "recipes");
        // A name that itself contains dashes keeps them; only the suffix goes.
        assert_eq!(folder_name("work-notes-00ffab"), "work-notes");
    }

    #[test]
    fn leaves_ids_without_a_hex_suffix_alone() {
        assert_eq!(folder_name("byteferret-vault"), "byteferret-vault"); // legacy id
        assert_eq!(folder_name("recipes"), "recipes");
        assert_eq!(folder_name("notes-xyz123"), "notes-xyz123"); // not all hex
        assert_eq!(folder_name("notes-abc12"), "notes-abc12"); // 5 chars, not 6
    }
}
