// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

use std::path::PathBuf;
use std::process::Command;

const RTLD_AUDIT_DIR: &str = "../litebox_rtld_audit";

fn main() {
    // Capture git commit hash for version info
    let git_hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map_or_else(|| "unknown".to_string(), |s| s.trim().to_string());
    println!("cargo:rustc-env=GIT_HASH={git_hash}");

    // Check for dirty working tree
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .is_some_and(|o| !o.stdout.is_empty());
    println!(
        "cargo:rustc-env=GIT_DIRTY={}",
        if dirty { "-dirty" } else { "" }
    );

    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/index");
    let mut make_cmd = std::process::Command::new("make");
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    if target_arch != "x86_64" {
        // XXX: Currently 32-bit x86 is unsupported (unimplemented), skip building
        return;
    }
    make_cmd
        .current_dir(RTLD_AUDIT_DIR)
        .env("OUT_DIR", &out_dir)
        .env("ARCH", target_arch);
    if std::env::var("PROFILE").unwrap_or_default() == "debug" {
        make_cmd.env("DEBUG", "1");
    }
    let output = make_cmd
        .output()
        .expect("Failed to execute make for rtld_audit");
    assert!(
        output.status.success(),
        "failed to build rtld_audit.so via make:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        out_dir.join("litebox_rtld_audit.so").exists(),
        "Build failed to create necessary file"
    );

    println!("cargo:rerun-if-changed={RTLD_AUDIT_DIR}/rtld_audit.c");
    println!("cargo:rerun-if-changed={RTLD_AUDIT_DIR}/Makefile");

    // Build litebox-sh (fork-free shell for container entrypoints)
    let litebox_sh_dir = PathBuf::from("tools/litebox-sh");
    let mut sh_cmd = std::process::Command::new("make");
    sh_cmd
        .current_dir(&litebox_sh_dir)
        .env("CFLAGS", "-Wall -Wextra -Werror -Os -static -s");
    let sh_output = sh_cmd
        .output()
        .expect("Failed to execute make for litebox-sh");
    assert!(
        sh_output.status.success(),
        "failed to build litebox-sh:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&sh_output.stdout),
        String::from_utf8_lossy(&sh_output.stderr),
    );
    let sh_binary = litebox_sh_dir.join("litebox-sh");
    assert!(sh_binary.exists(), "Build failed to create litebox-sh");
    // Copy to OUT_DIR for include_bytes!
    std::fs::copy(&sh_binary, out_dir.join("litebox-sh"))
        .expect("Failed to copy litebox-sh to OUT_DIR");

    println!("cargo:rerun-if-changed=tools/litebox-sh/litebox-sh.c");
    println!("cargo:rerun-if-changed=tools/litebox-sh/Makefile");
    println!("cargo:rerun-if-changed=build.rs");
}
