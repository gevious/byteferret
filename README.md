# ByteFerret

**Peer-to-peer document vault for your own machines.** ByteFerret keeps a folder
of Markdown notes (and their images, PDFs, and other assets) in sync across your
Linux and macOS machines — directly, machine-to-machine, with no account and no
server in the middle. It does this by bundling and orchestrating a private, version-pinned
[Syncthing](https://syncthing.net/); you drive everything through one small
`byteferret` CLI.

Ships as a single small binary: statically linked musl (~1.5 MB) on Linux, a
native Darwin build on macOS.

> **Status: Phase-1 MVP.** The peer-to-peer core (init a vault, pair machines,
> auto-sync) works today. The always-on hub, web UI, and mobile app are later
> phases. See the `backlog.md` and `getting-started.md` guides in the docs repo
> for the roadmap and what is / isn't implemented yet.

## Install

```sh
curl -fsSL https://byteferret.com/install.sh | sh
```

This detects your OS/arch (Linux or macOS, x86_64 or arm64) and downloads the
latest prebuilt binary to `~/.local/bin/byteferret`. Options via
environment variables (see [`scripts/install.sh`](scripts/install.sh)):

- `BYTEFERRET_ENABLE_SERVICE=1` — also install & start the user service
  (systemd on Linux, launchd on macOS).
- `BYTEFERRET_FROM_SOURCE=1` — build with `cargo` instead of downloading.
- `BYTEFERRET_PREFIX=/some/dir` — install somewhere other than `~/.local/bin`.

### From source

```sh
# Linux — produces a static musl binary (see .cargo/config.toml)
cargo build --release

# macOS — override the pinned musl default with this host's Darwin triple
cargo build --release --target aarch64-apple-darwin   # or x86_64-apple-darwin

# either OS: build the working tree and install it in one step
sh scripts/local-install.sh
```

On Linux this requires the `x86_64-unknown-linux-musl` (or `aarch64-…`) Rust
target and `musl-tools`. `curl` and `tar` must be present at runtime — used once
to fetch the pinned Syncthing (and Typst, for `publish`).

## Quick start (peer-to-peer)

```sh
byteferret init ~/notes            # register a folder to sync (named "notes")
byteferret service install --now   # auto-start the agent on login (optional)

# On machine A:
byteferret status                  # prints this machine's device ID

# On machine B:
byteferret pair --with <A's-device-id>

# Back on machine A:
byteferret status                  # shows B's request, with its device ID
byteferret pair <B's-device-id> --accept                     # approve the machine
byteferret pair <B's-device-id> --accept --folder notes      # share a folder with it

byteferret status                  # health, device id, peers, folders, sync state
```

Edit anything in `~/notes` and it syncs to your paired machines automatically.
Every folder is equal — there is no privileged "vault". You refer to a folder by
its **name** (`notes` above); ids are generated for you and are globally unique,
so a folder name only has to be unique on each machine.

### Accepting is per machine *and* per folder

Approving a machine and giving it a folder are two separate steps. `--accept`
on its own admits the machine and shares nothing; each folder is then granted
individually, so one peer can be given some folders and not others:

```sh
byteferret status                                   # lists requests and offered folders
byteferret pair <id> --accept --folder notes --folder recipes   # grant two folders
byteferret pair <id> --reject --folder recipes      # withdraw one, keep the rest
byteferret pair <id> --accept --all-folders         # take up everything it offers
```

A folder a peer offers is created in the current directory (named after the
folder), or wherever `--path` says. Rejecting an offer declines it; rejecting a
folder you already share stops sharing it *with that one peer*, leaving the
folder and its files in place.

To stop sharing a folder, use `byteferret unpair`:

```sh
byteferret unpair recipes --with laptop   # withdraw it from one peer; stays here + shared with others
byteferret unpair recipes                 # remove it from this machine entirely (asks to confirm)
byteferret unpair recipes --yes           # …skip the prompt (for scripts)
```

Without `--with` it unshares from every peer and unregisters the folder here, so
it asks first (there is no prompt in `--json` mode — pass `--yes`). Either way
your files stay on disk; only the sharing is removed, so `byteferret init <path>`
re-adds it later.

### Naming a device

Device IDs are long. Give one a local alias and then use the alias anywhere an
ID is expected — including when you first pair, before the device is even known:

```sh
byteferret alias FIRSTSEG-... laptop   # or an unambiguous prefix of a known peer
byteferret pair --with laptop          # pair by alias
byteferret pair laptop --accept        # accept by alias
byteferret status                      # shows "laptop (FIRSTSEG…)"; add -v for the full ID
```

Aliases are stored locally in `config.toml`, so unlike a peer's own name (which
the remote machine chooses) they are trusted: `pair` will resolve one to its
device ID. `byteferret alias` with no arguments lists them; `--remove` clears one.

### Syncing more folders

Any directory — new or already full of files — can be brought into sync; ids are
generated automatically and the folder is named after its directory (or
`--label`). Names must be unique on a machine, so a second folder by the same
name is refused. One folder can be shared with any number of peers, and each peer
can get a different subset:

```sh
byteferret init ~/recipes --label recipes          # existing dir, nothing changed in it
byteferret pair --with <peer-id> --folder recipes  # share it by name
```

Both verbs always name one peer by device ID — there is no "accept everything
waiting". Anyone who can reach your machine can put a request in that list, so a
bulk accept would hand them a folder alongside the machine you meant to approve.
Peer-supplied *names* are shown but never matched, and an ambiguous device-ID
prefix is refused rather than guessed.
A fuller two-machine walkthrough (`RUN-TWO-DESKTOPS.md`) and the product vision
+ Path B / hub (`getting-started.md`) live in the docs repo.

## Commands

| Command | What it does |
|---|---|
| `byteferret start` / `stop` | Start/stop the managed Syncthing (idempotent) |
| `byteferret init <path> [--label <name>]` | Bring a folder into sync (names unique per machine; ids automatic) |
| `byteferret pair --with <id> [--folder <name>]` | Pair with a machine and share a folder (the sole folder if just one) |
| `byteferret pair <id> --accept \| --reject [--folder <name> \| --all-folders]` | Approve/decline a machine, or one of its folders |
| `byteferret unpair <name> [--with <peer>]` | Stop sharing a folder — from one peer with `--with`, else remove it here entirely (confirms first; files kept) |
| `byteferret disconnect <device>` | Remove a paired device and withdraw every folder shared with it |
| `byteferret rename <folder> <new-name>` | Rename a folder locally; the shared folder id, path, and other devices are unchanged |
| `byteferret restore <folder> <file> [--at <time>]` | Restore an archived version |
| `byteferret restore --all` | From inside a registered folder, restore every file with history |
| `byteferret alias <id> <name>` | Label a device locally (usable as `<id>`; `--remove` to clear, no args to list) |
| `byteferret status [-v]` | Agent health, hostname + device ID, peers & folders, requests, conflicts (`-v` = full device IDs) |
| `byteferret doctor [--fix]` | Diagnose (and optionally repair) the setup |
| `byteferret logs [-n N] [-f]` | Show/follow the agent log |
| `byteferret publish <file> [--pdf] [--email]` | Render a doc to PDF locally |
| `byteferret service install \| uninstall \| status` | Manage the user service (systemd/launchd) |
| `byteferret version` | Agent + pinned Syncthing versions |

Every command accepts `--json` for machine-readable output.

## How it works

The agent never edits Syncthing's config files directly — it downloads a pinned
Syncthing, generates its keys, runs it detached bound to localhost with a random
API key (`0600`), and drives it entirely over the localhost REST API. Config
lives in `~/.config/byteferret/` (`config.toml` + a `0600` `secrets` file); managed
state in `~/.local/share/byteferret/`. See `.agent/CONTEXT.md` for the architecture
(and `DESIGN.md` in the docs repo for the full design).

## Repository layout

| Path | What |
|---|---|
| `src/` | Rust sources (`main.rs`, `commands/`, `syncthing/`, `publish/`, …). |
| `scripts/` | `install.sh` (curl-installer) and `e2e-local.sh` (two-agent P2P test). |
| `.cargo/config.toml` | Pins the default (Linux musl) build target. |
| `.agent/CONTEXT.md` | Architecture/domain context for the codebase. |

## License

[AGPL-3.0-only](LICENSE). Byteferret does not bundle or redistribute third-party
binaries: [Syncthing](https://github.com/syncthing/syncthing) (MPL-2.0) and
[Typst](https://github.com/typst/typst) (Apache-2.0) are downloaded at first run
from their official GitHub releases, checksum-verified, and run as separate
processes.

Dependency licenses are enforced in CI: `cargo deny check licenses` (config in
[`deny.toml`](deny.toml)) fails the build if any crate in the dependency graph
carries a license outside the AGPL-compatible allowlist.
