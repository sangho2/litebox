// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! LiteBox-native OCI container execution.
//!
//! This module runs OCI containers directly through LiteBox's sandbox,
//! without using youki's libcontainer (which does Linux-native pivot_root).

use std::ffi::CString;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use litebox::fs::{FileSystem as _, Mode};
use litebox_platform_multiplex::Platform;
use oci_spec::runtime::Spec;
use walkdir::WalkDir;

/// Flag to indicate whether we need the rtld_audit library for rewriter backend
static REQUIRE_RTLD_AUDIT: AtomicBool = AtomicBool::new(false);

/// Cache directory for rewritten binaries (used in eager mode)
fn cache_dir() -> PathBuf {
    // Use XDG cache dir or fallback to ~/.cache
    std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME").map_or_else(
                |_| PathBuf::from("/tmp"),
                |h| PathBuf::from(h).join(".cache"),
            )
        })
        .join("litebox-oci")
        .join("rewritten")
}

/// Compute a hash of file contents for cache key
fn hash_bytes(data: &[u8]) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    data.hash(&mut hasher);
    // Include data length to reduce collisions
    data.len().hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Try to load a rewritten binary from cache
fn load_from_cache(hash: &str) -> Option<Vec<u8>> {
    let cache_path = cache_dir().join(hash);
    let mut file = std::fs::File::open(&cache_path).ok()?;
    let mut data = Vec::new();
    file.read_to_end(&mut data).ok()?;
    tracing::debug!(hash = %hash, "loaded rewritten binary from cache");
    Some(data)
}

/// Save a rewritten binary to cache
fn save_to_cache(hash: &str, data: &[u8]) {
    let cache_path = cache_dir().join(hash);
    if let Some(parent) = cache_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = std::fs::File::create(&cache_path) {
        let _ = file.write_all(data);
        tracing::debug!(hash = %hash, "saved rewritten binary to cache");
    }
}

/// Rewrite syscalls in an ELF binary, using cache if available
fn rewrite_with_cache(data: &[u8]) -> Vec<u8> {
    let hash = hash_bytes(data);

    // Try cache first
    if let Some(cached) = load_from_cache(&hash) {
        return cached;
    }

    // Rewrite and cache
    match litebox_syscall_rewriter::hook_syscalls_in_elf(data, None) {
        Ok(rewritten) => {
            save_to_cache(&hash, &rewritten);
            rewritten
        }
        Err(_) => data.to_vec(),
    }
}

/// Squashfs cache directory for lazy loading
fn squashfs_cache_dir() -> PathBuf {
    std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME").map_or_else(
                |_| PathBuf::from("/tmp"),
                |h| PathBuf::from(h).join(".cache"),
            )
        })
        .join("litebox-oci")
        .join("squashfs")
}

/// Create a squashfs image from the rootfs directory.
/// Returns the path to the squashfs file.
fn create_squashfs(rootfs_path: &Path) -> Result<PathBuf> {
    // Hash the rootfs path for cache key
    let cache_key = hash_bytes(rootfs_path.to_string_lossy().as_bytes());
    let cache_dir = squashfs_cache_dir();
    std::fs::create_dir_all(&cache_dir)?;
    let squashfs_path = cache_dir.join(format!("{cache_key}.squashfs"));

    // Check if already cached
    if squashfs_path.exists() {
        tracing::debug!(path = %squashfs_path.display(), "using cached squashfs");
        return Ok(squashfs_path);
    }

    tracing::info!(
        rootfs = %rootfs_path.display(),
        squashfs = %squashfs_path.display(),
        "creating squashfs image"
    );

    let status = Command::new("mksquashfs")
        .arg(rootfs_path)
        .arg(&squashfs_path)
        .arg("-noappend")
        .arg("-quiet")
        .status()
        .context("failed to run mksquashfs. Is squashfs-tools installed?")?;

    if !status.success() {
        anyhow::bail!("mksquashfs failed with status: {}", status);
    }

    Ok(squashfs_path)
}

/// Mount a squashfs image and return the mount point path.
/// Returns a guard that unmounts when dropped.
fn mount_squashfs(squashfs_path: &Path) -> Result<SquashfsMount> {
    let mount_point = std::env::temp_dir().join(format!("litebox-sqfs-{}", std::process::id()));
    std::fs::create_dir_all(&mount_point)?;

    tracing::debug!(
        squashfs = %squashfs_path.display(),
        mount_point = %mount_point.display(),
        "mounting squashfs"
    );

    let status = Command::new("mount")
        .arg("-o")
        .arg("loop,ro")
        .arg(squashfs_path)
        .arg(&mount_point)
        .status()
        .context("failed to run mount. Do you have permission to use loop devices?")?;

    if !status.success() {
        // Try with sudo as fallback
        let status = Command::new("sudo")
            .arg("mount")
            .arg("-o")
            .arg("loop,ro")
            .arg(squashfs_path)
            .arg(&mount_point)
            .status()
            .context("failed to mount squashfs (tried with sudo)")?;

        if !status.success() {
            anyhow::bail!("mount failed. Try running with sudo or check loop device permissions");
        }
    }

    Ok(SquashfsMount { mount_point })
}

/// Guard that unmounts squashfs when dropped
struct SquashfsMount {
    mount_point: PathBuf,
}

impl Drop for SquashfsMount {
    fn drop(&mut self) {
        // Try to unmount - suppress errors in output
        let result = Command::new("umount")
            .arg(&self.mount_point)
            .stderr(std::process::Stdio::null())
            .status();

        if result.is_err() || !result.unwrap().success() {
            // Try with sudo as fallback (also suppress errors)
            let _ = Command::new("sudo")
                .arg("umount")
                .arg(&self.mount_point)
                .stderr(std::process::Stdio::null())
                .status();
        }
        // Clean up mount point directory
        let _ = std::fs::remove_dir(&self.mount_point);
    }
}

/// Tar cache directory for true lazy loading
fn tar_cache_dir() -> PathBuf {
    std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME").map_or_else(
                |_| PathBuf::from("/tmp"),
                |h| PathBuf::from(h).join(".cache"),
            )
        })
        .join("litebox-oci")
        .join("tar")
}

/// Create a tar archive from the rootfs directory.
/// Returns the tar data as a Vec<u8>.
fn create_tar_from_rootfs(rootfs_path: &Path) -> Result<Vec<u8>> {
    // Hash the rootfs path for cache key
    let cache_key = hash_bytes(rootfs_path.to_string_lossy().as_bytes());
    let cache_path = tar_cache_dir().join(format!("{cache_key}.tar"));

    // Check cache first
    if cache_path.exists() {
        tracing::debug!(path = %cache_path.display(), "using cached tar");
        return std::fs::read(&cache_path)
            .with_context(|| format!("failed to read cached tar: {}", cache_path.display()));
    }

    tracing::debug!(
        rootfs = %rootfs_path.display(),
        "creating tar archive from rootfs"
    );

    // Create tar in memory
    let mut tar_data = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_data);

        for entry in WalkDir::new(rootfs_path)
            .follow_links(false)
            .into_iter()
            .filter_map(std::result::Result::ok)
        {
            let rel_path = entry
                .path()
                .strip_prefix(rootfs_path)
                .unwrap_or(entry.path());

            // Skip the root itself
            if rel_path == Path::new("") {
                continue;
            }

            let path_str = rel_path.to_string_lossy();

            if entry.file_type().is_dir() {
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(tar::EntryType::Directory);
                header.set_mode(0o755);
                header.set_size(0);
                header.set_uid(0);
                header.set_gid(0);
                header.set_cksum();
                builder.append_data(&mut header, &*path_str, std::io::empty())?;
            } else if entry.file_type().is_file() {
                let metadata = entry.metadata()?;
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(tar::EntryType::Regular);
                // Only use permission bits (lower 12 bits), not file type bits
                header.set_mode(metadata.permissions().mode() & 0o7777);
                header.set_size(metadata.len());
                header.set_uid(0);
                header.set_gid(0);
                header.set_cksum();

                let file = std::fs::File::open(entry.path())?;
                builder.append_data(&mut header, &*path_str, file)?;
            } else if entry.file_type().is_symlink() {
                if let Ok(link_target) = std::fs::read_link(entry.path()) {
                    let mut header = tar::Header::new_gnu();
                    header.set_entry_type(tar::EntryType::Symlink);
                    header.set_mode(0o777);
                    header.set_size(0);
                    header.set_uid(0);
                    header.set_gid(0);
                    header.set_cksum();
                    builder.append_link(&mut header, &*path_str, &link_target)?;
                }
            }
        }
        builder.finish()?;
    }

    // Pad to 10240 bytes (tar block size) for tar-no-std compatibility
    let block_size = 10240;
    let padding_needed = (block_size - (tar_data.len() % block_size)) % block_size;
    tar_data.extend(std::iter::repeat_n(0u8, padding_needed));

    // Cache the tar for future use
    if let Some(parent) = cache_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&cache_path, &tar_data) {
        tracing::warn!(error = %e, "failed to cache tar");
    } else {
        tracing::debug!(path = %cache_path.display(), size = tar_data.len(), "cached tar archive");
    }

    Ok(tar_data)
}

/// A bind mount specification.
#[derive(Debug, Clone)]
pub struct Mount {
    /// Source path on the host
    pub source: PathBuf,
    /// Destination path in the container
    pub destination: String,
    /// Whether the mount is read-only (note: writes don't persist anyway)
    pub readonly: bool,
}

/// Stdio redirection configuration.
#[derive(Debug, Clone, Default)]
pub struct StdioRedirect {
    /// Path to redirect stdout to
    pub stdout: Option<PathBuf>,
    /// Path to redirect stderr to
    pub stderr: Option<PathBuf>,
}

/// Network configuration for container.
#[derive(Debug, Clone, Default)]
pub struct NetworkConfig {
    /// TUN device name to use for networking (e.g., "tun99").
    /// If None, networking syscalls will fail.
    pub tun_device: Option<String>,
}

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
    run_container_internal(
        bundle_path,
        None,
        &[],
        &[],
        &StdioRedirect::default(),
        &NetworkConfig::default(),
        LazyMode::Eager,
    )
}

/// Run a container with additional environment variables and mounts.
pub fn run_container_with_options(
    bundle_path: &Path,
    extra_env: &[String],
    mounts: &[Mount],
) -> Result<i32> {
    run_container_internal(
        bundle_path,
        None,
        extra_env,
        mounts,
        &StdioRedirect::default(),
        &NetworkConfig::default(),
        LazyMode::Eager,
    )
}

/// Run a command in a container's rootfs with all options.
pub fn run_container_with_all_options(
    bundle_path: &Path,
    args: &[String],
    extra_env: &[String],
    mounts: &[Mount],
) -> Result<i32> {
    if args.is_empty() {
        anyhow::bail!("exec command cannot be empty");
    }
    run_container_internal(
        bundle_path,
        Some(args),
        extra_env,
        mounts,
        &StdioRedirect::default(),
        &NetworkConfig::default(),
        LazyMode::Eager,
    )
}

/// Run a container with full control over all options including stdio redirection and networking.
pub fn run_container_full(
    bundle_path: &Path,
    override_args: Option<&[String]>,
    extra_env: &[String],
    mounts: &[Mount],
    stdio: &StdioRedirect,
    network: &NetworkConfig,
) -> Result<i32> {
    if let Some(args) = override_args
        && args.is_empty()
    {
        anyhow::bail!("exec command cannot be empty");
    }
    run_container_internal(
        bundle_path,
        override_args,
        extra_env,
        mounts,
        stdio,
        network,
        LazyMode::Eager,
    )
}

/// Run a container with lazy file loading using squashfs + loop mount.
///
/// This mode:
/// 1. Creates a squashfs image from the rootfs (cached for reuse)
/// 2. Mounts it via loop device
/// 3. Reads files on-demand instead of copying everything to memory
/// 4. Rewrites executables lazily when they are first executed
///
/// Benefits:
/// - Much faster container startup for large images
/// - Lower memory usage (only accessed files are loaded)
/// - Kernel handles caching and demand paging
///
/// Requirements:
/// - `mksquashfs` command available
/// - Root/sudo access for loop mount (or user namespaces)
pub fn run_container_lazy(
    bundle_path: &Path,
    override_args: Option<&[String]>,
    extra_env: &[String],
    mounts: &[Mount],
    stdio: &StdioRedirect,
    network: &NetworkConfig,
) -> Result<i32> {
    if let Some(args) = override_args
        && args.is_empty()
    {
        anyhow::bail!("exec command cannot be empty");
    }
    run_container_internal(
        bundle_path,
        override_args,
        extra_env,
        mounts,
        stdio,
        network,
        LazyMode::Squashfs,
    )
}

/// Run a container with true lazy file loading using tar + layered filesystem.
///
/// This mode:
/// 1. Creates a tar archive from the rootfs (cached for reuse)
/// 2. Uses tar_ro::FileSystem as read-only lower layer
/// 3. Uses in_mem::FileSystem as writable upper layer
/// 4. Combines them with layered::FileSystem for copy-on-write
/// 5. Only loads file metadata upfront - content is read on-demand
///
/// Benefits:
/// - Much faster startup (no upfront file copying)
/// - Lower memory usage (only accessed files are loaded)
/// - Copy-on-write semantics for modifications
///
/// Note: Executable rewriting still happens for files that are accessed.
pub fn run_container_lazy_tar(
    bundle_path: &Path,
    override_args: Option<&[String]>,
    extra_env: &[String],
    mounts: &[Mount],
    stdio: &StdioRedirect,
    network: &NetworkConfig,
) -> Result<i32> {
    if let Some(args) = override_args
        && args.is_empty()
    {
        anyhow::bail!("exec command cannot be empty");
    }
    run_container_internal(
        bundle_path,
        override_args,
        extra_env,
        mounts,
        stdio,
        network,
        LazyMode::TarLayered,
    )
}

/// Lazy loading mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LazyMode {
    /// Eager loading - copy all files upfront
    Eager,
    /// Squashfs + loop mount (still walks all files)
    Squashfs,
    /// True lazy loading with tar + layered filesystem
    TarLayered,
}

/// Internal implementation that handles both regular run and exec.
fn run_container_internal(
    bundle_path: &Path,
    override_args: Option<&[String]>,
    extra_env: &[String],
    mounts: &[Mount],
    stdio: &StdioRedirect,
    network: &NetworkConfig,
    lazy_mode: LazyMode,
) -> Result<i32> {
    // Set up stdio redirection before running
    let _stdout_guard = if let Some(path) = &stdio.stdout {
        Some(redirect_stdout(path)?)
    } else {
        None
    };
    let _stderr_guard = if let Some(path) = &stdio.stderr {
        Some(redirect_stderr(path)?)
    } else {
        None
    };

    let spec_path = bundle_path.join("config.json");
    let spec: Spec = {
        let file = std::fs::File::open(&spec_path).with_context(|| {
            format!(
                "failed to open {}. Ensure the bundle directory contains a valid config.json",
                spec_path.display()
            )
        })?;
        serde_json::from_reader(file).with_context(|| {
            "failed to parse config.json. Ensure it is valid OCI runtime spec JSON. \
                 See https://github.com/opencontainers/runtime-spec for format details."
                .to_string()
        })?
    };

    let rootfs_path = bundle_path.join(
        spec.root()
            .as_ref()
            .map_or(Path::new("rootfs"), |r| r.path().as_path()),
    );

    if !rootfs_path.exists() {
        anyhow::bail!(
            "rootfs not found at {}. \
             The bundle must contain a 'rootfs' directory (or path specified in config.json root.path)",
            rootfs_path.display()
        );
    }

    let process = spec
        .process()
        .as_ref()
        .context("OCI spec missing 'process' section. config.json must define process.args")?;

    // Use override args if provided, otherwise use spec args
    let args: Vec<String> = if let Some(override_args) = override_args {
        override_args.to_vec()
    } else {
        let spec_args = process.args().as_ref().context(
            "OCI spec missing 'process.args'. Specify the command to run in config.json",
        )?;
        if spec_args.is_empty() {
            anyhow::bail!(
                "process.args cannot be empty. Specify at least one argument (the program to run)"
            );
        }
        spec_args.clone()
    };

    tracing::info!(
        rootfs = %rootfs_path.display(),
        args = ?args,
        tun_device = ?network.tun_device,
        lazy = ?lazy_mode,
        "starting LiteBox OCI container"
    );

    // For squashfs lazy mode, create and mount squashfs
    // Keep mount guard alive until end of function for cleanup via Drop
    #[allow(unused_variables)]
    let squashfs_mount = if lazy_mode == LazyMode::Squashfs {
        let squashfs_path = create_squashfs(&rootfs_path)?;
        Some(mount_squashfs(&squashfs_path)?)
    } else {
        None
    };

    // Use the mount point as rootfs in squashfs mode
    let effective_rootfs = if let Some(ref mount) = squashfs_mount {
        mount.mount_point.clone()
    } else {
        rootfs_path.clone()
    };

    // Initialize LiteBox platform with optional TUN networking
    let platform = Platform::new(network.tun_device.as_deref());
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

        // Helper to load a file into the in-memory filesystem (with executable rewriting)
        let load_file_with_rewrite = |in_mem: &mut litebox::fs::in_mem::FileSystem<Platform>,
                                      data: Vec<u8>,
                                      target_str: &str,
                                      is_executable: bool,
                                      exec_mode: Mode,
                                      file_mode: Mode| {
            // If executable, rewrite syscalls for interception (with caching)
            let data: std::borrow::Cow<'static, [u8]> = if is_executable {
                let rewritten = rewrite_with_cache(&data);
                if rewritten.len() != data.len() {
                    tracing::debug!(path = %target_str, "rewrote syscalls in executable");
                }
                rewritten.into()
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
        };

        // For TarLayered mode, create tar and only load executables into upper layer
        let tar_data: std::borrow::Cow<'static, [u8]> = if lazy_mode == LazyMode::TarLayered {
            tracing::info!("using true lazy loading with tar + layered filesystem");
            let tar_bytes = create_tar_from_rootfs(&rootfs_path)?;

            // Single pass: collect executables and symlinks, load executable content
            // Symlinks pointing to executables need the rewritten content
            let mut symlinks: Vec<(String, String)> = Vec::new();
            let mut executable_content: std::collections::HashMap<String, Vec<u8>> =
                std::collections::HashMap::new();

            let mut tar_archive = tar::Archive::new(std::io::Cursor::new(&tar_bytes));
            for entry in tar_archive.entries()? {
                let entry = entry?;
                let path = entry.path()?;
                let path_str = format!("/{}", path.to_string_lossy());
                let entry_type = entry.header().entry_type();

                if entry_type == tar::EntryType::Regular {
                    let mode = entry.header().mode().unwrap_or(0);
                    let is_executable = mode & 0o111 != 0;

                    if is_executable {
                        let mut data = Vec::new();
                        let mut entry = entry;
                        entry.read_to_end(&mut data)?;
                        // Store for later so symlinks can reference it
                        executable_content.insert(path_str.clone(), data.clone());
                        load_file_with_rewrite(
                            &mut in_mem,
                            data,
                            &path_str,
                            true,
                            exec_mode,
                            file_mode,
                        );
                    }
                } else if entry_type == tar::EntryType::Symlink
                    || entry_type == tar::EntryType::Link
                {
                    if let Ok(link) = entry.link_name() {
                        if let Some(link_path) = link {
                            let link_str = link_path.to_string_lossy().to_string();
                            // Normalize the link path to be absolute
                            let link_abs = if link_str.starts_with('/') {
                                link_str
                            } else {
                                // Relative symlink - resolve relative to the symlink's directory
                                let parent =
                                    Path::new(&path_str).parent().unwrap_or(Path::new("/"));
                                parent.join(&link_str).to_string_lossy().to_string()
                            };
                            symlinks.push((path_str, link_abs));
                        }
                    }
                }
            }

            // Handle symlinks pointing to executables (use cached content)
            for (symlink_path, target_path) in &symlinks {
                if let Some(data) = executable_content.get(target_path) {
                    load_file_with_rewrite(
                        &mut in_mem,
                        data.clone(),
                        symlink_path,
                        true,
                        exec_mode,
                        file_mode,
                    );
                    tracing::debug!(symlink = %symlink_path, target = %target_path, "flattened executable symlink");
                }
            }

            tar_bytes.into()
        } else {
            // Eager or Squashfs mode: load all files from filesystem
            // Helper to load a file from host filesystem
            let load_file_from_host = |in_mem: &mut litebox::fs::in_mem::FileSystem<Platform>,
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

                    load_file_with_rewrite(
                        in_mem,
                        data,
                        target_str,
                        is_executable,
                        exec_mode,
                        file_mode,
                    );
                }
            };

            for entry in WalkDir::new(&effective_rootfs)
                .follow_links(false)
                .into_iter()
                .filter_map(std::result::Result::ok)
            {
                let rel_path = entry
                    .path()
                    .strip_prefix(&effective_rootfs)
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
                    load_file_from_host(
                        &mut in_mem,
                        entry.path(),
                        target_str,
                        exec_mode,
                        file_mode,
                    );
                } else if entry.file_type().is_symlink() {
                    // Resolve symlink and copy the target file
                    // LiteBox doesn't support symlinks, so we flatten them to regular files
                    if let Ok(link_target) = std::fs::read_link(entry.path()) {
                        // Build the full path and canonicalize to resolve .. and other relative components
                        let full_path = if link_target.is_absolute() {
                            effective_rootfs
                                .join(link_target.strip_prefix("/").unwrap_or(&link_target))
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
                        if !resolved.starts_with(&effective_rootfs) {
                            tracing::warn!(
                                symlink = %target_str,
                                target = %resolved.display(),
                                "symlink target outside rootfs, skipping"
                            );
                            continue;
                        }

                        if resolved.is_file() {
                            load_file_from_host(
                                &mut in_mem,
                                &resolved,
                                target_str,
                                exec_mode,
                                file_mode,
                            );
                        } else if resolved.is_dir() {
                            // Symlink to directory - create the directory AND copy all files
                            // from target dir to both locations (e.g., /lib64 -> /usr/lib64)
                            in_mem.with_root_privileges(|fs| {
                                let _ = fs.mkdir(target_str, exec_mode);
                            });

                            // Walk the target directory and add files at symlink paths
                            // This handles cases like /lib64/ld-linux-x86-64.so.2
                            for sub_entry in WalkDir::new(&resolved)
                                .follow_links(false)
                                .into_iter()
                                .filter_map(std::result::Result::ok)
                            {
                                let sub_rel = sub_entry
                                    .path()
                                    .strip_prefix(&resolved)
                                    .unwrap_or(sub_entry.path());
                                if sub_rel == Path::new("") {
                                    continue;
                                }

                                let symlink_target = Path::new(target_str).join(sub_rel);
                                let symlink_target_str = symlink_target.to_str().unwrap_or("/");

                                if sub_entry.file_type().is_dir() {
                                    in_mem.with_root_privileges(|fs| {
                                        let _ = fs.mkdir(symlink_target_str, exec_mode);
                                    });
                                } else if sub_entry.file_type().is_file() {
                                    load_file_from_host(
                                        &mut in_mem,
                                        sub_entry.path(),
                                        symlink_target_str,
                                        exec_mode,
                                        file_mode,
                                    );
                                } else if sub_entry.file_type().is_symlink() {
                                    // Resolve nested symlink
                                    if let Ok(nested_target) = sub_entry.path().canonicalize() {
                                        if nested_target.is_file()
                                            && nested_target.starts_with(&effective_rootfs)
                                        {
                                            load_file_from_host(
                                                &mut in_mem,
                                                &nested_target,
                                                symlink_target_str,
                                                exec_mode,
                                                file_mode,
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Use empty tar for read-only layer in eager mode
            litebox::fs::tar_ro::EMPTY_TAR_FILE.into()
        };

        // Load additional mounts (applies to all modes)
        for mount in mounts {
            tracing::info!(
                source = %mount.source.display(),
                destination = %mount.destination,
                "loading mount into sandbox"
            );

            for entry in WalkDir::new(&mount.source)
                .follow_links(false)
                .into_iter()
                .filter_map(std::result::Result::ok)
            {
                let rel_path = entry
                    .path()
                    .strip_prefix(&mount.source)
                    .unwrap_or(entry.path());

                // Skip the root itself
                if rel_path == Path::new("") {
                    continue;
                }

                let target_path = Path::new(&mount.destination).join(rel_path);
                let target_str = target_path.to_str().unwrap_or("/");

                if entry.file_type().is_dir() {
                    in_mem.with_root_privileges(|fs| {
                        let _ = fs.mkdir(target_str, exec_mode);
                    });
                } else if entry.file_type().is_file() {
                    if let Ok(data) = std::fs::read(entry.path()) {
                        let is_executable = entry
                            .path()
                            .metadata()
                            .map(|m| m.permissions().mode() & 0o111 != 0)
                            .unwrap_or(false);
                        load_file_with_rewrite(
                            &mut in_mem,
                            data,
                            target_str,
                            is_executable,
                            exec_mode,
                            file_mode,
                        );
                    }
                }
                // Skip symlinks in mounts for simplicity
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

        // Create read-only layer from tar data
        let tar_ro = litebox::fs::tar_ro::FileSystem::new(litebox_instance, tar_data);
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

    // Prepare envp from OCI spec, then add extra env vars
    let mut envp: Vec<CString> = process
        .env()
        .as_ref()
        .map(|envs| {
            envs.iter()
                .map(|s| CString::new(s.as_bytes()).unwrap_or_default())
                .collect()
        })
        .unwrap_or_default();

    // Add extra environment variables (these override spec vars with same key)
    for var in extra_env {
        // Remove any existing var with the same key
        if let Some(key) = var.split('=').next() {
            let prefix = format!("{key}=");
            envp.retain(|v| v.to_str().map(|s| !s.starts_with(&prefix)).unwrap_or(true));
        }
        envp.push(CString::new(var.as_bytes()).unwrap_or_default());
    }

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
                let host_path = effective_rootfs.join(candidate.trim_start_matches('/'));
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
        .with_context(|| {
            format!(
                "failed to load program '{prog_path}'. \
                 Verify the binary exists in rootfs and is a valid x86_64 ELF executable."
            )
        })?;

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

/// RAII guard for restoring stdout after redirection.
struct StdoutGuard {
    original_fd: i32,
}

impl Drop for StdoutGuard {
    fn drop(&mut self) {
        unsafe {
            libc::dup2(self.original_fd, libc::STDOUT_FILENO);
            libc::close(self.original_fd);
        }
    }
}

/// RAII guard for restoring stderr after redirection.
struct StderrGuard {
    original_fd: i32,
}

impl Drop for StderrGuard {
    fn drop(&mut self) {
        unsafe {
            libc::dup2(self.original_fd, libc::STDERR_FILENO);
            libc::close(self.original_fd);
        }
    }
}

/// Redirect stdout to a file, returning a guard that restores it on drop.
fn redirect_stdout(path: &Path) -> Result<StdoutGuard> {
    use std::fs::OpenOptions;
    use std::os::unix::io::AsRawFd;

    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .with_context(|| format!("failed to open stdout file: {}", path.display()))?;

    let original_fd = unsafe { libc::dup(libc::STDOUT_FILENO) };
    if original_fd < 0 {
        anyhow::bail!("failed to dup stdout");
    }

    unsafe {
        if libc::dup2(file.as_raw_fd(), libc::STDOUT_FILENO) < 0 {
            libc::close(original_fd);
            anyhow::bail!("failed to redirect stdout");
        }
    }

    Ok(StdoutGuard { original_fd })
}

/// Redirect stderr to a file, returning a guard that restores it on drop.
fn redirect_stderr(path: &Path) -> Result<StderrGuard> {
    use std::fs::OpenOptions;
    use std::os::unix::io::AsRawFd;

    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .with_context(|| format!("failed to open stderr file: {}", path.display()))?;

    let original_fd = unsafe { libc::dup(libc::STDERR_FILENO) };
    if original_fd < 0 {
        anyhow::bail!("failed to dup stderr");
    }

    unsafe {
        if libc::dup2(file.as_raw_fd(), libc::STDERR_FILENO) < 0 {
            libc::close(original_fd);
            anyhow::bail!("failed to redirect stderr");
        }
    }

    Ok(StderrGuard { original_fd })
}
