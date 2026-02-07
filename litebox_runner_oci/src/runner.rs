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
use rayon::prelude::*;
use walkdir::WalkDir;

/// Flag to indicate whether we need the rtld_audit library for rewriter backend
static REQUIRE_RTLD_AUDIT: AtomicBool = AtomicBool::new(false);

/// Embedded litebox-sh binary (built during compilation, statically linked)
#[cfg(target_arch = "x86_64")]
static LITEBOX_SH_BINARY: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/litebox-sh"));

/// Shell binaries that should be replaced with litebox-sh.
const SHELL_NAMES: &[&str] = &[
    "sh",
    "/bin/sh",
    "/usr/bin/sh",
    "bash",
    "/bin/bash",
    "/usr/bin/bash",
    "dash",
    "/bin/dash",
    "/usr/bin/dash",
    "ash",
    "/bin/ash",
];

/// Shell builtins that don't require fork.
const SHELL_BUILTINS: &[&str] = &[
    "echo", "cd", "pwd", "export", "unset", "exit", "test", "[", "true", "false", "set", "exec",
    "source", ".", "read", ":", "type", "hash", "umask", "alias", "unalias", "readonly", "shift",
    "wait", "trap", "return", "break", "continue", "eval", "local", "declare", "typeset", "printf",
    "kill", "getopts", "let",
];

/// Rewrite shell entrypoint args for fork-free compatibility.
///
/// Layer 1: Replace `sh -c "..."` or `sh /script.sh` with litebox-sh
/// Layer 2: Add `exec` before final external command in `-c` string
///
/// Returns the (possibly modified) args and whether any rewriting occurred.
fn rewrite_shell_args(args: Vec<String>) -> Vec<String> {
    if args.is_empty() {
        return args;
    }

    if !SHELL_NAMES.contains(&args[0].as_str()) {
        return args;
    }

    // Pattern 1: sh -c "inline script"
    if args.len() >= 3 && args[1] == "-c" {
        let original_shell = &args[0];
        let mut new_args = args.clone();

        new_args[0] = "/bin/litebox-sh".to_string();
        tracing::info!(
            original = %original_shell,
            "rewriting shell entrypoint: {} -> /bin/litebox-sh",
            original_shell
        );

        // Layer 2: Add exec before final external command in -c string
        let script = &args[2];
        if let Some(rewritten) = add_exec_to_final_command(script) {
            tracing::info!(
                original = %script,
                rewritten = %rewritten,
                "added exec before final external command"
            );
            new_args[2] = rewritten;
        }

        return new_args;
    }

    // Pattern 2: sh /script.sh [args...] or sh script.sh [args...]
    if args.len() >= 2 && args[1] != "-c" && !args[1].starts_with('-') {
        let original_shell = &args[0];
        let mut new_args = args.clone();
        new_args[0] = "/bin/litebox-sh".to_string();
        tracing::info!(
            original = %original_shell,
            script = %args[1],
            "rewriting shell script entrypoint: {} -> /bin/litebox-sh",
            original_shell
        );
        return new_args;
    }

    args
}

/// Parse a `-c` script string and add `exec` before the final external command.
///
/// Only applies if the final command is external (not a builtin) and doesn't
/// already have an `exec` prefix. Returns None if no change is needed.
fn add_exec_to_final_command(script: &str) -> Option<String> {
    // Split by operators (&&, ||, ;) to find the last command segment
    // We need to track positions to reconstruct the string
    let mut segments: Vec<(usize, usize)> = Vec::new();
    let bytes = script.as_bytes();
    let mut i = 0;
    let mut seg_start = 0;
    let mut in_single_quote = false;
    let mut in_double_quote = false;

    while i < bytes.len() {
        match bytes[i] {
            b'\'' if !in_double_quote => in_single_quote = !in_single_quote,
            b'"' if !in_single_quote => in_double_quote = !in_double_quote,
            b'\\' if !in_single_quote => {
                i += 1; // skip escaped char
            }
            b'&' if !in_single_quote && !in_double_quote => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'&' {
                    segments.push((seg_start, i));
                    i += 2;
                    seg_start = i;
                    continue;
                }
            }
            b'|' if !in_single_quote && !in_double_quote => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'|' {
                    segments.push((seg_start, i));
                    i += 2;
                    seg_start = i;
                    continue;
                }
            }
            b';' if !in_single_quote && !in_double_quote => {
                segments.push((seg_start, i));
                i += 1;
                seg_start = i;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    segments.push((seg_start, bytes.len()));

    // Get the last non-empty segment
    let (last_start, last_end) = segments.last()?;
    let last_cmd = script[*last_start..*last_end].trim();

    if last_cmd.is_empty() {
        return None;
    }

    // Extract the command name (first word, ignoring variable assignments)
    let cmd_name = extract_command_name(last_cmd)?;

    // Skip if it's already an exec or a builtin
    if cmd_name == "exec" || SHELL_BUILTINS.contains(&cmd_name.as_str()) {
        return None;
    }

    // Add exec before the final command
    let prefix = &script[..*last_start];
    let last_trimmed_start =
        *last_start + (script[*last_start..].len() - script[*last_start..].trim_start().len());
    let spacing = &script[*last_start..last_trimmed_start];
    let last_content = script[last_trimmed_start..].trim_end();
    let trailing = &script[last_trimmed_start + last_content.len()..];

    Some(format!("{prefix}{spacing}exec {last_content}{trailing}"))
}

/// Extract the command name from a command string, skipping leading
/// variable assignments (e.g., `FOO=bar cmd` → `cmd`).
fn extract_command_name(cmd: &str) -> Option<String> {
    for word in cmd.split_whitespace() {
        // Skip variable assignments
        if word.contains('=') && !word.starts_with('=') && !word.starts_with('-') {
            continue;
        }
        // Strip path to get base command name
        let base = if let Some(pos) = word.rfind('/') {
            &word[pos + 1..]
        } else {
            word
        };
        return Some(base.to_string());
    }
    None
}

/// Shell interpreter paths that should be replaced in shebangs.
const SHEBANG_SHELLS: &[&str] = &[
    "#!/bin/sh",
    "#!/usr/bin/sh",
    "#!/bin/bash",
    "#!/usr/bin/bash",
    "#!/bin/dash",
    "#!/usr/bin/dash",
    "#!/bin/ash",
    "#!/usr/bin/env sh",
    "#!/usr/bin/env bash",
    "#!/usr/bin/env dash",
];

/// Rewrite shell shebangs in script files to use litebox-sh.
///
/// Detects files starting with `#!/bin/sh` (or similar) and replaces
/// the interpreter with `#!/bin/litebox-sh`. Returns None if no change needed.
fn rewrite_shell_shebang(data: &[u8]) -> Option<Vec<u8>> {
    // Must start with #!
    if data.len() < 2 || data[0] != b'#' || data[1] != b'!' {
        return None;
    }

    // Find end of first line
    let line_end = data.iter().position(|&b| b == b'\n').unwrap_or(data.len());
    let shebang_line = std::str::from_utf8(&data[..line_end]).ok()?;

    // Check if shebang matches a known shell
    let trimmed = shebang_line.trim();
    for shell in SHEBANG_SHELLS {
        if trimmed == *shell || trimmed.starts_with(&format!("{shell} ")) {
            let mut result = b"#!/bin/litebox-sh".to_vec();
            // Preserve any flags after the interpreter (e.g., "#!/bin/sh -e")
            let suffix = &trimmed[shell.len()..];
            if !suffix.is_empty() {
                result.extend_from_slice(suffix.as_bytes());
            }
            result.extend_from_slice(&data[line_end..]);
            tracing::debug!(
                original = %trimmed,
                "rewrote shebang to #!/bin/litebox-sh"
            );
            return Some(result);
        }
    }
    None
}

/// Split a shell script string into pipeline stages at unquoted `|` characters.
///
/// Returns `None` if no pipes are found (single command).
/// Respects single/double quoting and backslash escaping.
fn split_pipeline(script: &str) -> Option<Vec<String>> {
    let mut stages = Vec::new();
    let mut current = String::new();
    let mut chars = script.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;

    while let Some(ch) = chars.next() {
        match ch {
            '\\' if !in_single => {
                current.push(ch);
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            '\'' if !in_double => {
                in_single = !in_single;
                current.push(ch);
            }
            '"' if !in_single => {
                in_double = !in_double;
                current.push(ch);
            }
            '|' if !in_single && !in_double => {
                // Check for || (logical OR) — not a pipe
                if chars.peek() == Some(&'|') {
                    current.push('|');
                    current.push(chars.next().unwrap());
                } else {
                    let trimmed = current.trim().to_string();
                    if trimmed.is_empty() {
                        return None; // malformed
                    }
                    stages.push(trimmed);
                    current.clear();
                }
            }
            _ => current.push(ch),
        }
    }

    let trimmed = current.trim().to_string();
    if !trimmed.is_empty() {
        stages.push(trimmed);
    }

    if stages.len() > 1 { Some(stages) } else { None }
}

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

/// Compute a hash of file contents for cache key using xxhash (10x faster than DefaultHasher)
fn hash_bytes(data: &[u8]) -> String {
    use xxhash_rust::xxh3::xxh3_64;
    format!("{:016x}_{:08x}", xxh3_64(data), data.len())
}

/// Resolve a symlink within the rootfs context, handling absolute symlinks
/// that would otherwise escape the rootfs boundary.
///
/// Unlike `canonicalize()`, this resolves absolute symlinks relative to the
/// rootfs (e.g., `/lib/foo` → `<rootfs>/lib/foo`). Prevents symlink chains
/// from escaping the rootfs.
fn resolve_in_rootfs(path: &Path, rootfs: &Path, max_depth: u32) -> Option<PathBuf> {
    if max_depth == 0 {
        return None; // prevent infinite loops
    }

    let metadata = path.symlink_metadata().ok()?;
    if !metadata.file_type().is_symlink() {
        // Not a symlink — return as-is if it exists
        return if path.exists() {
            Some(path.to_path_buf())
        } else {
            None
        };
    }

    let link_target = std::fs::read_link(path).ok()?;
    let resolved = if link_target.is_absolute() {
        // Absolute symlink: resolve within rootfs
        rootfs.join(link_target.strip_prefix("/").unwrap_or(&link_target))
    } else {
        // Relative symlink
        path.parent()?.join(&link_target)
    };

    // Recursively resolve if it's another symlink
    resolve_in_rootfs(&resolved, rootfs, max_depth - 1)
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

/// Tar data that can be either owned or memory-mapped for zero-copy access.
enum TarSource {
    Owned(Vec<u8>),
    Mmap(memmap2::Mmap),
}

impl AsRef<[u8]> for TarSource {
    fn as_ref(&self) -> &[u8] {
        match self {
            TarSource::Owned(v) => v.as_slice(),
            TarSource::Mmap(m) => m.as_ref(),
        }
    }
}

/// Create a tar archive from the rootfs directory.
/// Returns the tar data, either memory-mapped (for cached) or owned (for new).
fn create_tar_from_rootfs(rootfs_path: &Path) -> Result<TarSource> {
    // Hash the rootfs path for cache key
    let cache_key = hash_bytes(rootfs_path.to_string_lossy().as_bytes());
    let cache_path = tar_cache_dir().join(format!("{cache_key}.tar"));

    // Check cache first - use mmap for zero-copy access
    if cache_path.exists() {
        tracing::debug!(path = %cache_path.display(), "using cached tar (mmap)");
        let file = std::fs::File::open(&cache_path)
            .with_context(|| format!("failed to open cached tar: {}", cache_path.display()))?;
        // SAFETY: The file is read-only and we don't modify it
        let mmap = unsafe { memmap2::Mmap::map(&file) }
            .with_context(|| format!("failed to mmap cached tar: {}", cache_path.display()))?;
        return Ok(TarSource::Mmap(mmap));
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

    Ok(TarSource::Owned(tar_data))
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
    /// Path to redirect stdin from
    pub stdin: Option<PathBuf>,
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
        true,
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
        true,
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
        true,
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
    rewrite_shell: bool,
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
        rewrite_shell,
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
    rewrite_shell: bool,
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
        rewrite_shell,
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
    rewrite_shell: bool,
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
        rewrite_shell,
    )
}

/// Run an OCI container with lazy executable rewriting.
///
/// This mode combines the benefits of lazy loading with on-demand executable rewriting:
/// - Only critical executables (dynamic linker, main binary) are rewritten upfront
/// - Other executables are lazily rewritten when first accessed
/// - Significantly faster startup for images with many executables
///
/// This is the fastest option for most workloads.
pub fn run_container_lazy_rewrite(
    bundle_path: &Path,
    override_args: Option<&[String]>,
    extra_env: &[String],
    mounts: &[Mount],
    stdio: &StdioRedirect,
    network: &NetworkConfig,
    rewrite_shell: bool,
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
        LazyMode::LazyRewrite,
        rewrite_shell,
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
    /// Lazy rewriting - only critical executables eagerly, rest lazily transformed
    LazyRewrite,
}

/// Syscall rewriter implementing the ExecutableTransform trait for lazy rewriting.
struct SyscallRewriter;

impl litebox::fs::layered::ExecutableTransform for SyscallRewriter {
    fn transform(&self, content: &[u8]) -> Option<Vec<u8>> {
        // Use cached rewriting
        let rewritten = rewrite_with_cache(content);
        // Only return Some if the content was actually modified
        if rewritten.len() != content.len() || rewritten != content {
            Some(rewritten)
        } else {
            None
        }
    }
}

/// Execute a shell pipeline sequentially, connecting stages via temp files.
///
/// Each stage runs as a separate process (via re-exec of litebox_runner_oci)
/// because the LiteBox platform can only be initialized once per process.
/// Stage N's stdout is captured to a temp file, which becomes stage N+1's stdin.
fn run_pipeline(
    stages: &[String],
    bundle_path: &Path,
    original_args: &[String],
    extra_env: &[String],
    _mounts: &[Mount],
    _network: &NetworkConfig,
    _lazy_mode: LazyMode,
) -> Result<i32> {
    use std::fs;

    let temp_dir = std::env::temp_dir();
    let pipe_id = std::process::id();
    let mut prev_file: Option<PathBuf> = None;
    let mut temp_files: Vec<PathBuf> = Vec::new();
    let mut last_exit_code = 0;

    // Get our own executable path for re-exec
    let self_exe = std::env::current_exe().context("failed to get current executable path")?;

    for (i, stage) in stages.iter().enumerate() {
        let is_last = i == stages.len() - 1;

        // Build the -c script for this stage, adding exec for final command
        let stage_script = if let Some(rewritten) = add_exec_to_final_command(stage) {
            rewritten
        } else {
            stage.clone()
        };

        tracing::info!(
            stage = i + 1,
            total = stages.len(),
            cmd = %stage,
            "executing pipeline stage"
        );

        // Build command: litebox_runner_oci run --bundle <path> <container-id>
        // with overridden args via config.json rewrite
        let container_id = format!("pipe-{}-{}", pipe_id, i);

        // Write a temporary config.json with this stage's args
        let stage_config_dir = temp_dir.join(format!(".litebox_pipe_cfg_{}_{}", pipe_id, i));
        fs::create_dir_all(&stage_config_dir)?;

        // Read original config and override args
        let spec_path = bundle_path.join("config.json");
        let mut spec: serde_json::Value = serde_json::from_reader(
            fs::File::open(&spec_path).context("failed to open config.json")?,
        )?;

        // Override process.args to run this stage
        let stage_args = serde_json::json!([
            original_args[0], // /bin/litebox-sh
            "-c",
            stage_script
        ]);
        spec["process"]["args"] = stage_args;

        // Point root.path to the original rootfs using absolute path
        let rootfs_src = bundle_path.join(spec["root"]["path"].as_str().unwrap_or("rootfs"));
        let rootfs_abs = rootfs_src.canonicalize().unwrap_or(rootfs_src);
        spec["root"]["path"] = serde_json::json!(rootfs_abs.to_str().unwrap_or("rootfs"));

        fs::write(
            stage_config_dir.join("config.json"),
            serde_json::to_string_pretty(&spec)?,
        )?;
        temp_files.push(stage_config_dir.clone());

        // Set up output temp file for non-last stages
        let output_file = if !is_last {
            let path = temp_dir.join(format!(".litebox_pipe_{}_{}", pipe_id, i));
            temp_files.push(path.clone());
            Some(path)
        } else {
            None
        };

        // Build the command
        let mut cmd = std::process::Command::new(&self_exe);
        cmd.arg("run")
            .arg("--bundle")
            .arg(&stage_config_dir)
            .arg(&container_id);

        // Add extra env
        for env_var in extra_env {
            cmd.arg("--env").arg(env_var);
        }

        // Set up stdin from previous stage
        if let Some(ref prev) = prev_file {
            let stdin_file =
                fs::File::open(prev).context("failed to open pipe input from previous stage")?;
            cmd.stdin(std::process::Stdio::from(stdin_file));
        }

        // Set up stdout to temp file for non-last stages
        if let Some(ref out_path) = output_file {
            let stdout_file =
                fs::File::create(out_path).context("failed to create pipe output file")?;
            cmd.stdout(std::process::Stdio::from(stdout_file));
        }

        // Suppress stderr for pipeline stages (audit/debug noise)
        cmd.stderr(std::process::Stdio::null());

        // Run the stage
        let status = cmd
            .status()
            .with_context(|| format!("failed to execute pipeline stage {}", i + 1))?;
        last_exit_code = status.code().unwrap_or(1);

        prev_file = output_file;
    }

    // Clean up temp files and directories
    for path in temp_files.iter().rev() {
        if path.is_dir() {
            let _ = fs::remove_dir_all(path);
        } else {
            let _ = fs::remove_file(path);
        }
    }

    Ok(last_exit_code)
}

/// Internal implementation that handles both regular run and exec.
#[allow(clippy::too_many_arguments)]
fn run_container_internal(
    bundle_path: &Path,
    override_args: Option<&[String]>,
    extra_env: &[String],
    mounts: &[Mount],
    stdio: &StdioRedirect,
    network: &NetworkConfig,
    lazy_mode: LazyMode,
    rewrite_shell: bool,
) -> Result<i32> {
    // Set up stdio redirection before running
    let _stdin_guard = if let Some(path) = &stdio.stdin {
        Some(redirect_stdin(path)?)
    } else {
        None
    };
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

    // Rewrite shell entrypoints for fork-free compatibility
    let args = if rewrite_shell {
        rewrite_shell_args(args)
    } else {
        args
    };

    // Detect pipeline patterns in -c script strings and execute sequentially
    if rewrite_shell && args.len() >= 3 && args[1] == "-c" {
        if let Some(stages) = split_pipeline(&args[2]) {
            tracing::info!(
                stages = stages.len(),
                pipeline = %args[2],
                "detected pipeline, executing stages sequentially"
            );
            return run_pipeline(
                &stages,
                bundle_path,
                &args,
                extra_env,
                mounts,
                network,
                lazy_mode,
            );
        }
    }

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
            } else if rewrite_shell {
                // Rewrite shell shebangs in script files
                if let Some(rewritten) = rewrite_shell_shebang(&data) {
                    tracing::debug!(path = %target_str, "rewrote shell shebang");
                    rewritten.into()
                } else {
                    data.into()
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
        };

        // For TarLayered mode, create tar and only load executables into upper layer
        let tar_data: std::borrow::Cow<'static, [u8]> = if lazy_mode == LazyMode::TarLayered
            || lazy_mode == LazyMode::LazyRewrite
        {
            if lazy_mode == LazyMode::LazyRewrite {
                tracing::info!("using lazy rewriting with tar + layered filesystem");
            } else {
                tracing::info!("using true lazy loading with tar + layered filesystem");
            }
            let tar_source = create_tar_from_rootfs(&rootfs_path)?;
            let tar_bytes = tar_source.as_ref();

            // For LazyRewrite mode, only eagerly load critical executables
            // (dynamic linkers and main binary). Other executables are lazily rewritten.
            // Determine main binary path for LazyRewrite mode
            let main_binary = &args[0];
            let main_binary_paths: Vec<String> = if main_binary.starts_with('/') {
                vec![main_binary.clone()]
            } else {
                // Search in common paths
                vec![
                    format!("/bin/{main_binary}"),
                    format!("/usr/bin/{main_binary}"),
                    format!("/sbin/{main_binary}"),
                    format!("/usr/sbin/{main_binary}"),
                    format!("/usr/local/bin/{main_binary}"),
                ]
            };

            // Single pass: collect executables and symlinks, load executable content
            // Symlinks pointing to executables need the rewritten content
            let mut symlinks: Vec<(String, String)> = Vec::new();
            let mut executable_content: std::collections::HashMap<String, Vec<u8>> =
                std::collections::HashMap::new();

            // For LazyRewrite mode, do a first pass to collect symlinks
            // so we can resolve the main binary's symlink target
            let mut critical_targets: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            if lazy_mode == LazyMode::LazyRewrite {
                let mut tar_archive = tar::Archive::new(std::io::Cursor::new(tar_bytes));
                for entry in tar_archive.entries().into_iter().flatten() {
                    let Ok(entry) = entry else {
                        continue;
                    };
                    let Ok(path) = entry.path() else {
                        continue;
                    };
                    let path_str = format!("/{}", path.to_string_lossy());
                    let entry_type = entry.header().entry_type();

                    if entry_type == tar::EntryType::Symlink || entry_type == tar::EntryType::Link {
                        // Check if this symlink is one of the main binary paths
                        if main_binary_paths.iter().any(|p| p == &path_str) {
                            if let Ok(Some(link_path)) = entry.link_name() {
                                let link_str = link_path.to_string_lossy().to_string();
                                let link_abs = if link_str.starts_with('/') {
                                    link_str
                                } else {
                                    let parent =
                                        Path::new(&path_str).parent().unwrap_or(Path::new("/"));
                                    parent.join(&link_str).to_string_lossy().to_string()
                                };
                                critical_targets.insert(link_abs);
                            }
                        }
                    }
                }
            }

            let is_critical_executable = |path: &str| -> bool {
                // Always load dynamic linkers - they must be rewritten before anything runs
                if path.contains("ld-linux") || path.contains("ld-musl") {
                    return true;
                }
                // In LazyRewrite mode, also load the main binary and its symlink targets
                if lazy_mode == LazyMode::LazyRewrite {
                    // Check if this is the main binary
                    for main_path in &main_binary_paths {
                        if path == main_path {
                            return true;
                        }
                    }
                    // Check if this is a symlink target of the main binary
                    if critical_targets.contains(path) {
                        return true;
                    }
                    return false;
                }
                // In TarLayered mode, load all executables
                true
            };

            // First pass: collect all executable data from tar
            let mut executables_to_rewrite: Vec<(String, Vec<u8>)> = Vec::new();
            let mut tar_archive = tar::Archive::new(std::io::Cursor::new(tar_bytes));
            for entry in tar_archive.entries()? {
                let entry = entry?;
                let path = entry.path()?;
                let path_str = format!("/{}", path.to_string_lossy());
                let entry_type = entry.header().entry_type();

                if entry_type == tar::EntryType::Regular {
                    let mode = entry.header().mode().unwrap_or(0);
                    let is_executable = mode & 0o111 != 0;

                    if is_executable && is_critical_executable(&path_str) {
                        let mut data = Vec::new();
                        let mut entry = entry;
                        entry.read_to_end(&mut data)?;
                        executables_to_rewrite.push((path_str, data));
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

            // Parallel rewrite executables using rayon
            let rewritten_executables: Vec<(String, Vec<u8>)> = executables_to_rewrite
                .into_par_iter()
                .map(|(path, data)| {
                    let rewritten = rewrite_with_cache(&data);
                    if rewritten.len() != data.len() {
                        tracing::debug!(path = %path, "rewrote syscalls in executable");
                    }
                    (path, rewritten)
                })
                .collect();

            // Load rewritten executables into filesystem (sequential - filesystem not thread-safe)
            for (path_str, data) in &rewritten_executables {
                executable_content.insert(path_str.clone(), data.clone());

                in_mem.with_root_privileges(|fs| {
                    // Ensure parent directories exist
                    let target_path = Path::new(path_str);
                    if let Some(parent) = target_path.parent() {
                        let mut current = std::path::PathBuf::from("/");
                        for component in parent.components().skip(1) {
                            current.push(component);
                            let _ = fs.mkdir(current.to_str().unwrap(), exec_mode);
                        }
                    }

                    let fd = fs
                        .open(
                            path_str,
                            litebox::fs::OFlags::WRONLY | litebox::fs::OFlags::CREAT,
                            exec_mode,
                        )
                        .expect("failed to create file in sandbox");
                    fs.initialize_primarily_read_heavy_file(&fd, data.clone().into());
                    fs.close(&fd).expect("failed to close file");
                });
            }

            // Handle symlinks pointing to executables (use cached content)
            for (symlink_path, target_path) in &symlinks {
                if let Some(data) = executable_content.get(target_path) {
                    in_mem.with_root_privileges(|fs| {
                        // Ensure parent directories exist
                        let target_path_obj = Path::new(symlink_path);
                        if let Some(parent) = target_path_obj.parent() {
                            let mut current = std::path::PathBuf::from("/");
                            for component in parent.components().skip(1) {
                                current.push(component);
                                let _ = fs.mkdir(current.to_str().unwrap(), exec_mode);
                            }
                        }

                        let fd = fs
                            .open(
                                symlink_path,
                                litebox::fs::OFlags::WRONLY | litebox::fs::OFlags::CREAT,
                                exec_mode,
                            )
                            .expect("failed to create file in sandbox");
                        fs.initialize_primarily_read_heavy_file(&fd, data.clone().into());
                        fs.close(&fd).expect("failed to close file");
                    });
                    tracing::debug!(symlink = %symlink_path, target = %target_path, "flattened executable symlink");
                }
            }

            // Convert TarSource to Cow for the tar filesystem
            // For mmap, we leak the memory to get a 'static lifetime (container runs once anyway)
            let tar_data: std::borrow::Cow<'static, [u8]> = match tar_source {
                TarSource::Owned(v) => v.into(),
                TarSource::Mmap(m) => {
                    // Leak the mmap to get 'static lifetime - this is fine since the container
                    // process will exit and release all memory anyway
                    let leaked: &'static [u8] = Box::leak(m.to_vec().into_boxed_slice());
                    std::borrow::Cow::Borrowed(leaked)
                }
            };
            tar_data
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
                    // Resolve symlink within rootfs context
                    // Uses rootfs-aware resolution to handle absolute symlinks correctly
                    let Some(resolved) = resolve_in_rootfs(entry.path(), &effective_rootfs, 10)
                    else {
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
                                // Resolve nested symlink within rootfs
                                if let Some(nested_target) =
                                    resolve_in_rootfs(sub_entry.path(), &effective_rootfs, 10)
                                {
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

        // Inject litebox-sh (fork-free shell) into rootfs when shell rewriting is enabled
        #[cfg(target_arch = "x86_64")]
        if rewrite_shell {
            let rewritten = rewrite_with_cache(LITEBOX_SH_BINARY);
            in_mem.with_root_privileges(|fs| {
                let rwxr_xr_x = Mode::RWXU | Mode::RGRP | Mode::XGRP | Mode::ROTH | Mode::XOTH;
                let _ = fs.mkdir("/bin", rwxr_xr_x);
                let fd = fs
                    .open(
                        "/bin/litebox-sh",
                        litebox::fs::OFlags::WRONLY | litebox::fs::OFlags::CREAT,
                        rwxr_xr_x,
                    )
                    .expect("Failed to create /bin/litebox-sh");
                fs.initialize_primarily_read_heavy_file(&fd, rewritten.into());
                fs.close(&fd).expect("Failed to close /bin/litebox-sh");
            });
            tracing::debug!("injected /bin/litebox-sh into rootfs");
        }

        // Create read-only layer from tar data
        let tar_ro = litebox::fs::tar_ro::FileSystem::new(litebox_instance, tar_data);

        // For LazyRewrite mode, use executable transform to rewrite on first access
        if lazy_mode == LazyMode::LazyRewrite {
            shim_builder.default_fs_with_transform(in_mem, tar_ro, Box::new(SyscallRewriter))
        } else {
            shim_builder.default_fs(in_mem, tar_ro)
        }
    };

    shim_builder.set_fs(initial_fs);
    shim_builder.set_load_filter(fixup_env);
    let shim = shim_builder.build();

    // Using rewriter backend - no seccomp setup needed
    // The syscalls have been rewritten in the ELF files

    // If the entrypoint is a script file (not ELF), rewrite to use litebox-sh as interpreter
    let args = if rewrite_shell && !args.is_empty() && !SHELL_NAMES.contains(&args[0].as_str()) {
        let entry = &args[0];
        let host_path = if entry.starts_with('/') {
            effective_rootfs.join(entry.trim_start_matches('/'))
        } else {
            effective_rootfs.join(entry)
        };
        if host_path.exists() {
            if let Ok(header) = std::fs::read(&host_path).map(|d| d[..d.len().min(128)].to_vec()) {
                if header.starts_with(b"#!") {
                    // It's a script — check if shebang is a shell
                    let line_end = header
                        .iter()
                        .position(|&b| b == b'\n')
                        .unwrap_or(header.len());
                    let shebang = String::from_utf8_lossy(&header[..line_end]);
                    let is_shell_shebang = SHEBANG_SHELLS.iter().any(|s| {
                        shebang.trim() == *s || shebang.trim().starts_with(&format!("{s} "))
                    });
                    if is_shell_shebang {
                        tracing::info!(
                            script = %entry,
                            shebang = %shebang.trim(),
                            "script entrypoint detected, prepending /bin/litebox-sh"
                        );
                        let mut new_args = vec!["/bin/litebox-sh".to_string()];
                        new_args.extend(args);
                        new_args
                    } else {
                        args
                    }
                } else {
                    args
                }
            } else {
                args
            }
        } else {
            args
        }
    } else {
        args
    };

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

    // Set initial working directory from OCI spec
    let cwd = process.cwd().as_path().to_str().unwrap_or("/");
    if cwd != "/" {
        program.set_cwd(cwd);
    }

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

/// RAII guard for restoring stdin after redirection.
struct StdinGuard {
    original_fd: i32,
}

impl Drop for StdinGuard {
    fn drop(&mut self) {
        unsafe {
            libc::dup2(self.original_fd, libc::STDIN_FILENO);
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

/// Redirect stdin from a file, returning a guard that restores it on drop.
fn redirect_stdin(path: &Path) -> Result<StdinGuard> {
    use std::fs::OpenOptions;
    use std::os::unix::io::AsRawFd;

    let file = OpenOptions::new()
        .read(true)
        .open(path)
        .with_context(|| format!("failed to open stdin file: {}", path.display()))?;

    // Safety: dup/dup2 are standard POSIX calls, single-threaded at this point
    let original_fd = unsafe { libc::dup(libc::STDIN_FILENO) };
    if original_fd < 0 {
        anyhow::bail!("failed to dup stdin");
    }

    unsafe {
        if libc::dup2(file.as_raw_fd(), libc::STDIN_FILENO) < 0 {
            libc::close(original_fd);
            anyhow::bail!("failed to redirect stdin");
        }
    }

    Ok(StdinGuard { original_fd })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rewrite_shell_args_replaces_sh() {
        let args = vec!["sh".to_string(), "-c".to_string(), "echo hello".to_string()];
        let result = rewrite_shell_args(args);
        assert_eq!(result[0], "/bin/litebox-sh");
        assert_eq!(result[1], "-c");
    }

    #[test]
    fn test_rewrite_shell_args_replaces_bin_sh() {
        let args = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "echo hello".to_string(),
        ];
        let result = rewrite_shell_args(args);
        assert_eq!(result[0], "/bin/litebox-sh");
    }

    #[test]
    fn test_rewrite_shell_args_replaces_bash() {
        let args = vec![
            "/bin/bash".to_string(),
            "-c".to_string(),
            "echo hello".to_string(),
        ];
        let result = rewrite_shell_args(args);
        assert_eq!(result[0], "/bin/litebox-sh");
    }

    #[test]
    fn test_rewrite_shell_args_ignores_non_shell() {
        let args = vec![
            "/usr/bin/python3".to_string(),
            "-c".to_string(),
            "print('hi')".to_string(),
        ];
        let result = rewrite_shell_args(args.clone());
        assert_eq!(result, args);
    }

    #[test]
    fn test_rewrite_shell_args_rewrites_script_file() {
        let args = vec!["sh".to_string(), "/entrypoint.sh".to_string()];
        let result = rewrite_shell_args(args);
        assert_eq!(result[0], "/bin/litebox-sh");
        assert_eq!(result[1], "/entrypoint.sh");
    }

    #[test]
    fn test_rewrite_shell_args_ignores_direct_exec() {
        let args = vec!["/bin/ls".to_string(), "/".to_string()];
        let result = rewrite_shell_args(args.clone());
        assert_eq!(result, args);
    }

    #[test]
    fn test_add_exec_to_final_external_command() {
        let result = add_exec_to_final_command("export FOO=bar && /app/server");
        assert_eq!(
            result,
            Some("export FOO=bar && exec /app/server".to_string())
        );
    }

    #[test]
    fn test_add_exec_skips_builtin_final() {
        let result = add_exec_to_final_command("cd /app && echo hello");
        assert_eq!(result, None);
    }

    #[test]
    fn test_add_exec_skips_already_exec() {
        let result = add_exec_to_final_command("export FOO=bar && exec /app/server");
        assert_eq!(result, None);
    }

    #[test]
    fn test_add_exec_single_external() {
        let result = add_exec_to_final_command("/app/server --port 8080");
        assert_eq!(result, Some("exec /app/server --port 8080".to_string()));
    }

    #[test]
    fn test_add_exec_single_builtin() {
        let result = add_exec_to_final_command("echo hello");
        assert_eq!(result, None);
    }

    #[test]
    fn test_add_exec_with_or_operator() {
        let result = add_exec_to_final_command("test -f /app/config || /app/setup");
        assert_eq!(
            result,
            Some("test -f /app/config || exec /app/setup".to_string())
        );
    }

    #[test]
    fn test_add_exec_with_semicolon() {
        let result = add_exec_to_final_command("export PATH=/app:$PATH; /app/server");
        assert_eq!(
            result,
            Some("export PATH=/app:$PATH; exec /app/server".to_string())
        );
    }

    #[test]
    fn test_add_exec_relative_command() {
        let result = add_exec_to_final_command("cd /app && ./server");
        assert_eq!(result, Some("cd /app && exec ./server".to_string()));
    }

    #[test]
    fn test_add_exec_preserves_quotes() {
        let result = add_exec_to_final_command(r#"export FOO="hello world" && /app/server"#);
        assert_eq!(
            result,
            Some(r#"export FOO="hello world" && exec /app/server"#.to_string())
        );
    }

    #[test]
    fn test_add_exec_complex_chain() {
        let result = add_exec_to_final_command("export A=1 && export B=2 && /app/server --flag");
        assert_eq!(
            result,
            Some("export A=1 && export B=2 && exec /app/server --flag".to_string())
        );
    }

    #[test]
    fn test_extract_command_name_absolute() {
        assert_eq!(
            extract_command_name("/usr/bin/python3 script.py"),
            Some("python3".to_string())
        );
    }

    #[test]
    fn test_extract_command_name_relative() {
        assert_eq!(
            extract_command_name("./server --port 8080"),
            Some("server".to_string())
        );
    }

    #[test]
    fn test_extract_command_name_with_env() {
        assert_eq!(
            extract_command_name("FOO=bar /app/server"),
            Some("server".to_string())
        );
    }

    #[test]
    fn test_extract_command_name_simple() {
        assert_eq!(extract_command_name("echo hello"), Some("echo".to_string()));
    }

    #[test]
    fn test_full_rewrite_adds_exec() {
        let args = vec![
            "sh".to_string(),
            "-c".to_string(),
            "export FOO=bar && /app/server".to_string(),
        ];
        let result = rewrite_shell_args(args);
        assert_eq!(result[0], "/bin/litebox-sh");
        assert_eq!(result[2], "export FOO=bar && exec /app/server");
    }

    #[test]
    fn test_full_rewrite_all_builtins_no_exec() {
        let args = vec![
            "sh".to_string(),
            "-c".to_string(),
            "export FOO=bar && echo hello".to_string(),
        ];
        let result = rewrite_shell_args(args);
        assert_eq!(result[0], "/bin/litebox-sh");
        // No exec added since echo is a builtin
        assert_eq!(result[2], "export FOO=bar && echo hello");
    }

    // ── Script file args rewriting ──

    #[test]
    fn test_rewrite_shell_script_file() {
        let args = vec!["sh".to_string(), "/entrypoint.sh".to_string()];
        let result = rewrite_shell_args(args);
        assert_eq!(result[0], "/bin/litebox-sh");
        assert_eq!(result[1], "/entrypoint.sh");
    }

    #[test]
    fn test_rewrite_shell_script_with_args() {
        let args = vec![
            "/bin/bash".to_string(),
            "/start.sh".to_string(),
            "--port".to_string(),
            "8080".to_string(),
        ];
        let result = rewrite_shell_args(args);
        assert_eq!(result[0], "/bin/litebox-sh");
        assert_eq!(result[1], "/start.sh");
        assert_eq!(result[2], "--port");
    }

    #[test]
    fn test_rewrite_ignores_shell_with_flags() {
        // sh -e shouldn't be treated as a script file
        let args = vec!["sh".to_string(), "-e".to_string()];
        let result = rewrite_shell_args(args.clone());
        assert_eq!(result, args);
    }

    // ── Shebang rewriting ──

    #[test]
    fn test_shebang_rewrite_bin_sh() {
        let data = b"#!/bin/sh\necho hello\n";
        let result = rewrite_shell_shebang(data).unwrap();
        assert_eq!(&result[..18], b"#!/bin/litebox-sh\n");
        assert!(result.ends_with(b"echo hello\n"));
    }

    #[test]
    fn test_shebang_rewrite_bin_bash() {
        let data = b"#!/bin/bash\nset -e\necho hi\n";
        let result = rewrite_shell_shebang(data).unwrap();
        assert!(result.starts_with(b"#!/bin/litebox-sh\n"));
    }

    #[test]
    fn test_shebang_rewrite_usr_bin_env_sh() {
        let data = b"#!/usr/bin/env sh\necho hi\n";
        let result = rewrite_shell_shebang(data).unwrap();
        assert!(result.starts_with(b"#!/bin/litebox-sh\n"));
    }

    #[test]
    fn test_shebang_rewrite_with_flags() {
        let data = b"#!/bin/sh -e\necho hi\n";
        let result = rewrite_shell_shebang(data).unwrap();
        assert!(result.starts_with(b"#!/bin/litebox-sh -e\n"));
    }

    #[test]
    fn test_shebang_no_rewrite_python() {
        let data = b"#!/usr/bin/python3\nprint('hi')\n";
        assert!(rewrite_shell_shebang(data).is_none());
    }

    #[test]
    fn test_shebang_no_rewrite_non_script() {
        let data = b"\x7fELF binary data";
        assert!(rewrite_shell_shebang(data).is_none());
    }

    #[test]
    fn test_shebang_no_rewrite_empty() {
        assert!(rewrite_shell_shebang(b"").is_none());
    }

    // ── Pipeline parsing ──

    #[test]
    fn test_split_pipeline_simple() {
        let stages = split_pipeline("echo hello | cat").unwrap();
        assert_eq!(stages, vec!["echo hello", "cat"]);
    }

    #[test]
    fn test_split_pipeline_three_stages() {
        let stages = split_pipeline("ls / | grep bin | head -n 5").unwrap();
        assert_eq!(stages, vec!["ls /", "grep bin", "head -n 5"]);
    }

    #[test]
    fn test_split_pipeline_no_pipe() {
        assert!(split_pipeline("echo hello && echo world").is_none());
    }

    #[test]
    fn test_split_pipeline_or_not_pipe() {
        // || is logical OR, not a pipe
        assert!(split_pipeline("echo hello || echo fallback").is_none());
    }

    #[test]
    fn test_split_pipeline_pipe_in_single_quotes() {
        // Pipe inside quotes is literal
        assert!(split_pipeline("echo 'hello | world'").is_none());
    }

    #[test]
    fn test_split_pipeline_pipe_in_double_quotes() {
        assert!(split_pipeline("echo \"hello | world\"").is_none());
    }

    #[test]
    fn test_split_pipeline_mixed_pipe_and_chain() {
        let stages = split_pipeline("echo hello | grep hello && echo done").unwrap();
        assert_eq!(stages, vec!["echo hello", "grep hello && echo done"]);
    }

    #[test]
    fn test_split_pipeline_escaped_pipe() {
        // Backslash-escaped pipe is literal
        assert!(split_pipeline("echo hello \\| world").is_none());
    }

    #[test]
    fn test_split_pipeline_with_redirects() {
        let stages = split_pipeline("cat /etc/os-release | grep -i name").unwrap();
        assert_eq!(stages, vec!["cat /etc/os-release", "grep -i name"]);
    }
}
