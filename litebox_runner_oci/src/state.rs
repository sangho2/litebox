// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Container state management for OCI lifecycle.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Container status per OCI runtime spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Container is being created
    Creating,
    /// Container has been created but not started
    Created,
    /// Container is running
    Running,
    /// Container has exited
    Stopped,
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Status::Creating => write!(f, "creating"),
            Status::Created => write!(f, "created"),
            Status::Running => write!(f, "running"),
            Status::Stopped => write!(f, "stopped"),
        }
    }
}

/// Container state per OCI runtime spec.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerState {
    /// OCI version
    #[serde(rename = "ociVersion")]
    pub oci_version: String,

    /// Container ID
    pub id: String,

    /// Current status
    pub status: Status,

    /// PID of the container's init process (if running)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,

    /// Path to the OCI bundle
    pub bundle: PathBuf,

    /// Optional annotations
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub annotations: std::collections::HashMap<String, String>,
}

impl ContainerState {
    /// Create a new container state.
    pub fn new(id: String, bundle: PathBuf) -> Self {
        Self {
            oci_version: "1.0.0".to_string(),
            id,
            status: Status::Creating,
            pid: None,
            bundle,
            annotations: std::collections::HashMap::new(),
        }
    }
}

/// Manages container state on disk.
pub struct StateManager {
    root: PathBuf,
}

impl StateManager {
    /// Create a new state manager with the given root directory.
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Get the state directory for a container.
    pub fn container_dir(&self, id: &str) -> PathBuf {
        self.root.join("containers").join(id)
    }

    /// Get the state file path for a container.
    fn state_file(&self, id: &str) -> PathBuf {
        self.container_dir(id).join("state.json")
    }

    /// Get the sync pipe path for a container.
    pub fn sync_pipe(&self, id: &str) -> PathBuf {
        self.container_dir(id).join("sync.pipe")
    }

    /// Check if a container exists.
    pub fn exists(&self, id: &str) -> bool {
        self.state_file(id).exists()
    }

    /// List all container IDs.
    pub fn list(&self) -> Result<Vec<String>> {
        let containers_dir = self.root.join("containers");
        if !containers_dir.exists() {
            return Ok(Vec::new());
        }

        let mut ids = Vec::new();
        for entry in fs::read_dir(&containers_dir)
            .with_context(|| format!("failed to read {}", containers_dir.display()))?
        {
            let entry = entry?;
            if entry.file_type()?.is_dir()
                && let Some(name) = entry.file_name().to_str()
            {
                ids.push(name.to_string());
            }
        }
        Ok(ids)
    }

    /// Create state directory for a new container.
    pub fn create_dir(&self, id: &str) -> Result<PathBuf> {
        let dir = self.container_dir(id);
        if dir.exists() {
            anyhow::bail!("container {id} already exists");
        }
        fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create state directory: {}", dir.display()))?;
        Ok(dir)
    }

    /// Save container state to disk.
    pub fn save(&self, state: &ContainerState) -> Result<()> {
        let path = self.state_file(&state.id);

        // Ensure directory exists
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let json = serde_json::to_string_pretty(state)?;
        fs::write(&path, json)
            .with_context(|| format!("failed to write state to {}", path.display()))?;
        Ok(())
    }

    /// Load container state from disk.
    pub fn load(&self, id: &str) -> Result<ContainerState> {
        let path = self.state_file(id);
        let json =
            fs::read_to_string(&path).with_context(|| format!("container {id} not found"))?;
        let state: ContainerState = serde_json::from_str(&json)
            .with_context(|| format!("failed to parse state for container {id}"))?;
        Ok(state)
    }

    /// Update container state on disk.
    pub fn update<F>(&self, id: &str, f: F) -> Result<ContainerState>
    where
        F: FnOnce(&mut ContainerState),
    {
        let mut state = self.load(id)?;
        f(&mut state);
        self.save(&state)?;
        Ok(state)
    }

    /// Delete container state directory.
    pub fn delete(&self, id: &str) -> Result<()> {
        let dir = self.container_dir(id);
        if dir.exists() {
            fs::remove_dir_all(&dir)
                .with_context(|| format!("failed to remove state directory: {}", dir.display()))?;
        }
        Ok(())
    }

    /// Check if the container process is still running.
    pub fn is_process_alive(pid: u32) -> bool {
        // Send signal 0 to check if process exists
        unsafe { libc::kill(pid.cast_signed(), 0) == 0 }
    }

    /// Refresh container state by checking if process is still alive.
    pub fn refresh_state(&self, id: &str) -> Result<ContainerState> {
        let mut state = self.load(id)?;

        if state.status == Status::Running
            && let Some(pid) = state.pid
            && !Self::is_process_alive(pid)
        {
            state.status = Status::Stopped;
            self.save(&state)?;
        }

        Ok(state)
    }
}
