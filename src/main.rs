//! byteferret — P2P document vault agent (Path A, peer-to-peer).
//! Orchestrates a bundled, version-pinned Syncthing to sync a vault directly
//! between a user's own machines. See RUN-TWO-DESKTOPS.md.

mod agent;
mod commands;
mod config;
mod fetch;
mod fsutil;
mod output;
mod paths;
mod publish;
mod service;
mod syncthing;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "byteferret",
    version,
    about = "P2P document vault agent (peer-to-peer sync via Syncthing)"
)]
struct Cli {
    /// Machine-readable output (one JSON object on stdout)
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start the agent (downloads & runs Syncthing)
    Start,
    /// Stop the agent
    Stop,
    /// Bring a folder into sync (names must be unique; ids are automatic)
    Init {
        path: String,
        /// Attach an existing folder instead of scaffolding (won't overwrite files)
        #[arg(long)]
        existing: bool,
        /// Human-friendly folder name (defaults to the directory name)
        #[arg(long)]
        label: Option<String>,
    },
    /// Pair with another machine (Path A, peer-to-peer)
    Pair {
        /// Pair with a machine by its device ID and share a folder with it
        #[arg(long, value_name = "DEVICE-ID")]
        with: Option<String>,
        /// Approve the named peer's connection, or a folder of theirs with --folder
        #[arg(long)]
        accept: bool,
        /// Decline the named peer's request, or one folder with --folder
        #[arg(long)]
        reject: bool,
        /// Folder id to accept/reject/share; repeat for several
        #[arg(long, value_name = "FOLDER-ID")]
        folder: Vec<String>,
        /// Act on every folder the peer currently offers
        #[arg(long)]
        all_folders: bool,
        /// Where to put a folder taken up from a peer (defaults to a new folder in the current directory)
        #[arg(long, value_name = "DIR")]
        path: Option<String>,
        /// Explicit peer sync address, e.g. tcp://host:22000 (manual/overlay pairing)
        #[arg(long)]
        address: Option<String>,
        /// Friendly name to record for the peer
        #[arg(long)]
        name: Option<String>,
        /// Device id (or an unambiguous prefix) that --accept/--reject applies to
        #[arg(value_name = "DEVICE-ID")]
        id: Option<String>,
    },
    /// Give a device a local alias (usable anywhere a device id is)
    Alias {
        /// Device id, an unambiguous prefix, or an existing alias (omit to list all)
        #[arg(value_name = "DEVICE-ID")]
        device: Option<String>,
        /// The alias to assign (omit to show the current one; omit with --remove)
        alias: Option<String>,
        /// Remove the alias for the named device
        #[arg(long)]
        remove: bool,
    },
    /// Stop sharing a folder — from one machine with --with, or everywhere (files kept)
    Unpair {
        /// Folder name (or an unambiguous prefix) to unpair
        #[arg(value_name = "FOLDER")]
        folder: String,
        /// Unpair from just this machine (device id, prefix, or alias); the folder stays shared with others
        #[arg(long, value_name = "DEVICE")]
        with: Option<String>,
        /// Skip the confirmation prompt when removing the folder from every machine
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// List archived versions of a folder's files (from a paired device's changes)
    History {
        /// Folder name (or an unambiguous prefix) to look in
        #[arg(value_name = "FOLDER")]
        folder: String,
        /// Limit to one file (path relative to the folder, or just its name)
        #[arg(value_name = "FILE")]
        file: Option<String>,
    },
    /// Restore a file to an earlier archived version (the current copy is kept too)
    Restore {
        /// Folder name (or an unambiguous prefix) the file lives in
        #[arg(value_name = "FOLDER")]
        folder: String,
        /// File to restore (path relative to the folder, or just its name)
        #[arg(value_name = "FILE")]
        file: String,
        /// Which version to bring back (a time from `history`); newest if omitted
        #[arg(long, value_name = "TIME")]
        at: Option<String>,
        /// Skip the confirmation prompt
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Show agent health, this device's id, peers, folders, sync state
    Status {
        /// Show full device ids instead of the short first segment
        #[arg(short = 'v', long)]
        verbose: bool,
    },
    /// Print agent and Syncthing versions
    Version,
    /// Diagnose agent health (optionally repair safe issues)
    Doctor {
        /// Apply safe fixes (tighten secrets perms, start a stopped agent)
        #[arg(long)]
        fix: bool,
    },
    /// Show the agent's Syncthing log
    Logs {
        /// Number of trailing lines to show
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: usize,
        /// Follow the log, printing new lines as they arrive
        #[arg(short = 'f', long)]
        follow: bool,
        /// Print the log file path and exit
        #[arg(long)]
        path: bool,
    },
    /// Render a vault document to PDF (and optionally email it)
    Publish {
        /// Path to the Markdown document to render
        file: String,
        /// Render to PDF (the default and only format today)
        #[arg(long)]
        pdf: bool,
        /// Output path (defaults to the source with a .pdf extension)
        #[arg(long)]
        out: Option<String>,
        /// Open a mail draft with the PDF attached (via xdg-email)
        #[arg(long)]
        email: bool,
    },
    /// Manage the user service (systemd/launchd auto-start on login)
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Install and enable the user service
    Install {
        /// Start it immediately as well as enabling it on login
        #[arg(long)]
        now: bool,
    },
    /// Stop, disable, and remove the user service
    Uninstall,
    /// Show whether the service is installed, enabled, and active
    Status,
}

fn main() {
    let cli = Cli::parse();
    output::set_json_mode(cli.json);

    let result = match &cli.cmd {
        Cmd::Start => commands::start::start(),
        Cmd::Stop => commands::stop::stop(),
        Cmd::Init { path, existing, label } => commands::init::init(path, *existing, label.as_deref()),
        Cmd::Pair {
            with,
            accept,
            reject,
            folder,
            all_folders,
            path,
            address,
            name,
            id,
        } => commands::pair::pair(commands::pair::PairArgs {
            with: with.as_deref(),
            accept: *accept,
            reject: *reject,
            target: id.as_deref(),
            address: address.as_deref(),
            name: name.as_deref(),
            folders: folder.clone(),
            all_folders: *all_folders,
            path: path.as_deref(),
        }),
        Cmd::Alias { device, alias, remove } => {
            commands::alias::alias(device.as_deref(), alias.as_deref(), *remove)
        }
        Cmd::Unpair { folder, with, yes } => {
            commands::unpair::unpair(folder, with.as_deref(), *yes)
        }
        Cmd::History { folder, file } => commands::history::history(folder, file.as_deref()),
        Cmd::Restore { folder, file, at, yes } => {
            commands::history::restore(folder, file, at.as_deref(), *yes)
        }
        Cmd::Status { verbose } => commands::status::status(*verbose),
        Cmd::Version => commands::version::version(),
        Cmd::Doctor { fix } => commands::doctor::doctor(*fix),
        Cmd::Logs {
            lines,
            follow,
            path,
        } => commands::logs::logs(*lines, *follow, *path),
        Cmd::Publish {
            file,
            pdf,
            out,
            email,
        } => commands::publish::publish(file, *pdf, out.as_deref(), *email),
        Cmd::Service { action } => match action {
            ServiceAction::Install { now } => commands::service::install(*now),
            ServiceAction::Uninstall => commands::service::uninstall(),
            ServiceAction::Status => commands::service::status(),
        },
    };

    if let Err(e) = result {
        if output::is_json_mode() {
            println!(
                "{}",
                serde_json::json!({ "ok": false, "error": e.to_string() })
            );
        } else {
            eprintln!("\nerror: {e:#}");
        }
        std::process::exit(1);
    }
}
