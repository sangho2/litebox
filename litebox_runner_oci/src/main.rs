// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! OCI-compliant container runtime CLI using LiteBox sandbox.
//!
//! Implements OCI runtime specification commands for running
//! containers through LiteBox's userspace syscall emulation.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use litebox_runner_oci::lifecycle::Lifecycle;
use litebox_runner_oci::state::StateManager;

/// Build version string including git commit hash.
fn version_string() -> &'static str {
    concat!(
        env!("CARGO_PKG_VERSION"),
        " (",
        env!("GIT_HASH"),
        env!("GIT_DIRTY"),
        ")"
    )
}

#[derive(Parser, Debug)]
#[clap(
    name = "litebox-oci",
    about = "OCI container runtime powered by LiteBox"
)]
#[command(version = version_string())]
struct Cli {
    /// Root directory for container state
    #[clap(long, default_value = "/run/litebox-oci")]
    root: PathBuf,

    /// Log file path (accepted for compatibility, logs to stderr)
    #[clap(long)]
    log: Option<PathBuf>,

    /// Log format (accepted for compatibility)
    #[clap(long, default_value = "text")]
    log_format: String,

    #[clap(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Create a container (OCI lifecycle)
    Create {
        /// Path to the OCI bundle directory
        #[clap(short = 'b', long)]
        bundle: PathBuf,

        /// Unique identifier for the container
        container_id: String,

        /// File to write the container PID to
        #[clap(long)]
        pid_file: Option<PathBuf>,

        /// Console socket for terminal (accepted but not implemented)
        #[clap(long)]
        console_socket: Option<PathBuf>,

        /// Don't use pivot_root (accepted, we never pivot anyway)
        #[clap(long)]
        no_pivot: bool,

        /// Don't create new namespaces (accepted, we don't use namespaces)
        #[clap(long)]
        no_new_keyring: bool,
    },

    /// Start a created container (OCI lifecycle)
    Start {
        /// Container ID
        container_id: String,
    },

    /// Query container state (OCI lifecycle)
    State {
        /// Container ID
        container_id: String,
    },

    /// Send a signal to a container (OCI lifecycle)
    Kill {
        /// Container ID
        container_id: String,

        /// Signal to send (default: SIGTERM)
        #[clap(default_value = "SIGTERM")]
        signal: String,

        /// Send signal to all processes (accepted but ignored)
        #[clap(short, long)]
        all: bool,
    },

    /// Delete a container (OCI lifecycle)
    Delete {
        /// Container ID
        container_id: String,

        /// Force deletion even if container is running
        #[clap(short, long)]
        force: bool,
    },

    /// List all containers
    List,

    /// Display container events and statistics
    Events {
        /// Container ID
        container_id: String,

        /// Display stats once and exit
        #[clap(long)]
        stats: bool,

        /// Stats collection interval (ignored, stats are emulated)
        #[clap(long, default_value = "5s")]
        interval: String,
    },

    /// Create and immediately run a container (convenience command)
    Run {
        /// Path to the OCI bundle directory
        #[clap(short, long)]
        bundle: PathBuf,

        /// Container ID
        container_id: String,

        /// Set environment variables (can be specified multiple times)
        #[clap(short, long, value_name = "KEY=VALUE")]
        env: Vec<String>,

        /// Read environment variables from a file (one KEY=VALUE per line)
        #[clap(long, value_name = "FILE")]
        env_file: Option<PathBuf>,

        /// Bind mount a host path into the container (can be specified multiple times)
        /// Format: source=<path>,destination=<path>[,readonly]
        #[clap(short, long, value_name = "MOUNT_SPEC")]
        mount: Vec<String>,

        /// Redirect stdout to a file
        #[clap(long, value_name = "FILE")]
        stdout: Option<PathBuf>,

        /// Redirect stderr to a file
        #[clap(long, value_name = "FILE")]
        stderr: Option<PathBuf>,

        /// TUN device name for container networking (e.g., "tun99").
        /// Requires a pre-configured TUN device on the host.
        /// See litebox_platform_linux_userland/scripts/tun-setup.sh
        #[clap(long, value_name = "DEVICE")]
        tun_device: Option<String>,

        /// Enable lazy file loading using squashfs + loop mount.
        /// Files are loaded on-demand instead of copying entire rootfs to memory.
        /// Requires: mksquashfs, root/sudo for loop mount.
        #[clap(long)]
        lazy: bool,

        /// Enable true lazy loading using tar + layered filesystem.
        /// Only executables are loaded upfront (for rewriting), all other files
        /// are read on-demand from a tar archive. Much faster for large images.
        #[clap(long)]
        lazy_tar: bool,

        /// Enable lazy rewriting with tar + layered filesystem.
        /// Only critical executables (dynamic linker, main binary) are loaded upfront.
        /// Other executables are lazily rewritten on first access. Fastest startup.
        #[clap(long)]
        lazy_rewrite: bool,
    },

    /// Execute a command in a container's rootfs (simplified exec)
    ///
    /// Note: This creates a new sandbox with the same rootfs, it does not
    /// share process state with the running container.
    Exec {
        /// Container ID
        container_id: String,

        /// Set environment variables (can be specified multiple times)
        #[clap(short, long, value_name = "KEY=VALUE")]
        env: Vec<String>,

        /// Read environment variables from a file (one KEY=VALUE per line)
        #[clap(long, value_name = "FILE")]
        env_file: Option<PathBuf>,

        /// Bind mount a host path into the container (can be specified multiple times)
        /// Format: source=<path>,destination=<path>[,readonly]
        #[clap(short, long, value_name = "MOUNT_SPEC")]
        mount: Vec<String>,

        /// TUN device name for container networking (e.g., "tun99").
        /// Requires a pre-configured TUN device on the host.
        #[clap(long, value_name = "DEVICE")]
        tun_device: Option<String>,

        /// Command and arguments to execute
        #[clap(required = true, num_args = 1..)]
        command: Vec<String>,
    },

    /// Show runtime version and features
    Info,
}

/// Parse a signal name or number into a signal number.
fn parse_signal(s: &str) -> Result<i32> {
    // Try parsing as number first
    if let Ok(num) = s.parse::<i32>() {
        return Ok(num);
    }

    // Parse signal name
    let s = s.to_uppercase();
    let s = s.strip_prefix("SIG").unwrap_or(&s);

    match s {
        "TERM" => Ok(libc::SIGTERM),
        "KILL" => Ok(libc::SIGKILL),
        "INT" => Ok(libc::SIGINT),
        "HUP" => Ok(libc::SIGHUP),
        "QUIT" => Ok(libc::SIGQUIT),
        "USR1" => Ok(libc::SIGUSR1),
        "USR2" => Ok(libc::SIGUSR2),
        "STOP" => Ok(libc::SIGSTOP),
        "CONT" => Ok(libc::SIGCONT),
        _ => anyhow::bail!("unknown signal: {s}"),
    }
}

/// Parse environment variables from command line and optional env file.
fn parse_extra_env(env: &[String], env_file: Option<&PathBuf>) -> Result<Vec<String>> {
    let mut extra_env = Vec::new();

    // Parse env file if provided
    if let Some(path) = env_file {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read env file: {}", path.display()))?;

        for line in content.lines() {
            let line = line.trim();
            // Skip empty lines and comments
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // Validate KEY=VALUE format
            if !line.contains('=') {
                anyhow::bail!("invalid env file line (expected KEY=VALUE): {line}");
            }
            extra_env.push(line.to_string());
        }
    }

    // Add command-line env vars (these override file vars)
    for var in env {
        if !var.contains('=') {
            anyhow::bail!("invalid env var (expected KEY=VALUE): {var}");
        }
        extra_env.push(var.clone());
    }

    Ok(extra_env)
}

/// Parse mount specifications into Mount structs.
fn parse_mounts(mount_specs: &[String]) -> Result<Vec<litebox_runner_oci::Mount>> {
    let mut mounts = Vec::new();

    for spec in mount_specs {
        let mut source = None;
        let mut destination = None;
        let mut readonly = false;

        for part in spec.split(',') {
            let part = part.trim();
            if let Some(val) = part.strip_prefix("source=") {
                source = Some(PathBuf::from(val));
            } else if let Some(val) = part.strip_prefix("src=") {
                source = Some(PathBuf::from(val));
            } else if let Some(val) = part.strip_prefix("destination=") {
                destination = Some(val.to_string());
            } else if let Some(val) = part.strip_prefix("dst=") {
                destination = Some(val.to_string());
            } else if let Some(val) = part.strip_prefix("target=") {
                destination = Some(val.to_string());
            } else if part == "readonly" || part == "ro" {
                readonly = true;
            } else if part.starts_with("type=") {
                // Accept but ignore type (we only support bind-like behavior)
            } else if !part.is_empty() {
                anyhow::bail!("unknown mount option: {part}");
            }
        }

        let source = source.ok_or_else(|| anyhow::anyhow!("mount missing source: {spec}"))?;
        let destination =
            destination.ok_or_else(|| anyhow::anyhow!("mount missing destination: {spec}"))?;

        if !source.exists() {
            anyhow::bail!("mount source does not exist: {}", source.display());
        }

        mounts.push(litebox_runner_oci::Mount {
            source,
            destination,
            readonly,
        });
    }

    Ok(mounts)
}

fn main() -> Result<()> {
    // Only enable tracing if RUST_LOG is set
    if std::env::var("RUST_LOG").is_ok() {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::from_default_env()
                    .add_directive("tar_no_std=off".parse().unwrap()),
            )
            .init();
    }

    let cli = Cli::parse();
    let state_manager = StateManager::new(cli.root.clone());
    let lifecycle = Lifecycle::new(state_manager);

    match cli.command {
        Command::Create {
            bundle,
            container_id,
            pid_file,
            console_socket: _, // Accepted but not implemented
            no_pivot: _,       // Accepted, we never pivot anyway
            no_new_keyring: _, // Accepted, we don't use keyrings
        } => {
            tracing::info!(
                container_id = %container_id,
                bundle = %bundle.display(),
                "creating container"
            );

            let state = lifecycle.create(&container_id, &bundle)?;

            // Write PID file if requested
            if let Some(pid_file) = pid_file
                && let Some(pid) = state.pid
            {
                std::fs::write(&pid_file, format!("{pid}"))
                    .with_context(|| format!("failed to write pid file: {}", pid_file.display()))?;
            }

            println!("{}", serde_json::to_string_pretty(&state)?);
            Ok(())
        }

        Command::Start { container_id } => {
            tracing::info!(container_id = %container_id, "starting container");

            let state = lifecycle.start(&container_id)?;
            println!("{}", serde_json::to_string_pretty(&state)?);
            Ok(())
        }

        Command::State { container_id } => {
            let state = lifecycle.state(&container_id)?;
            println!("{}", serde_json::to_string_pretty(&state)?);
            Ok(())
        }

        Command::Kill {
            container_id,
            signal,
            all: _, // Accepted but ignored - we only have one process
        } => {
            let sig = parse_signal(&signal)?;
            tracing::info!(container_id = %container_id, signal = sig, "killing container");

            lifecycle.kill(&container_id, sig)?;
            println!("Signal sent to container {container_id}");
            Ok(())
        }

        Command::Delete {
            container_id,
            force,
        } => {
            tracing::info!(container_id = %container_id, force = force, "deleting container");

            lifecycle.delete(&container_id, force)?;
            println!("Container {container_id} deleted");
            Ok(())
        }

        Command::List => {
            let states = lifecycle.list()?;
            if states.is_empty() {
                println!("No containers");
            } else {
                println!("{:<20} {:<10} {:<10} BUNDLE", "ID", "STATUS", "PID");
                for state in states {
                    println!(
                        "{:<20} {:<10} {:<10} {}",
                        state.id,
                        state.status,
                        state.pid.map(|p| p.to_string()).unwrap_or_default(),
                        state.bundle.display()
                    );
                }
            }
            Ok(())
        }

        Command::Events {
            container_id,
            stats,
            interval: _,
        } => {
            // Verify container exists
            let state = lifecycle.state(&container_id)?;

            // Try to get real stats from /proc if PID is available
            #[allow(clippy::similar_names)]
            let (memory_usage, cpu_user_ns, cpu_sys_ns) = if let Some(pid) = state.pid {
                // Read memory from /proc/[pid]/statm (pages)
                let mem = std::fs::read_to_string(format!("/proc/{pid}/statm"))
                    .ok()
                    .and_then(|s| {
                        s.split_whitespace()
                            .next()
                            .and_then(|v| v.parse::<u64>().ok())
                    })
                    .map_or(0, |pages| pages * 4096); // Convert pages to bytes

                // Read CPU time from /proc/[pid]/stat
                let (utime, stime) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                    .ok()
                    .and_then(|s| {
                        let parts: Vec<&str> = s.split_whitespace().collect();
                        if parts.len() > 14 {
                            let utime = parts[13].parse::<u64>().unwrap_or(0);
                            let stime = parts[14].parse::<u64>().unwrap_or(0);
                            // Convert jiffies to nanoseconds (assuming 100 Hz)
                            Some((utime * 10_000_000, stime * 10_000_000))
                        } else {
                            None
                        }
                    })
                    .unwrap_or((0, 0));

                (mem, utime, stime)
            } else {
                (0, 0, 0)
            };

            // Generate stats JSON matching runc format
            let stats_json = serde_json::json!({
                "type": "stats",
                "id": container_id,
                "data": {
                    "cpu": {
                        "usage": {
                            "total": cpu_user_ns + cpu_sys_ns,
                            "kernel": cpu_sys_ns,
                            "user": cpu_user_ns
                        },
                        "throttling": {}
                    },
                    "memory": {
                        "usage": {
                            "limit": 0,
                            "usage": memory_usage,
                            "max": memory_usage,
                            "failcnt": 0
                        },
                        "swap": {
                            "limit": 0,
                            "usage": 0,
                            "failcnt": 0
                        }
                    },
                    "pids": {
                        "current": i32::from(state.pid.is_some()),
                        "limit": 0
                    },
                    "blkio": {},
                    "hugetlb": {},
                    "intel_rdt": {}
                }
            });

            println!("{stats_json}");

            // If not --stats, we would loop. For now, just exit after one output.
            // Real implementation would loop with interval.
            if !stats {
                // In streaming mode, runc loops forever. We just output once.
                // Container orchestrators typically use --stats for one-shot.
            }

            Ok(())
        }

        Command::Run {
            bundle,
            container_id,
            env,
            env_file,
            mount,
            stdout,
            stderr,
            tun_device,
            lazy,
            lazy_tar,
            lazy_rewrite,
        } => {
            tracing::info!(
                container_id = %container_id,
                bundle = %bundle.display(),
                tun_device = ?tun_device,
                lazy = lazy,
                lazy_tar = lazy_tar,
                lazy_rewrite = lazy_rewrite,
                "running container"
            );

            let bundle = bundle
                .canonicalize()
                .with_context(|| format!("bundle path not found: {}", bundle.display()))?;

            // Set up stdio redirection if requested
            let stdio = litebox_runner_oci::StdioRedirect {
                stdout: stdout.clone(),
                stderr: stderr.clone(),
            };

            // Set up network configuration
            let network = litebox_runner_oci::NetworkConfig {
                tun_device: tun_device.clone(),
            };

            let extra_env = parse_extra_env(&env, env_file.as_ref())?;
            let mounts = parse_mounts(&mount)?;

            let exit_code = if lazy_rewrite {
                litebox_runner_oci::run_container_lazy_rewrite(
                    &bundle, None, &extra_env, &mounts, &stdio, &network,
                )?
            } else if lazy_tar {
                litebox_runner_oci::run_container_lazy_tar(
                    &bundle, None, &extra_env, &mounts, &stdio, &network,
                )?
            } else if lazy {
                litebox_runner_oci::run_container_lazy(
                    &bundle, None, &extra_env, &mounts, &stdio, &network,
                )?
            } else {
                litebox_runner_oci::run_container_full(
                    &bundle, None, &extra_env, &mounts, &stdio, &network,
                )?
            };
            std::process::exit(exit_code);
        }

        Command::Exec {
            container_id,
            env,
            env_file,
            mount,
            tun_device,
            command,
        } => {
            tracing::info!(
                container_id = %container_id,
                command = ?command,
                tun_device = ?tun_device,
                "exec in container"
            );

            // Load container state to get bundle path
            let state = lifecycle.state(&container_id)?;

            // Container must exist (any status is fine for exec)
            let bundle = state.bundle;

            // Set up network configuration
            let network = litebox_runner_oci::NetworkConfig {
                tun_device: tun_device.clone(),
            };

            let extra_env = parse_extra_env(&env, env_file.as_ref())?;
            let mounts = parse_mounts(&mount)?;
            // Run with overridden command, extra env, mounts, and networking
            let exit_code = litebox_runner_oci::run_container_full(
                &bundle,
                Some(&command),
                &extra_env,
                &mounts,
                &litebox_runner_oci::StdioRedirect::default(),
                &network,
            )?;
            std::process::exit(exit_code);
        }

        Command::Info => {
            println!("litebox-oci - OCI container runtime powered by LiteBox");
            println!("Version: {}", version_string());
            println!();
            println!("Features:");
            println!("  - Userspace syscall emulation via LiteBox");
            println!("  - In-memory filesystem isolation");
            println!("  - Syscall rewriting for interception");
            println!("  - TUN-based networking (--tun-device)");
            println!();
            println!("OCI Lifecycle Commands:");
            println!("  create  - Create a container");
            println!("  start   - Start a created container");
            println!("  state   - Query container state");
            println!("  kill    - Send signal to container");
            println!("  delete  - Delete a container");
            println!("  list    - List all containers");
            println!();
            println!("Convenience Commands:");
            println!("  run     - Create and run a container directly");
            println!("  exec    - Run a command in a container's rootfs");
            println!("  info    - Show this information");
            Ok(())
        }
    }
}
