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

/// Set up a PTY and send the master fd over a console socket.
///
/// Creates a new pseudoterminal pair. Sends the master fd to the
/// console-socket via SCM_RIGHTS (as runc does per OCI spec).
/// Returns the slave fd which should be used for container stdio.
fn setup_console_socket(console_socket_path: &Path) -> Result<std::os::fd::OwnedFd> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    #[repr(C)]
    struct CmsgFd {
        hdr: libc::cmsghdr,
        fd: i32,
    }

    // Open a new PTY master via posix_openpt
    let master_fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    if master_fd < 0 {
        anyhow::bail!("posix_openpt failed: {}", std::io::Error::last_os_error());
    }
    // SAFETY: master_fd is a valid fd returned by posix_openpt
    let master = unsafe { OwnedFd::from_raw_fd(master_fd) };

    if unsafe { libc::grantpt(master_fd) } != 0 {
        anyhow::bail!("grantpt failed: {}", std::io::Error::last_os_error());
    }
    if unsafe { libc::unlockpt(master_fd) } != 0 {
        anyhow::bail!("unlockpt failed: {}", std::io::Error::last_os_error());
    }

    // Get slave path
    let slave_name = unsafe { libc::ptsname(master_fd) };
    if slave_name.is_null() {
        anyhow::bail!("ptsname failed: {}", std::io::Error::last_os_error());
    }
    // SAFETY: ptsname returned a valid C string
    let slave_path = unsafe { std::ffi::CStr::from_ptr(slave_name) };
    let slave_fd = unsafe { libc::open(slave_path.as_ptr(), libc::O_RDWR) };
    if slave_fd < 0 {
        anyhow::bail!(
            "failed to open PTY slave: {}",
            std::io::Error::last_os_error()
        );
    }
    // SAFETY: slave_fd is a valid fd returned by open
    let slave = unsafe { OwnedFd::from_raw_fd(slave_fd) };

    // Connect to the console-socket and send master fd via SCM_RIGHTS
    let sock = UnixStream::connect(console_socket_path).with_context(|| {
        format!(
            "failed to connect to console-socket: {}",
            console_socket_path.display()
        )
    })?;

    // Send master fd using sendmsg + SCM_RIGHTS
    let sock_fd = sock.as_raw_fd();
    let data: [u8; 1] = [0];
    let iov = libc::iovec {
        iov_base: data.as_ptr() as *mut libc::c_void,
        iov_len: 1,
    };
    #[allow(clippy::cast_possible_truncation)]
    let mut cmsg_buf = CmsgFd {
        hdr: libc::cmsghdr {
            cmsg_len: unsafe { libc::CMSG_LEN(std::mem::size_of::<i32>() as u32) } as _,
            cmsg_level: libc::SOL_SOCKET,
            cmsg_type: libc::SCM_RIGHTS,
        },
        fd: master.as_raw_fd(),
    };
    let msg = libc::msghdr {
        msg_name: std::ptr::null_mut(),
        msg_namelen: 0,
        msg_iov: (&raw const iov).cast_mut(),
        msg_iovlen: 1,
        msg_control: (&raw mut cmsg_buf).cast::<libc::c_void>(),
        msg_controllen: std::mem::size_of::<CmsgFd>(),
        msg_flags: 0,
    };
    let ret = unsafe { libc::sendmsg(sock_fd, &raw const msg, 0) };
    if ret < 0 {
        anyhow::bail!(
            "sendmsg (SCM_RIGHTS) failed: {}",
            std::io::Error::last_os_error()
        );
    }

    // Close the master fd and socket — orchestrator now owns the master
    drop(master);
    drop(sock);

    Ok(slave)
}

/// Check if the OCI spec requests a new network namespace (type=network, no path).
/// When true, the child process should unshare into a new netns so that container
/// managers can apply CNI plugins to the child's /proc/<pid>/ns/net.
fn should_unshare_netns(spec: &oci_spec::runtime::Spec) -> bool {
    use oci_spec::runtime::LinuxNamespaceType;
    spec.linux()
        .as_ref()
        .and_then(|l| l.namespaces().as_ref())
        .is_some_and(|namespaces| {
            namespaces
                .iter()
                .any(|ns| ns.typ() == LinuxNamespaceType::Network && ns.path().is_none())
        })
}

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
    pub fn create(
        &self,
        id: &str,
        bundle: &Path,
        console_socket: Option<&Path>,
        extra_run_args: &[String],
    ) -> Result<ContainerState> {
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

                // If the OCI spec requests a new network namespace (no path), unshare
                // into one so that container managers (e.g., ctr --cni) can apply CNI
                // plugins to our netns via /proc/<pid>/ns/net.
                if let Ok(spec_data) = fs::read(&config_path)
                    && let Ok(spec) = serde_json::from_slice::<oci_spec::runtime::Spec>(&spec_data)
                    && should_unshare_netns(&spec)
                {
                    // SAFETY: unshare is a standard Linux syscall.
                    let ret = unsafe { libc::unshare(libc::CLONE_NEWNET) };
                    if ret == 0 {
                        // Bring up loopback in the new netns
                        let _ = std::process::Command::new("ip")
                            .args(["link", "set", "lo", "up"])
                            .status();
                    }
                }

                // Create a listener socket that start() will connect to
                let listener =
                    UnixListener::bind(&sync_path).expect("child: failed to bind sync socket");

                // Set up PTY before signaling ready — orchestrators (conmon, containerd)
                // expect the console-socket connection to complete during `create`.
                if let Some(cs_path) = console_socket {
                    use std::os::fd::AsRawFd;
                    match setup_console_socket(cs_path) {
                        Ok(slave_fd) => {
                            let raw = slave_fd.as_raw_fd();
                            // Dup slave fd onto stdin/stdout/stderr
                            unsafe {
                                libc::setsid();
                                libc::dup2(raw, 0); // stdin
                                libc::dup2(raw, 1); // stdout
                                libc::dup2(raw, 2); // stderr
                                libc::ioctl(raw, libc::TIOCSCTTY, 0);
                            }
                            drop(slave_fd); // Close the original fd (duped onto 0/1/2)
                        }
                        Err(e) => {
                            eprintln!("child: console-socket setup failed: {e}");
                            std::process::exit(1);
                        }
                    }
                }

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
                    let mut cmd = exec::Command::new(&exe);
                    cmd.arg("run").arg("--bundle").arg(&bundle);
                    for arg in extra_run_args {
                        cmd.arg(arg);
                    }
                    cmd.arg(id);
                    let err = cmd.exec();

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
        let mut stream = UnixStream::connect(&sync_path).with_context(|| {
            format!(
                "failed to connect to container sync socket at {}. \
                 The container process may have exited unexpectedly. \
                 Try deleting and recreating the container.",
                sync_path.display()
            )
        })?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::StateManager;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn create_temp_lifecycle() -> (TempDir, Lifecycle) {
        let temp_dir = TempDir::new().unwrap();
        let state_manager = StateManager::new(temp_dir.path().to_path_buf());
        let lifecycle = Lifecycle::new(state_manager);
        (temp_dir, lifecycle)
    }

    fn create_valid_bundle(temp_dir: &TempDir) -> PathBuf {
        let bundle_dir = temp_dir.path().join("bundle");
        fs::create_dir_all(&bundle_dir).unwrap();

        // Create minimal config.json
        let config = r#"{
            "ociVersion": "1.0.0",
            "root": { "path": "rootfs" },
            "process": {
                "args": ["/bin/echo", "hello"],
                "cwd": "/"
            }
        }"#;
        fs::write(bundle_dir.join("config.json"), config).unwrap();

        // Create rootfs directory
        fs::create_dir_all(bundle_dir.join("rootfs")).unwrap();

        bundle_dir
    }

    #[test]
    fn test_lifecycle_new() {
        let temp_dir = TempDir::new().unwrap();
        let state_manager = StateManager::new(temp_dir.path().to_path_buf());
        let _lifecycle = Lifecycle::new(state_manager);
        // Constructor should work without error
    }

    #[test]
    fn test_create_bundle_not_found() {
        let (_temp, lifecycle) = create_temp_lifecycle();

        let result = lifecycle.create("test-id", Path::new("/nonexistent/bundle"), None, &[]);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("bundle not found"));
    }

    #[test]
    fn test_create_config_not_found() {
        let (temp, lifecycle) = create_temp_lifecycle();

        // Create bundle dir without config.json
        let bundle_dir = temp.path().join("bundle");
        fs::create_dir_all(&bundle_dir).unwrap();

        let result = lifecycle.create("test-id", &bundle_dir, None, &[]);

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("config.json not found")
        );
    }

    #[test]
    fn test_create_already_exists() {
        let (temp, lifecycle) = create_temp_lifecycle();
        let bundle = create_valid_bundle(&temp);

        // Pre-create a container state
        let state = ContainerState::new("test-id".to_string(), bundle.clone());
        lifecycle.state_manager.save(&state).unwrap();

        let result = lifecycle.create("test-id", &bundle, None, &[]);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[test]
    fn test_start_not_found() {
        let (_temp, lifecycle) = create_temp_lifecycle();

        let result = lifecycle.start("nonexistent");

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_start_wrong_status_running() {
        let (temp, lifecycle) = create_temp_lifecycle();
        let bundle = create_valid_bundle(&temp);

        // Create a container in Running status (can't start again)
        let mut state = ContainerState::new("test-id".to_string(), bundle);
        state.status = Status::Running;
        state.pid = Some(12345);
        lifecycle.state_manager.save(&state).unwrap();

        let result = lifecycle.start("test-id");

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("cannot start"));
        assert!(err.contains("running"));
    }

    #[test]
    fn test_start_wrong_status_stopped() {
        let (temp, lifecycle) = create_temp_lifecycle();
        let bundle = create_valid_bundle(&temp);

        // Create a container in Stopped status
        let mut state = ContainerState::new("test-id".to_string(), bundle);
        state.status = Status::Stopped;
        lifecycle.state_manager.save(&state).unwrap();

        let result = lifecycle.start("test-id");

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("cannot start"));
        assert!(err.contains("stopped"));
    }

    #[test]
    fn test_state_not_found() {
        let (_temp, lifecycle) = create_temp_lifecycle();

        let result = lifecycle.state("nonexistent");

        assert!(result.is_err());
    }

    #[test]
    fn test_state_returns_container_state() {
        let (temp, lifecycle) = create_temp_lifecycle();
        let bundle = create_valid_bundle(&temp);

        let mut state = ContainerState::new("test-id".to_string(), bundle);
        state.status = Status::Created;
        state.pid = Some(999);
        lifecycle.state_manager.save(&state).unwrap();

        let result = lifecycle.state("test-id").unwrap();

        assert_eq!(result.id, "test-id");
        assert_eq!(result.status, Status::Created);
        assert_eq!(result.pid, Some(999));
    }

    #[test]
    fn test_kill_not_found() {
        let (_temp, lifecycle) = create_temp_lifecycle();

        let result = lifecycle.kill("nonexistent", libc::SIGTERM);

        assert!(result.is_err());
    }

    #[test]
    fn test_kill_already_stopped() {
        let (temp, lifecycle) = create_temp_lifecycle();
        let bundle = create_valid_bundle(&temp);

        let mut state = ContainerState::new("test-id".to_string(), bundle);
        state.status = Status::Stopped;
        lifecycle.state_manager.save(&state).unwrap();

        let result = lifecycle.kill("test-id", libc::SIGTERM);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already stopped"));
    }

    #[test]
    fn test_kill_no_pid() {
        let (temp, lifecycle) = create_temp_lifecycle();
        let bundle = create_valid_bundle(&temp);

        let mut state = ContainerState::new("test-id".to_string(), bundle);
        state.status = Status::Running;
        state.pid = None; // No PID
        lifecycle.state_manager.save(&state).unwrap();

        let result = lifecycle.kill("test-id", libc::SIGTERM);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no PID"));
    }

    #[test]
    fn test_delete_not_found() {
        let (_temp, lifecycle) = create_temp_lifecycle();

        let result = lifecycle.delete("nonexistent", false);

        assert!(result.is_err());
    }

    #[test]
    fn test_delete_stopped_container() {
        let (temp, lifecycle) = create_temp_lifecycle();
        let bundle = create_valid_bundle(&temp);

        let mut state = ContainerState::new("test-id".to_string(), bundle);
        state.status = Status::Stopped;
        lifecycle.state_manager.save(&state).unwrap();

        let result = lifecycle.delete("test-id", false);

        assert!(result.is_ok());
        assert!(!lifecycle.state_manager.exists("test-id"));
    }

    #[test]
    fn test_delete_running_without_force_fails() {
        let (temp, lifecycle) = create_temp_lifecycle();
        let bundle = create_valid_bundle(&temp);

        let mut state = ContainerState::new("test-id".to_string(), bundle);
        state.status = Status::Running;
        state.pid = Some(std::process::id()); // Use current process so it appears alive
        lifecycle.state_manager.save(&state).unwrap();

        let result = lifecycle.delete("test-id", false);

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("cannot delete"));
        assert!(err.contains("--force"));
    }

    #[test]
    fn test_list_empty() {
        let (_temp, lifecycle) = create_temp_lifecycle();

        let states = lifecycle.list().unwrap();

        assert!(states.is_empty());
    }

    #[test]
    fn test_list_multiple_containers() {
        let (temp, lifecycle) = create_temp_lifecycle();
        let bundle = create_valid_bundle(&temp);

        // Create multiple containers
        for i in 1..=3 {
            let mut state = ContainerState::new(format!("container-{i}"), bundle.clone());
            state.status = Status::Stopped;
            lifecycle.state_manager.save(&state).unwrap();
        }

        let mut states = lifecycle.list().unwrap();
        states.sort_by(|a, b| a.id.cmp(&b.id));

        assert_eq!(states.len(), 3);
        assert_eq!(states[0].id, "container-1");
        assert_eq!(states[1].id, "container-2");
        assert_eq!(states[2].id, "container-3");
    }

    #[test]
    fn test_list_skips_corrupted_state() {
        let (temp, lifecycle) = create_temp_lifecycle();
        let bundle = create_valid_bundle(&temp);

        // Create a valid container
        let state = ContainerState::new("valid-container".to_string(), bundle);
        lifecycle.state_manager.save(&state).unwrap();

        // Create a corrupted container (directory exists but state.json is invalid)
        let corrupted_dir = temp.path().join("containers").join("corrupted-container");
        fs::create_dir_all(&corrupted_dir).unwrap();
        fs::write(corrupted_dir.join("state.json"), "not valid json").unwrap();

        let states = lifecycle.list().unwrap();

        // Should only return the valid container
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].id, "valid-container");
    }
}
