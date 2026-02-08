// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Integration tests for the ARM64 Linux userland runner.
//!
//! These tests verify that both systrap-based (seccomp) and rewriter-based syscall
//! interception work correctly on ARM64 (aarch64) architecture.

mod cache;
mod common;

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

/// Backend to use for syscall interception
#[derive(Clone, Copy, Debug)]
enum Backend {
    Seccomp,
    Rewriter,
}

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

impl Runner {
    fn new(target: &Path, unique_name: &str) -> Self {
        Self::with_backend(target, unique_name, Backend::Seccomp)
    }

    fn with_backend(target: &Path, unique_name: &str, backend: Backend) -> Self {
        let dir_path = common::get_out_dir();

        // For rewriter backend, rewrite the executable
        let path = match backend {
            Backend::Seccomp => target.to_path_buf(),
            Backend::Rewriter => {
                // new path in out_dir with .hooked suffix
                let out_path = dir_path.join(format!(
                    "{}.hooked",
                    target.file_name().unwrap().to_str().unwrap()
                ));
                // Use --allow-no-syscalls since dynamically linked binaries may not have
                // syscalls in the main executable (they're in libc.so instead)
                let success =
                    common::rewrite_with_cache(target, &out_path, &["--allow-no-syscalls"]);
                assert!(success, "failed to run litebox_syscall_rewriter_arm64");
                out_path
            }
        };

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
            match backend {
                Backend::Seccomp => {
                    println!(
                        "Copying {} to {}",
                        file_path.to_str().unwrap(),
                        dest_path.to_str().unwrap()
                    );
                    if let Err(e) = std::fs::copy(file_path, &dest_path) {
                        eprintln!("Warning: failed to copy {}: {}", file_path.display(), e);
                    }
                }
                Backend::Rewriter => {
                    // Use --allow-no-syscalls since some libraries may not have syscalls
                    let success =
                        common::rewrite_with_cache(file_path, &dest_path, &["--allow-no-syscalls"]);
                    assert!(
                        success,
                        "failed to run litebox_syscall_rewriter_arm64 for {}",
                        file_path.to_str().unwrap()
                    );
                }
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

        // Add backend-specific args
        match backend {
            Backend::Seccomp => {
                command.args(["--interception-backend", "seccomp"]);
            }
            Backend::Rewriter => {
                // We pre-rewrite binaries and libraries, so we only need to tell the runner
                // to use the rewriter backend (for rtld_audit), not to rewrite at runtime
                command.args(["--interception-backend", "rewriter"]);
            }
        }

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

/// Test statically linked executables with rewriter backend
///
/// This test uses the syscall rewriter to hook all SVC instructions in the binary,
/// avoiding the seccomp timing issues. The rewriter replaces SVC with jumps to
/// trampoline code that calls into the litebox syscall handler.
#[test]
#[cfg(target_arch = "aarch64")]
fn test_static_exec_with_rewriter() {
    for path in find_c_test_files("./tests") {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .expect("failed to get file stem");
        let unique_name = format!("{stem}_exec_rewriter");
        let target = common::compile(path.to_str().unwrap(), &unique_name, true, false);
        Runner::with_backend(&target, &unique_name, Backend::Rewriter).run();
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

/// Test dynamically linked executables with rewriter backend
///
/// This test rewrites both the executable and all its shared library dependencies.
#[test]
#[cfg(target_arch = "aarch64")]
fn test_dynamic_lib_with_rewriter() {
    for path in find_c_test_files("./tests") {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .expect("failed to get file stem");
        let unique_name = format!("{stem}_dynamic_rewriter");
        let target = common::compile(path.to_str().unwrap(), &unique_name, false, false);
        Runner::with_backend(&target, &unique_name, Backend::Rewriter).run();
    }
}

/// Get the path of a program using `which`
#[cfg(target_arch = "aarch64")]
fn run_which(prog: &str) -> std::path::PathBuf {
    let prog_path_str = std::process::Command::new("which")
        .arg(prog)
        .output()
        .expect("Failed to find program binary")
        .stdout;
    let prog_path_str = String::from_utf8(prog_path_str).unwrap().trim().to_string();
    let prog_path = std::path::PathBuf::from(prog_path_str);
    assert!(prog_path.exists(), "Program binary not found: {}", prog);
    prog_path
}

/// Test running `ls` command with rewriter backend
#[test]
#[cfg(target_arch = "aarch64")]
fn test_runner_with_ls() {
    let ls_path = run_which("ls");
    let output = Runner::with_backend(&ls_path, "ls_rewriter", Backend::Rewriter)
        .arg("-a")
        .output();

    let output_str = String::from_utf8_lossy(&output);
    let normalized = output_str.split_whitespace().collect::<Vec<_>>();
    for each in [".", "..", "lib"] {
        assert!(
            normalized.contains(&each),
            "unexpected ls output:\n{output_str}\n{each} not found",
        );
    }

    // test `ls` subdir - find the directory containing libc.so.6
    let libs = common::find_dependencies(ls_path.to_str().unwrap());
    let libc_path = libs
        .iter()
        .find(|l| l.contains("libc.so"))
        .expect("libc.so not found in dependencies");
    let libc_dir = std::path::Path::new(libc_path.as_str())
        .parent()
        .unwrap()
        .to_str()
        .unwrap();
    let output = Runner::with_backend(&ls_path, "ls_lib_rewriter", Backend::Rewriter)
        .args(["-a", libc_dir])
        .output();

    let output_str = String::from_utf8_lossy(&output);
    let normalized = output_str.split_whitespace().collect::<Vec<_>>();
    for each in [".", "..", "libc.so.6"] {
        assert!(
            normalized.contains(&each),
            "unexpected ls output:\n{output_str}\n{each} not found",
        );
    }
}

/// Test running Node.js with rewriter backend
#[test]
#[cfg(target_arch = "aarch64")]
fn test_node_with_rewriter() {
    const HELLO_WORLD_JS: &str = r"
const fs = require('node:fs');

const content = 'Hello World!';
console.log(content);
";

    let node_path = run_which("node");
    Runner::with_backend(&node_path, "hello_node_rewriter", Backend::Rewriter)
        .arg("/out/hello_world.js")
        .with_fs_path(|out_dir| {
            // write the test js file to the output directory
            std::fs::write(out_dir.join("out/hello_world.js"), HELLO_WORLD_JS).unwrap();
        })
        .run();
}

#[cfg(target_arch = "aarch64")]
fn run_python(args: &[&str]) -> String {
    let output = std::process::Command::new("python3")
        .args(args)
        .output()
        .expect("Failed to run Python");
    assert!(output.status.success(), "Python script failed");
    String::from_utf8(output.stdout).unwrap()
}

#[cfg(target_arch = "aarch64")]
fn has_origin_in_libs(binary_path: &Path) -> bool {
    let output = std::process::Command::new("readelf")
        .args(["-d", binary_path.to_str().unwrap()])
        .output()
        .expect("Failed to run readelf");

    if !output.status.success() {
        eprintln!("Warning: readelf failed for {}", binary_path.display());
        return false;
    }

    let output_str = String::from_utf8_lossy(&output.stdout);
    for line in output_str.lines() {
        // Check for $ORIGIN in NEEDED (shared library) entries
        if line.contains("(NEEDED)") && line.contains("$ORIGIN") {
            return true;
        }
    }
    false
}

/// Test running Python3 with rewriter backend
#[cfg(target_arch = "aarch64")]
#[test]
fn test_runner_with_python() {
    const HELLO_WORLD_PY: &str = "print(\"Hello, World from litebox!\")";
    let python_path = run_which("python3");

    if has_origin_in_libs(&python_path) {
        println!(
            "Skipping test: Python executable at {} uses $ORIGIN in library paths",
            python_path.display()
        );
        return;
    }

    let python_home = run_python(&["-c", "import sys; print(sys.prefix);"]);
    println!("Detected PYTHONHOME: {python_home}");
    let python_sys_path = run_python(&["-c", "import sys; print(':'.join(sys.path))"]);
    println!("Detected PYTHONPATH: {python_sys_path}");
    Runner::with_backend(&python_path, "python_rewriter", Backend::Rewriter)
        .args(["-c", HELLO_WORLD_PY])
        .envs([
            &format!("PYTHONHOME={}", python_home.trim()),
            &format!("PYTHONPATH={}", python_sys_path.trim()),
            // LiteBox does not support timestamp yet, so pre-compiled .pyc files are not usable.
            // Avoid creating .pyc files as tar filesystem is read-only.
            "PYTHONDONTWRITEBYTECODE=1",
        ])
        .with_fs_path(|out_dir| {
            for each in python_sys_path.split(':') {
                if each.is_empty() || !each.starts_with("/usr") {
                    continue;
                }
                let python_lib_src = Path::new(each);
                if python_lib_src.is_dir() {
                    let python_lib_dst = out_dir.join(&each[1..]); // remove leading '/'
                    if !python_lib_dst.exists() {
                        std::fs::create_dir_all(&python_lib_dst).unwrap();
                        println!(
                            "Copying python3 lib from {} to {}",
                            python_lib_src.to_str().unwrap(),
                            python_lib_dst.to_str().unwrap()
                        );
                        let output = std::process::Command::new("cp")
                            .args([
                                "-rpL", // -r for recursive, -p to preserve attributes, -L to dereference symbolic links
                                python_lib_src.to_str().unwrap(),
                                python_lib_dst.parent().unwrap().to_str().unwrap(),
                            ])
                            .output()
                            .expect("Failed to copy python3 lib");
                        assert!(
                            output.status.success(),
                            "failed to copy python3 lib {:?}",
                            std::str::from_utf8(output.stderr.as_slice()).unwrap()
                        );
                    }
                    let known_exts = ["py", "pyc", "txt", "css", "ps1", "rst"];
                    // rewrite all files under the python lib directory except those with known extensions
                    for entry in walkdir::WalkDir::new(python_lib_src)
                        .into_iter()
                        .filter_map(std::result::Result::ok)
                        .filter(|e| {
                            e.path()
                                .extension()
                                .is_some_and(|ext| !known_exts.contains(&ext.to_str().unwrap()))
                        })
                    {
                        let so_file = entry.path();
                        let so_file_dest = out_dir.join(so_file.strip_prefix("/").unwrap());
                        println!(
                            "Rewrite {} to {}",
                            so_file.display(),
                            so_file_dest.display()
                        );
                        let success =
                            common::rewrite_with_cache(so_file, &so_file_dest, &["--allow-no-syscalls"]);
                        if entry.path().extension().is_some_and(|ext| ext == "so") {
                            assert!(success, "failed to rewrite {} file", so_file.display());
                        }
                    }
                }
            }
        })
        .run();
}

/// Test network performance with iperf3
///
/// To run it with release build and see output, use:
/// ```
/// cargo test --package litebox_runner_linux_arm64_userland --test run --release -- test_tun_and_runner_with_iperf3 --exact --nocapture
/// ```
#[cfg(target_arch = "aarch64")]
#[test]
fn test_tun_and_runner_with_iperf3() {
    const NUM_CLIENTS: usize = 1;
    let iperf3_path = run_which("iperf3");
    let cloned_path = iperf3_path.clone();
    let has_started = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let has_started_clone = has_started.clone();
    std::thread::spawn(move || {
        // Rewrite iperf3 and its dependencies may take some time, wait until it's done.
        while !has_started_clone.load(std::sync::atomic::Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        std::thread::sleep(std::time::Duration::from_secs(5)); // wait a bit more to ensure server is ready
        println!("Starting iperf3 client...");
        let mut client = std::process::Command::new(&cloned_path)
            .args(["-c", "10.0.0.2", "-P", NUM_CLIENTS.to_string().as_str()])
            .spawn()
            .expect("Failed to start iperf3 client");
        client.wait().expect("Failed to wait on iperf3 client");
    });
    let mut runner =
        Runner::with_backend(&iperf3_path, "iperf3_server_rewriter", Backend::Rewriter);
    runner
        .args([
            "-s", // run in server mode
            "-1", // handle one client then exit
            "-B", "10.0.0.2", // bind to this address
        ])
        .tun_device_name("tun99");
    has_started.store(true, std::sync::atomic::Ordering::Relaxed);
    runner.run();
}
