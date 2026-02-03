// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! OCI container lifecycle management.
//!
//! Implements the OCI runtime lifecycle: create, start, state, kill, delete.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

use anyhow::{Context, Result};

use crate::state::{ContainerState, StateManager, Status};

/// OCI lifecycle manager.
pub struct Lifecycle {
    state_manager: StateManager,
}

impl Lifecycle {
    /// Create a new lifecycle manager.
    pub fn new(state_manager: StateManager) -> Self {
        Self { state_manager }
    }

    /// Create a container without starting it.
    ///
    /// This spawns a child process that waits for the start signal via a Unix socket.
    ///
    /// # Panics
    ///
    /// This function may panic in the child process if:
    /// - Failed to bind the sync socket
    /// - Failed to signal ready to parent
    /// - Failed to accept start connection
    /// - Failed to read start signal
    pub fn create(&self, id: &str, bundle: &Path) -> Result<ContainerState> {
        // Validate bundle
        let bundle = bundle
            .canonicalize()
            .with_context(|| format!("bundle not found: {}", bundle.display()))?;

        let config_path = bundle.join("config.json");
        if !config_path.exists() {
            anyhow::bail!("config.json not found in bundle: {}", bundle.display());
        }

        // Check if container already exists
        if self.state_manager.exists(id) {
            anyhow::bail!("container {id} already exists");
        }

        // Create state directory
        let state_dir = self.state_manager.create_dir(id)?;

        // Create a pair of connected sockets for parent-child sync
        let (parent_sock, child_sock) =
            UnixStream::pair().context("failed to create socket pair")?;

        // Create sync socket that the child will listen on for start command
        let sync_path = self.state_manager.sync_pipe(id);

        // Get path to current executable
        let exe = std::env::current_exe().context("failed to get current executable")?;

        // Fork using nix for proper control
        match unsafe { nix::unistd::fork() } {
            Ok(nix::unistd::ForkResult::Parent { child }) => {
                // Parent process
                drop(child_sock); // Close child's end
                let pid = child.as_raw().cast_unsigned();

                // Wait for child to signal it's ready
                let mut parent_sock = parent_sock;
                let mut buf = [0u8; 1];
                parent_sock
                    .read_exact(&mut buf)
                    .context("failed to read sync from child")?;

                if buf[0] != b'R' {
                    anyhow::bail!("child process failed to initialize");
                }

                // Save state
                let mut state = ContainerState::new(id.to_string(), bundle);
                state.status = Status::Created;
                state.pid = Some(pid);
                self.state_manager.save(&state)?;

                Ok(state)
            }
            Ok(nix::unistd::ForkResult::Child) => {
                // Child process
                drop(parent_sock); // Close parent's end
                let mut child_sock = child_sock;

                // Create a listener socket that start() will connect to
                let listener =
                    UnixListener::bind(&sync_path).expect("child: failed to bind sync socket");

                // Signal parent we're ready
                child_sock
                    .write_all(b"R")
                    .expect("child: failed to signal ready");
                drop(child_sock); // Done with parent sync

                // Wait for start command to connect
                let (mut stream, _) = listener
                    .accept()
                    .expect("child: failed to accept start connection");

                // Wait for start signal
                let mut buf = [0u8; 1];
                stream
                    .read_exact(&mut buf)
                    .expect("child: failed to read start signal");

                if buf[0] == b'S' {
                    // Clean up listener before exec
                    drop(listener);
                    let _ = fs::remove_file(&sync_path);

                    // Execute the container
                    let err = exec::Command::new(&exe)
                        .arg("run")
                        .arg("--bundle")
                        .arg(&bundle)
                        .arg(id)
                        .exec();

                    eprintln!("child: exec failed: {err}");
                    std::process::exit(1);
                } else {
                    // Killed before start
                    std::process::exit(0);
                }
            }
            Err(e) => {
                // Clean up state directory on fork failure
                let _ = fs::remove_dir_all(&state_dir);
                anyhow::bail!("fork failed: {e}");
            }
        }
    }

    /// Start a created container.
    pub fn start(&self, id: &str) -> Result<ContainerState> {
        let state = self.state_manager.load(id)?;

        if state.status != Status::Created {
            anyhow::bail!(
                "cannot start container {}: status is {} (expected created)",
                id,
                state.status
            );
        }

        // Connect to the child's sync socket and signal it to start
        let sync_path = self.state_manager.sync_pipe(id);
        let mut stream = UnixStream::connect(&sync_path)
            .context("failed to connect to container sync socket")?;

        stream
            .write_all(b"S")
            .context("failed to send start signal")?;
        drop(stream);

        // Update state
        let state = self.state_manager.update(id, |s| {
            s.status = Status::Running;
        })?;

        Ok(state)
    }

    /// Get the state of a container.
    pub fn state(&self, id: &str) -> Result<ContainerState> {
        self.state_manager.refresh_state(id)
    }

    /// Send a signal to a container.
    pub fn kill(&self, id: &str, signal: i32) -> Result<()> {
        let state = self.state_manager.refresh_state(id)?;

        if state.status == Status::Stopped {
            anyhow::bail!("container {id} is already stopped");
        }

        let pid = state.pid.context("container has no PID")?;

        let result = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid.cast_signed()),
            nix::sys::signal::Signal::try_from(signal).ok(),
        );

        if let Err(e) = result {
            anyhow::bail!("failed to send signal {signal} to process {pid}: {e}");
        }

        // Check if process exited
        std::thread::sleep(std::time::Duration::from_millis(100));
        if !StateManager::is_process_alive(pid) {
            self.state_manager.update(id, |s| {
                s.status = Status::Stopped;
            })?;
        }

        Ok(())
    }

    /// Delete a container.
    pub fn delete(&self, id: &str, force: bool) -> Result<()> {
        let state = self.state_manager.refresh_state(id)?;

        match state.status {
            Status::Stopped => {
                // OK to delete
            }
            Status::Created => {
                // Kill the waiting process first
                if let Some(pid) = state.pid {
                    let _ = nix::sys::signal::kill(
                        nix::unistd::Pid::from_raw(pid.cast_signed()),
                        nix::sys::signal::Signal::SIGKILL,
                    );
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
            Status::Running if force => {
                // Force kill
                if let Some(pid) = state.pid {
                    let _ = nix::sys::signal::kill(
                        nix::unistd::Pid::from_raw(pid.cast_signed()),
                        nix::sys::signal::Signal::SIGKILL,
                    );
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
            _ => {
                anyhow::bail!(
                    "cannot delete container {}: status is {} (must be stopped, or use --force)",
                    id,
                    state.status
                );
            }
        }

        self.state_manager.delete(id)?;
        Ok(())
    }

    /// List all containers.
    pub fn list(&self) -> Result<Vec<ContainerState>> {
        let ids = self.state_manager.list()?;
        let mut states = Vec::new();
        for id in ids {
            match self.state_manager.refresh_state(&id) {
                Ok(state) => states.push(state),
                Err(e) => {
                    tracing::warn!(container = %id, error = %e, "failed to load container state");
                }
            }
        }
        Ok(states)
    }
}
