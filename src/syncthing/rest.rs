//! Thin wrapper over Syncthing's localhost REST API. Every agent interaction
//! with Syncthing goes through here (FR-8 — no config-file edits while it runs).
//! The API key authenticates via the X-API-Key header.
//!
//! Config objects (folders/devices/options) are passed as `serde_json::Value`
//! so Syncthing's own fields survive a get-modify-put round trip untouched.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use serde::Deserialize;
use serde_json::Value;

pub struct Client {
    base: String,
    api_key: String,
    agent: ureq::Agent,
}

#[derive(Deserialize)]
struct SystemStatus {
    #[serde(rename = "myID")]
    my_id: String,
}

#[derive(Deserialize)]
struct SystemVersion {
    version: String,
}

#[derive(Deserialize)]
pub struct FolderStatus {
    pub state: String,
    #[serde(rename = "needBytes")]
    pub need_bytes: i64,
}

#[derive(Deserialize)]
pub struct Connection {
    pub connected: bool,
    #[serde(default)]
    pub address: String,
}

#[derive(Deserialize)]
struct Connections {
    connections: BTreeMap<String, Connection>,
}

#[derive(Deserialize)]
pub struct PendingDevice {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub address: String,
}

/// A folder some peer has offered to share with us that we have not accepted.
/// Keyed by folder id; `offered_by` maps each announcing device id to what that
/// device told us about the folder. The same folder id can be offered by several
/// peers, which is why accepting is always scoped to one (folder, device) pair.
#[derive(Deserialize, Default)]
pub struct PendingFolder {
    #[serde(rename = "offeredBy", default)]
    pub offered_by: BTreeMap<String, OfferedFolder>,
}

/// The remote's description of an offered folder. Every field here is chosen by
/// the *peer*, so treat `label` as untrusted text (see `output::sanitize`) and
/// never as a filesystem path.
#[derive(Deserialize, Default)]
pub struct OfferedFolder {
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub time: String,
    #[serde(rename = "receiveEncrypted", default)]
    pub receive_encrypted: bool,
    #[serde(rename = "remoteEncrypted", default)]
    pub remote_encrypted: bool,
}

/// Per-device view of how complete our shared folder is on the *remote* side.
/// `remote_state` is Syncthing's coarse label for the peer's copy of the folder:
/// "valid" (they share it), "notSharing" (they never added it), "paused", or
/// "unknown". The whole "connected but nothing syncs" trap is remote_state
/// sitting at "notSharing".
#[derive(Deserialize)]
pub struct Completion {
    #[serde(default)]
    pub completion: f64,
    #[serde(rename = "remoteState", default)]
    pub remote_state: String,
}

impl Client {
    pub fn new(gui_address: &str, api_key: &str) -> Client {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout_read(Duration::from_secs(30))
            .build();
        Client { base: format!("http://{gui_address}"), api_key: api_key.to_string(), agent }
    }

    fn req(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value> {
        let url = format!("{}{}", self.base, path);
        let r = self.agent.request(method, &url).set("X-API-Key", &self.api_key);
        let resp = match body {
            Some(b) => r.send_json(b.clone()),
            None => r.call(),
        };
        match resp {
            Ok(resp) => {
                let s = resp.into_string().unwrap_or_default();
                if s.trim().is_empty() {
                    Ok(Value::Null)
                } else {
                    serde_json::from_str(&s).map_err(|e| anyhow!("bad JSON from {method} {path}: {e}"))
                }
            }
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                bail!("Syncthing API {method} {path} failed: {code} {}", body.trim())
            }
            Err(e) => bail!("cannot reach Syncthing at {} — is the agent started? ({e})", self.base),
        }
    }

    fn get_typed<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T> {
        let v = self.req("GET", path, None)?;
        Ok(serde_json::from_value(v)?)
    }

    pub fn ping(&self) -> bool {
        self.req("GET", "/rest/system/ping", None).is_ok()
    }

    pub fn my_device_id(&self) -> Result<String> {
        let s: SystemStatus = self.get_typed("/rest/system/status")?;
        Ok(s.my_id)
    }

    pub fn version(&self) -> Result<String> {
        let v: SystemVersion = self.get_typed("/rest/system/version")?;
        Ok(v.version)
    }

    // --- options (round-tripped as Value to preserve unknown fields) ---

    pub fn get_options(&self) -> Result<Value> {
        self.req("GET", "/rest/config/options", None)
    }

    pub fn put_options(&self, options: &Value) -> Result<()> {
        self.req("PUT", "/rest/config/options", Some(options)).map(|_| ())
    }

    // --- folders ---

    pub fn get_folders(&self) -> Result<Vec<Value>> {
        match self.req("GET", "/rest/config/folders", None)? {
            Value::Array(a) => Ok(a),
            _ => Ok(vec![]),
        }
    }

    pub fn get_folder(&self, id: &str) -> Result<Option<Value>> {
        Ok(self.get_folders()?.into_iter().find(|f| f.get("id").and_then(Value::as_str) == Some(id)))
    }

    pub fn put_folder(&self, folder: &Value) -> Result<()> {
        let id = folder.get("id").and_then(Value::as_str).ok_or_else(|| anyhow!("folder missing id"))?;
        self.req("PUT", &format!("/rest/config/folders/{id}"), Some(folder)).map(|_| ())
    }

    /// Remove a folder from Syncthing entirely (unregister it on this machine).
    /// Stops all syncing and unshares it from every peer; the directory and its
    /// files on disk are left untouched — Syncthing only drops the folder config.
    pub fn delete_folder(&self, id: &str) -> Result<()> {
        self.req("DELETE", &format!("/rest/config/folders/{id}"), None).map(|_| ())
    }

    pub fn folder_status(&self, id: &str) -> Result<FolderStatus> {
        self.get_typed(&format!("/rest/db/status?folder={}", urlencode(id)))
    }

    // --- file history (.stversions) ---

    /// Every archived version Syncthing holds for a folder, as a map of file path
    /// (relative to the folder root, forward-slashed) to the list of versions.
    /// Each version carries an RFC3339 `versionTime` that is the tag used to
    /// restore it. Empty object when the folder has no history yet.
    pub fn folder_versions(&self, id: &str) -> Result<Value> {
        self.req("GET", &format!("/rest/folder/versions?folder={}", urlencode(id)), None)
    }

    /// Restore archived versions: `picks` maps each file path to the exact
    /// `versionTime` to bring back. Syncthing archives whatever is currently at
    /// that path before overwriting it, so the restore is itself reversible.
    /// Returns Syncthing's per-file result map — an empty error string against a
    /// file means it was restored, a non-empty one is why it was not.
    pub fn restore_versions(&self, id: &str, picks: &Value) -> Result<Value> {
        self.req("POST", &format!("/rest/folder/versions?folder={}", urlencode(id)), Some(picks))
    }

    /// How complete our folder is on a specific peer — the check that reveals a
    /// peer connected but not actually sharing the vault back.
    pub fn folder_completion(&self, folder: &str, device: &str) -> Result<Completion> {
        self.get_typed(&format!(
            "/rest/db/completion?folder={}&device={}",
            urlencode(folder),
            urlencode(device)
        ))
    }

    // --- devices ---

    pub fn get_devices(&self) -> Result<Vec<Value>> {
        match self.req("GET", "/rest/config/devices", None)? {
            Value::Array(a) => Ok(a),
            _ => Ok(vec![]),
        }
    }

    pub fn put_device(&self, device: &Value) -> Result<()> {
        let id = device.get("deviceID").and_then(Value::as_str).ok_or_else(|| anyhow!("device missing deviceID"))?;
        self.req("PUT", &format!("/rest/config/devices/{id}"), Some(device)).map(|_| ())
    }

    // --- cluster / connections ---

    pub fn pending_devices(&self) -> Result<BTreeMap<String, PendingDevice>> {
        let v = self.req("GET", "/rest/cluster/pending/devices", None)?;
        Ok(serde_json::from_value(v).unwrap_or_default())
    }

    /// Dismiss a pending pairing request (the `pair --reject` action). Removes
    /// the device from Syncthing's pending list without adding it as a peer.
    pub fn dismiss_pending_device(&self, device: &str) -> Result<()> {
        self.req(
            "DELETE",
            &format!("/rest/cluster/pending/devices?device={}", urlencode(device)),
            None,
        )
        .map(|_| ())
    }

    /// Folders peers have offered us but that we have not added — the per-folder
    /// half of pairing. A folder only shows up here once its announcing device is
    /// already a configured peer, so accepting one is always a decision about a
    /// peer we have already let in.
    pub fn pending_folders(&self) -> Result<BTreeMap<String, PendingFolder>> {
        let v = self.req("GET", "/rest/cluster/pending/folders", None)?;
        Ok(serde_json::from_value(v).unwrap_or_default())
    }

    /// Dismiss one peer's offer of a folder, without adding it. `device` scopes
    /// the dismissal to that peer's announcement — Syncthing would otherwise drop
    /// the offer from *every* device, silently discarding a second peer's
    /// unrelated offer of the same folder id.
    pub fn dismiss_pending_folder(&self, folder: &str, device: &str) -> Result<()> {
        self.req(
            "DELETE",
            &format!(
                "/rest/cluster/pending/folders?folder={}&device={}",
                urlencode(folder),
                urlencode(device)
            ),
            None,
        )
        .map(|_| ())
    }

    pub fn connections(&self) -> Result<BTreeMap<String, Connection>> {
        let c: Connections = self.get_typed("/rest/system/connections")?;
        Ok(c.connections)
    }
}

/// Minimal percent-encoding for a folder id in a query string.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}
