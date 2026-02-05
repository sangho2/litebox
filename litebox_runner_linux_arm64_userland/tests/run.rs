// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Integration tests for the ARM64 Linux userland runner.
//!
//! These tests verify that the systrap-based syscall interception works correctly
//! on ARM64 (aarch64) architecture. Unlike the x86 runner, the ARM64 runner only
//! supports the seccomp/systrap backend (no binary rewriting).

mod cache;
mod common;

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

#[must_use]
struct Runner {
    command: std::process::Command,
    dir_path: PathBuf,
    tar_dir: PathBuf,
    unique_name: String,
    cmd_path: PathBuf,
    cmd_args: Vec<OsString>,
    has_run: bool,
}

/// Get the output directory for test artifacts
fn get_out_dir() -> PathBuf {
    std::env::var_os("OUT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            // Fallback to target/debug/test-artifacts when OUT_DIR is not set
            let manifest_dir =
                std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
            let target_dir = PathBuf::from(manifest_dir)
                .parent()
                .unwrap_or(Path::new("."))
                .join("target/debug/test-artifacts");
            std::fs::create_dir_all(&target_dir).ok();
            target_dir
        })
}

impl Runner {
    fn new(target: &Path, unique_name: &str) -> Self {
        let dir_path = get_out_dir();

        // For systrap backend, we use the target directly without rewriting
        let path = target.to_path_buf();

        // create tar file containing all dependencies
        let tar_dir = dir_path.join(format!("tar_files_{unique_name}"));
        // ARM64 library directories
        let dirs_to_create = ["lib", "lib/aarch64-linux-gnu", "usr/lib/aarch64-linux-gnu"];
        for dir in dirs_to_create {
            std::fs::create_dir_all(tar_dir.join(dir)).unwrap();
        }
        std::fs::create_dir_all(tar_dir.join("out")).unwrap();

        let libs = common::find_dependencies(target.to_str().unwrap());
        for file in &libs {
            let file_path = std::path::Path::new(file.as_str());
            let dest_path = tar_dir.join(&file[1..]);
            // Ensure parent directory exists
            if let Some(parent) = dest_path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            println!(
                "Copying {} to {}",
                file_path.to_str().unwrap(),
                dest_path.to_str().unwrap()
            );
            if let Err(e) = std::fs::copy(file_path, &dest_path) {
                eprintln!("Warning: failed to copy {}: {}", file_path.display(), e);
            }
        }

        // Get the path to the litebox_runner_linux_arm64_userland binary
        let binary_path = std::env::var("NEXTEST_BIN_EXE_litebox_runner_linux_arm64_userland")
            .unwrap_or_else(|_| {
                env!("CARGO_BIN_EXE_litebox_runner_linux_arm64_userland").to_string()
            });

        // run litebox_runner_linux_arm64_userland with the tar file and the compiled executable
        let mut command = std::process::Command::new(binary_path);
        command.args([
            "--unstable",
            // Tell ld where to find the libraries.
            "--env",
            "LD_LIBRARY_PATH=/lib:/lib/aarch64-linux-gnu:/usr/lib/aarch64-linux-gnu",
            "--env",
            "HOME=/",
        ]);

        Self {
            command,
            dir_path,
            tar_dir,
            cmd_path: path,
            cmd_args: Vec::new(),
            has_run: false,
            unique_name: unique_name.to_owned(),
        }
    }

    fn env(&mut self, env: impl AsRef<std::ffi::OsStr>) -> &mut Self {
        self.command.arg("--env").arg(env);
        self
    }

    #[allow(dead_code)]
    fn envs(&mut self, envs: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>) -> &mut Self {
        for env in envs {
            self.env(env);
        }
        self
    }

    fn arg(&mut self, arg: impl AsRef<std::ffi::OsStr>) -> &mut Self {
        self.cmd_args.push(arg.as_ref().to_os_string());
        self
    }

    #[allow(dead_code)]
    fn args(&mut self, args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>) -> &mut Self {
        for arg in args {
            self.arg(arg);
        }
        self
    }

    fn tun_device_name(&mut self, tun_name: &str) -> &mut Self {
        self.command.arg("--tun-device-name").arg(tun_name);
        self
    }

    #[allow(dead_code)]
    fn with_fs_path(&mut self, f: impl FnOnce(&Path)) -> &mut Self {
        f(&self.tar_dir);
        self
    }

    fn run(&mut self) {
        self.run_inner(false);
    }

    #[must_use]
    #[allow(dead_code)]
    fn output(&mut self) -> Vec<u8> {
        self.run_inner(true)
    }

    fn run_inner(&mut self, capture_stdout: bool) -> Vec<u8> {
        assert!(!self.has_run);
        self.has_run = true;
        // create tar file using `tar` command with caching
        let tar_file = self
            .dir_path
            .join(format!("rootfs_{}.tar", self.unique_name));
        let tar_success =
            common::create_tar_with_cache(&self.tar_dir, &tar_file, &self.unique_name);
        assert!(tar_success, "failed to create tar file");
        println!("Tar file ready at: {}", tar_file.to_str().unwrap());

        self.command
            .arg("--initial-files")
            .arg(tar_file)
            .arg(&self.cmd_path)
            .args(&self.cmd_args)
            .stderr(std::process::Stdio::inherit());
        if !capture_stdout {
            self.command.stdout(std::process::Stdio::inherit());
        }
        println!("Running `{:?}`", self.command);
        let output = self
            .command
            .output()
            .expect("Failed to run litebox_runner_linux_arm64_userland");
        assert!(
            output.status.success(),
            "failed to run litebox_runner_linux_arm64_userland: {}",
            output.status
        );
        output.stdout
    }
}

/// Find all C test files in a directory
fn find_c_test_files(dir: &str) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if let Some("c") = path.extension().and_then(|e| e.to_str()) {
            files.push(path);
        }
    }
    files
}

/// Test statically linked executables with systrap backend
///
/// NOTE: This test is currently ignored because the seccomp backend has issues with
/// syscalls made by std/libc after the filter is applied. The SIGSYS handler assumes
/// guest mode is active, but syscalls from seccompiler/libc trigger SIGSYS before
/// we enter guest mode. This needs to be fixed by using raw syscalls with backdoor
/// magic for all post-filter operations.
#[test]
#[cfg(target_arch = "aarch64")]
#[ignore = "Seccomp backend needs raw syscalls post-filter; see issue with libc syscalls"]
fn test_static_exec_with_systrap() {
    for path in find_c_test_files("./tests") {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .expect("failed to get file stem");
        let unique_name = format!("{stem}_exec_systrap");
        let target = common::compile(path.to_str().unwrap(), &unique_name, true, false);
        Runner::new(&target, &unique_name).run();
    }
}

/// Test dynamically linked executables with systrap backend
#[test]
#[cfg(target_arch = "aarch64")]
#[ignore = "Dynamic linking with systrap needs platform std support"]
fn test_dynamic_lib_with_systrap() {
    for path in find_c_test_files("./tests") {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .expect("failed to get file stem");
        let unique_name = format!("{stem}_systrap");
        let target = common::compile(path.to_str().unwrap(), &unique_name, false, false);
        Runner::new(&target, &unique_name).run();
    }
}

/// Test TCP socket with TUN device
/// Test TCP socket with TUN device
/// Note: This test requires root/CAP_NET_ADMIN privileges to create TUN devices
#[test]
#[cfg(target_arch = "aarch64")]
#[ignore = "Requires root/CAP_NET_ADMIN to create TUN devices"]
fn test_tun_with_tcp_socket() {
    let tcp_server_path = PathBuf::from("./tests/net/tcp_server.c");
    let tcp_client_path = PathBuf::from("./tests/net/tcp_client.c");
    let unique_name = "tcp_server_exec_systrap";
    let server_target =
        common::compile(tcp_server_path.to_str().unwrap(), unique_name, true, false);
    let client_target = common::compile(
        tcp_client_path.to_str().unwrap(),
        "tcp_client_arm64",
        false,
        false,
    );

    let child = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(2)); // wait for server to start
        std::process::Command::new(client_target.to_str().unwrap())
            .arg("10.0.0.2")
            .arg("12345")
            .status()
            .expect("failed to execute client");
    });
    Runner::new(&server_target, unique_name)
        .arg("10.0.0.2")
        .arg("12345")
        .tun_device_name("tun99")
        .run();
    child.join().unwrap();
}
