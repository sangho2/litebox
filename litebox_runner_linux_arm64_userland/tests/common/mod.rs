// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

use std::path::{Path, PathBuf};

/// Find all dependencies of a given binary via `ldd`
#[allow(dead_code, reason = "may not be used in all test configurations")]
pub fn find_dependencies(prog: &str) -> Vec<String> {
    let output = std::process::Command::new("ldd")
        .arg(prog)
        .output()
        .expect("Failed to execute ldd");

    let dependencies = String::from_utf8_lossy(&output.stdout);
    println!("Dependencies:\n{dependencies}");

    let mut paths = Vec::new();

    for line in dependencies.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        if let Some(idx) = line.find("=>") {
            // Format: "libc.so.6 => /lib/.../libc.so.6 (0x...)"
            let right = line[idx + 2..].trim();
            // Skip "not found"
            if right.starts_with("not found") {
                println!("Warning: dependency not found: {line}");
                continue;
            }
            // Extract token before whitespace or '('
            if let Some(token) = right.split_whitespace().next()
                && token.starts_with('/')
            {
                paths.push(token.to_string());
            } else {
                println!("Warning: unexpected ldd output line: {line}");
            }
        } else {
            // Format: "/lib64/ld-linux-aarch64.so.1 (0x...)" or "linux-vdso.so.1 (0x...)"
            if let Some(token) = line.split_whitespace().next()
                && token.starts_with('/')
            {
                paths.push(token.to_string());
            }
        }
    }

    println!("Resolved dependency paths: {paths:?}");

    paths
}

/// Get the output directory for test artifacts
fn get_out_dir() -> PathBuf {
    std::env::var("OUT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
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

/// Compile C code into an executable with caching
pub fn compile(src_path: &str, unique_name: &str, exec_or_lib: bool, nolibc: bool) -> PathBuf {
    let dir_path = get_out_dir();
    let path = dir_path.join(unique_name);
    let output = path.to_str().unwrap();

    let mut args = vec!["-o", output, src_path];
    if exec_or_lib {
        args.push("-static");
    }
    if nolibc {
        args.push("-nostdlib");
    }
    // Add pthread for thread tests
    args.push("-lpthread");

    // Create command string for caching
    let mut command_parts = vec!["gcc"];
    command_parts.extend_from_slice(&args);
    let command = command_parts.join(" ");

    // Check cache first
    let src_path_buf = Path::new(src_path);
    let input_paths = vec![src_path_buf];

    if let Ok(true) = crate::cache::is_cached_and_valid(&input_paths, &path, &command) {
        println!("Using cached compilation result for: {unique_name}");
        return path;
    }

    println!("Compiling: {src_path} -> {unique_name}");

    let output = std::process::Command::new("gcc")
        .args(args)
        .output()
        .expect("Failed to compile");
    assert!(
        output.status.success(),
        "failed to compile: {:?}",
        std::str::from_utf8(output.stderr.as_slice()).unwrap()
    );

    // Create cache entry after successful compilation
    if let Err(e) = crate::cache::create_cache_entry(&input_paths, &path, &command) {
        eprintln!("Warning: Failed to create cache entry for {unique_name}: {e}");
    }

    path
}

/// Create tar file with caching
#[allow(dead_code, reason = "may not be used in all test configurations")]
pub(crate) fn create_tar_with_cache(tar_dir: &Path, tar_file: &Path, unique_name: &str) -> bool {
    // For tar files, we need to consider the entire directory tree as input
    // We'll create a hash of all files in the tar_dir
    let mut all_files = Vec::new();
    if let Ok(entries) = walkdir::WalkDir::new(tar_dir)
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
    {
        for entry in entries {
            if entry.file_type().is_file() {
                all_files.push(entry.path().to_path_buf());
            }
        }
    } else {
        return false;
    }

    // Convert to Path refs for the caching function
    let input_paths: Vec<&Path> = all_files.iter().map(std::path::PathBuf::as_path).collect();

    // Create command string for caching
    let tar_filename = format!("../rootfs_{unique_name}.tar");
    let mut args = vec![
        // ustar format allows longer file names
        "--format=ustar",
        "-cvf",
        tar_filename.as_str(),
    ];
    // collect all entries in the tar_dir
    let all_entries = tar_dir
        .read_dir()
        .expect("Failed to read tar directory")
        .filter_map(|entry| entry.map(|e| e.file_name()).ok())
        .collect::<Vec<_>>();
    for entry in &all_entries {
        args.push(entry.to_str().unwrap());
    }
    let mut command_parts = vec!["tar"];
    command_parts.extend_from_slice(&args);
    let command = command_parts.join(" ");
    println!("Tar command: {command}");

    if let Ok(true) = crate::cache::is_cached_and_valid(&input_paths, tar_file, &command) {
        println!("Using cached tar file for: {unique_name}");
        return true;
    }

    println!("Creating tar file for: {unique_name}");

    // create tar file using `tar` command
    let tar_data = std::process::Command::new("tar")
        .args(args)
        .current_dir(tar_dir)
        .output()
        .expect("Failed to create tar file");

    let success = tar_data.status.success();
    if !success {
        eprintln!(
            "failed to create tar file {:?}",
            std::str::from_utf8(tar_data.stderr.as_slice()).unwrap()
        );
        return false;
    }

    // Create cache entry after successful tar creation
    if let Err(e) = crate::cache::create_cache_entry(&input_paths, tar_file, &command) {
        eprintln!("Warning: Failed to create cache entry for {unique_name}: {e}");
    }

    success
}
