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

#[derive(Parser, Debug)]
#[clap(
    name = "litebox-oci",
    about = "OCI container runtime powered by LiteBox"
)]
#[command(version)]
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

    /// Create and immediately run a container (convenience command)
    Run {
        /// Path to the OCI bundle directory
        #[clap(short, long)]
        bundle: PathBuf,

        /// Container ID
        container_id: String,
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

        Command::Run {
            bundle,
            container_id,
        } => {
            tracing::info!(
                container_id = %container_id,
                bundle = %bundle.display(),
                "running container"
            );

            let bundle = bundle
                .canonicalize()
                .with_context(|| format!("bundle path not found: {}", bundle.display()))?;

            let exit_code = litebox_runner_oci::run_container(&bundle)?;
            std::process::exit(exit_code);
        }

        Command::Info => {
            println!("litebox-oci - OCI container runtime powered by LiteBox");
            println!("Version: {}", env!("CARGO_PKG_VERSION"));
            println!();
            println!("Features:");
            println!("  - Userspace syscall emulation via LiteBox");
            println!("  - In-memory filesystem isolation");
            println!("  - Syscall rewriting for interception");
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
            println!("  info    - Show this information");
            Ok(())
        }
    }
}
