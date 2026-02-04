// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! OCI-compliant container runtime using LiteBox sandbox.
//!
//! This crate provides an OCI runtime that uses LiteBox for process sandboxing,
//! similar to how gVisor's runsc works.
//!
//! # OCI Lifecycle
//!
//! The runtime supports the full OCI container lifecycle:
//! - `create`: Set up a container without starting it
//! - `start`: Start a created container
//! - `state`: Query container state
//! - `kill`: Send signal to container
//! - `delete`: Remove container

pub mod lifecycle;
mod runner;
pub mod state;

pub use runner::Mount;
pub use runner::StdioRedirect;
pub use runner::run_container;
pub use runner::run_container_full;
pub use runner::run_container_with_all_options;
pub use runner::run_container_with_options;
