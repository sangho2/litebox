// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! LiteBox-native OCI container execution.
//!
//! This module runs OCI containers directly through LiteBox's sandbox,
//! without using youki's libcontainer (which does Linux-native pivot_root).

use std::ffi::CString;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use litebox::fs::{FileSystem as _, Mode};
use litebox_platform_multiplex::Platform;
use oci_spec::runtime::Spec;
use walkdir::WalkDir;

/// Flag to indicate whether we need the rtld_audit library for rewriter backend
static REQUIRE_RTLD_AUDIT: AtomicBool = AtomicBool::new(false);

/// Run an OCI container using LiteBox sandbox.
///
/// This function:
/// 1. Loads all files from the OCI rootfs into LiteBox's in-memory filesystem
/// 2. Rewrites syscalls in executables for interception
/// 3. Sets up the environment from the OCI spec
/// 4. Runs the process through LiteBox's syscall emulation
///
/// # Panics
///
/// This function may panic if:
/// - Path conversion to string fails for non-UTF8 paths
/// - File creation in the sandbox fails
pub fn run_container(bundle_path: &Path) -> Result<i32> {
    let spec_path = bundle_path.join("config.json");
    let spec: Spec = {
        let file = std::fs::File::open(&spec_path)
            .with_context(|| format!("failed to open {}", spec_path.display()))?;
        serde_json::from_reader(file).context("failed to parse config.json")?
    };

    let rootfs_path = bundle_path.join(
        spec.root()
            .as_ref()
            .map_or(Path::new("rootfs"), |r| r.path().as_path()),
    );

    if !rootfs_path.exists() {
        anyhow::bail!("rootfs not found at {}", rootfs_path.display());
    }

    let process = spec
        .process()
        .as_ref()
        .context("OCI spec missing process section")?;

    let args = process
        .args()
        .as_ref()
        .context("OCI spec missing process args")?;

    if args.is_empty() {
        anyhow::bail!("process args cannot be empty");
    }

    tracing::info!(
        rootfs = %rootfs_path.display(),
        args = ?args,
        "starting LiteBox OCI container"
    );

    // Initialize LiteBox platform
    let platform = Platform::new(None);
    litebox_platform_multiplex::set_platform(platform);

    let mut shim_builder = litebox_shim_linux::LinuxShimBuilder::new();
    let litebox_instance = shim_builder.litebox();

    // Set up filesystem from OCI rootfs
    let initial_fs = {
        let mut in_mem = litebox::fs::in_mem::FileSystem::new(litebox_instance);

        // Create standard directory structure first
        in_mem.with_root_privileges(|fs| {
            let dir_mode = Mode::RWXU | Mode::RGRP | Mode::XGRP | Mode::ROTH | Mode::XOTH;
            let _ = fs.mkdir("/", dir_mode);
            let _ = fs.mkdir("/tmp", Mode::RWXU | Mode::RWXG | Mode::RWXO);
            let _ = fs.mkdir("/proc", dir_mode);
            let _ = fs.mkdir("/dev", dir_mode);
            let _ = fs.mkdir("/lib", dir_mode);
        });

        // Walk the rootfs and copy files into LiteBox's in-memory fs
        let exec_mode = Mode::RWXU | Mode::RGRP | Mode::XGRP | Mode::ROTH | Mode::XOTH;
        let file_mode = Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::ROTH;

        // Helper to load a file into the in-memory filesystem
        let load_file = |in_mem: &mut litebox::fs::in_mem::FileSystem<Platform>,
                         host_path: &Path,
                         target_str: &str,
                         exec_mode: Mode,
                         file_mode: Mode| {
            if let Ok(data) = std::fs::read(host_path) {
                // Check if this is an executable
                let is_executable = host_path
                    .metadata()
                    .map(|m| m.permissions().mode() & 0o111 != 0)
                    .unwrap_or(false);

                // If executable, rewrite syscalls for interception
                let data: std::borrow::Cow<'static, [u8]> = if is_executable {
                    match litebox_syscall_rewriter::hook_syscalls_in_elf(&data, None) {
                        Ok(rewritten) => {
                            tracing::debug!(path = %target_str, "rewrote syscalls in executable");
                            rewritten.into()
                        }
                        Err(_) => data.into(),
                    }
                } else {
                    data.into()
                };

                in_mem.with_root_privileges(|fs| {
                    // Ensure parent directories exist
                    let target_path = Path::new(target_str);
                    if let Some(parent) = target_path.parent() {
                        let mut current = std::path::PathBuf::from("/");
                        for component in parent.components().skip(1) {
                            current.push(component);
                            let _ = fs.mkdir(current.to_str().unwrap(), exec_mode);
                        }
                    }

                    let mode = if is_executable { exec_mode } else { file_mode };

                    let fd = fs
                        .open(
                            target_str,
                            litebox::fs::OFlags::WRONLY | litebox::fs::OFlags::CREAT,
                            mode,
                        )
                        .expect("failed to create file in sandbox");
                    fs.initialize_primarily_read_heavy_file(&fd, data);
                    fs.close(&fd).expect("failed to close file");
                });
            }
        };

        for entry in WalkDir::new(&rootfs_path)
            .follow_links(false)
            .into_iter()
            .filter_map(std::result::Result::ok)
        {
            let rel_path = entry
                .path()
                .strip_prefix(&rootfs_path)
                .unwrap_or(entry.path());

            // Skip the root itself
            if rel_path == Path::new("") {
                continue;
            }

            let target_path = Path::new("/").join(rel_path);
            let target_str = target_path.to_str().unwrap_or("/");

            if entry.file_type().is_dir() {
                in_mem.with_root_privileges(|fs| {
                    let _ = fs.mkdir(target_str, exec_mode);
                });
            } else if entry.file_type().is_file() {
                load_file(&mut in_mem, entry.path(), target_str, exec_mode, file_mode);
            } else if entry.file_type().is_symlink() {
                // Resolve symlink and copy the target file
                // LiteBox doesn't support symlinks, so we flatten them to regular files
                if let Ok(link_target) = std::fs::read_link(entry.path()) {
                    // Build the full path and canonicalize to resolve .. and other relative components
                    let full_path = if link_target.is_absolute() {
                        rootfs_path.join(link_target.strip_prefix("/").unwrap_or(&link_target))
                    } else {
                        entry
                            .path()
                            .parent()
                            .unwrap_or(entry.path())
                            .join(&link_target)
                    };

                    // Canonicalize to resolve all symlinks and relative paths
                    let Ok(resolved) = full_path.canonicalize() else {
                        continue; // Skip broken symlinks
                    };

                    // Ensure the resolved path is still within rootfs
                    if !resolved.starts_with(&rootfs_path) {
                        tracing::warn!(
                            symlink = %target_str,
                            target = %resolved.display(),
                            "symlink target outside rootfs, skipping"
                        );
                        continue;
                    }

                    if resolved.is_file() {
                        load_file(&mut in_mem, &resolved, target_str, exec_mode, file_mode);
                    } else if resolved.is_dir() {
                        // Symlink to directory - create the directory
                        in_mem.with_root_privileges(|fs| {
                            let _ = fs.mkdir(target_str, exec_mode);
                        });
                    }
                }
            }
        }

        // Include litebox_rtld_audit.so for rewriter backend (x86_64 only)
        #[cfg(target_arch = "x86_64")]
        {
            REQUIRE_RTLD_AUDIT.store(true, Ordering::SeqCst);
            in_mem.with_root_privileges(|fs| {
                let rwxr_xr_x = Mode::RWXU | Mode::RGRP | Mode::XGRP | Mode::ROTH | Mode::XOTH;
                let _ = fs.mkdir("/lib", rwxr_xr_x);
                // Include the rtld_audit library built during compilation
                // Note: This requires the library to be available at build time
                let rtld_audit_data =
                    include_bytes!(concat!(env!("OUT_DIR"), "/litebox_rtld_audit.so"));
                let fd = fs
                    .open(
                        "/lib/litebox_rtld_audit.so",
                        litebox::fs::OFlags::WRONLY | litebox::fs::OFlags::CREAT,
                        rwxr_xr_x,
                    )
                    .expect("Failed to create /lib/litebox_rtld_audit.so");
                fs.initialize_primarily_read_heavy_file(&fd, rtld_audit_data.as_slice().into());
                fs.close(&fd)
                    .expect("Failed to close /lib/litebox_rtld_audit.so");
            });
        }

        // Use empty tar for read-only layer
        let tar_ro = litebox::fs::tar_ro::FileSystem::new(
            litebox_instance,
            litebox::fs::tar_ro::EMPTY_TAR_FILE.into(),
        );
        shim_builder.default_fs(in_mem, tar_ro)
    };

    shim_builder.set_fs(initial_fs);
    shim_builder.set_load_filter(fixup_env);
    let shim = shim_builder.build();

    // Using rewriter backend - no seccomp setup needed
    // The syscalls have been rewritten in the ELF files

    // Prepare argv (named differently to avoid confusion with args from OCI spec)
    let argv_cstrings: Vec<CString> = args
        .iter()
        .map(|s| CString::new(s.as_bytes()).unwrap_or_default())
        .collect();

    // Prepare envp from OCI spec
    let envp: Vec<CString> = process
        .env()
        .as_ref()
        .map(|envs| {
            envs.iter()
                .map(|s| CString::new(s.as_bytes()).unwrap_or_default())
                .collect()
        })
        .unwrap_or_default();

    // Determine program path - resolve relative paths using PATH from env
    let prog_path = {
        let first_arg = &args[0];
        if first_arg.starts_with('/') {
            first_arg.clone()
        } else {
            // Search PATH for the binary
            let path_dirs: Vec<&str> = process
                .env()
                .as_ref()
                .and_then(|envs| {
                    envs.iter()
                        .find(|e| e.starts_with("PATH="))
                        .map(|e| e.strip_prefix("PATH=").unwrap_or(""))
                })
                .unwrap_or("/usr/local/bin:/usr/bin:/bin")
                .split(':')
                .collect();

            let mut resolved = None;
            for dir in &path_dirs {
                let candidate = format!("{dir}/{first_arg}");
                let host_path = rootfs_path.join(candidate.trim_start_matches('/'));
                if host_path.exists() || host_path.is_symlink() {
                    resolved = Some(candidate);
                    break;
                }
            }
            resolved.unwrap_or_else(|| first_arg.clone())
        }
    };

    tracing::info!(path = %prog_path, "loading program into LiteBox sandbox");

    let platform = litebox_platform_multiplex::platform();
    let program = shim
        .load_program(platform.init_task(), &prog_path, argv_cstrings, envp)
        .with_context(|| format!("failed to load program: {prog_path}"))?;

    // Run the sandboxed program
    let _ = unsafe {
        litebox_platform_linux_userland::run_thread(
            program.entrypoints,
            &mut litebox_common_linux::PtRegs::default(),
        )
    };

    // Return exit code
    Ok(program.process.wait())
}

/// Fixup environment variables for the rewriter backend
fn fixup_env(envp: &mut Vec<std::ffi::CString>) {
    if REQUIRE_RTLD_AUDIT.load(Ordering::SeqCst) {
        let p = c"LD_AUDIT=/lib/litebox_rtld_audit.so";
        let has_ld_audit = envp.iter().any(|var| var.as_c_str() == p);
        if !has_ld_audit {
            envp.push(p.into());
        }
    }
}
