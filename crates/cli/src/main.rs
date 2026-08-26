//! pereprava CLI entry point.

mod bench;
mod bundle;
mod commands;
mod doctor;
mod format;
mod mountcmd;
mod progress;

use clap::{Parser, Subcommand};

/// Modern async MTP client for macOS — Android <-> Mac file transfer.
#[derive(Parser)]
#[command(name = "pereprava", version, about)]
struct Cli {
    /// Enable verbose logging (RUST_LOG style filtering).
    #[arg(long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Diagnose device visibility and common blockers (ptpcamerad, AFT, adb).
    Doctor,
    /// Show device and storage summary.
    Info,
    /// List a device directory (`/` lists storages).
    Ls {
        /// Device path, e.g. "/Internal shared storage/DCIM".
        #[arg(default_value_t = String::from("/"))]
        path: String,
    },
    /// Download a file or directory from the phone.
    Pull {
        /// Remote path to download.
        remote: String,
        /// Local destination (default: current directory).
        local: Option<String>,
    },
    /// Upload a file or directory to the phone.
    Push {
        /// Local source path.
        local: String,
        /// Remote parent directory (default: /1 — first storage root).
        remote: Option<String>,
        /// Replace an existing remote file instead of failing.
        #[arg(short = 'f', long)]
        force: bool,
    },
    /// Create a directory on the phone (parents included).
    Mkdir { path: String },
    /// Delete a file or directory on the phone.
    Rm {
        path: String,
        /// Delete directories recursively.
        #[arg(short = 'r', long)]
        recursive: bool,
    },
    /// Move/rename on the phone.
    Mv {
        /// Source device path.
        from: String,
        /// Destination directory or full new path.
        to: String,
    },
    /// Throughput micro-benchmarks (artifacts are cleaned up).
    Bench {
        /// Big-file size in MiB (0 disables the phase).
        #[arg(long, default_value_t = 64)]
        size_mib: u64,
        /// Small-file count (0 disables the phase).
        #[arg(long, default_value_t = 200)]
        small_files: u32,
        /// Also push the small-file tree as one .tar.zst bundle and compare.
        #[arg(long)]
        bundle: bool,
    },
    /// Upload a directory as a single `.tar.zst` object (fast for many files).
    Pack {
        /// Local directory to pack.
        local: String,
        /// Remote parent directory (default: /1).
        remote: Option<String>,
    },
    /// Download and extract a `.tar.zst` object from the phone.
    Unpack {
        /// Remote path to the archive.
        archive: String,
        /// Local extraction target directory.
        dest: String,
    },
    /// Mount the phone in Finder as a read-only NFS volume (Ctrl-C to eject).
    Mount {
        /// Mount point (default /Volumes/pereprava).
        #[arg(long, default_value = "/Volumes/pereprava")]
        path: String,
        /// Local TCP port for the loopback NFS server.
        #[arg(long, default_value_t = 34567)]
        port: u16,
        /// Debug: serve NFS without invoking mount_nfs.
        #[arg(long)]
        serve_only: bool,
        /// Debug/test: accept clients from unprivileged source ports.
        #[arg(long)]
        allow_unprivileged_source_port: bool,
        /// Export subpath: "/" = all storages, "/1" = first storage root.
        #[arg(long, default_value = "/")]
        export: String,
        /// Force read-only even when the device storage is writable.
        #[arg(long)]
        read_only: bool,
    },
    /// Detach a previously mounted pereprava volume.
    Unmount {
        /// Mount point.
        #[arg(long, default_value = "/Volumes/pereprava")]
        path: String,
    },
    /// Daemon: auto-mount whenever a phone appears (for LaunchAgent).
    Watch {
        /// Mount point.
        #[arg(long, default_value = "/Volumes/pereprava")]
        path: String,
        /// Local TCP port for the loopback NFS server.
        #[arg(long, default_value_t = 34567)]
        port: u16,
        /// Force read-only even when the device storage is writable.
        #[arg(long)]
        read_only: bool,
        /// Polling interval in seconds.
        #[arg(long, default_value_t = 3)]
        poll_secs: u64,
    },
}

fn init_logging(verbose: bool) {
    use tracing_subscriber::EnvFilter;
    // RUST_LOG wins over the -v default so `fernfs` internals can be traced.
    let filter = std::env::var("RUST_LOG")
        .map(EnvFilter::new)
        .unwrap_or_else(|_| {
            if verbose {
                EnvFilter::new("pereprava=trace,mtp_rs=info,nusb=info,fernfs=debug")
            } else {
                EnvFilter::new("pereprava=warn")
            }
        });
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    init_logging(cli.verbose);

    let result = match cli.cmd {
        Cmd::Doctor => doctor::run().await,
        Cmd::Info => commands::info().await,
        Cmd::Ls { path } => commands::ls(&path).await,
        Cmd::Pull { remote, local } => commands::pull(remote, local).await,
        Cmd::Push {
            local,
            remote,
            force,
        } => commands::push(local, remote, force).await,
        Cmd::Mkdir { path } => commands::mkdir(path).await,
        Cmd::Rm { path, recursive } => commands::rm(path, recursive).await,
        Cmd::Mv { from, to } => commands::mv(from, to).await,
        Cmd::Bench {
            size_mib,
            small_files,
            bundle,
        } => {
            bench::run(bench::Params {
                size_mib,
                small_files,
                bundle,
            })
            .await
        }
        Cmd::Pack { local, remote } => {
            let p = std::path::PathBuf::from(&local);
            commands::with_device(move |dev| async move {
                let remote_parent = remote.unwrap_or_else(|| "/1".to_string());
                let stats = bundle::push_as_bundle(&dev, &p, &remote_parent).await?;
                println!(
                    "packed {} file(s), {} dir(s), {} -> {} ({:.2}x smaller) in {:.2}s",
                    stats.files,
                    stats.dirs,
                    format::human_bytes(stats.raw_bytes),
                    format::human_bytes(stats.packed_bytes),
                    stats.raw_bytes as f64 / stats.packed_bytes.max(1) as f64,
                    stats.elapsed_ms as f64 / 1000.0
                );
                Ok(())
            })
            .await
        }
        Cmd::Unpack { archive, dest } => {
            let d = std::path::PathBuf::from(&dest);
            commands::with_device(move |dev| async move {
                let stats = bundle::pull_bundle(&dev, &archive, &d).await?;
                println!(
                    "extracted {} file(s) {} (from {}) into {} in {:.2}s",
                    stats.files,
                    format::human_bytes(stats.raw_bytes),
                    format::human_bytes(stats.packed_bytes),
                    d.display(),
                    stats.elapsed_ms as f64 / 1000.0
                );
                Ok(())
            })
            .await
        }
        Cmd::Mount {
            path,
            port,
            serve_only,
            allow_unprivileged_source_port,
            export,
            read_only,
        } => {
            mountcmd::run(
                std::path::PathBuf::from(path),
                port,
                serve_only,
                allow_unprivileged_source_port,
                export,
                read_only,
            )
            .await
        }
        Cmd::Unmount { path } => mountcmd::detach(std::path::PathBuf::from(path)).await,
        Cmd::Watch {
            path,
            port,
            read_only,
            poll_secs,
        } => mountcmd::watch(std::path::PathBuf::from(path), port, read_only, poll_secs).await,
    };

    if let Err(e) = result {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
