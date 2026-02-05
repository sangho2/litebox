// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

fn objdump(binary: &[u8]) -> String {
    use std::io::Write;
    use std::process::Command;
    use tempfile::NamedTempFile;

    let mut temp_file = NamedTempFile::new().unwrap();
    temp_file.write_all(binary).unwrap();

    // Run objdump on the temporary file and capture the output
    let output = Command::new("objdump")
        .arg("-d")
        .arg(temp_file.path())
        .output()
        .unwrap();

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.contains("/tmp/"))
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

const HELLO_INPUT_ARM64: &[u8] = include_bytes!("hello-arm64");

fn run_snapshot_test(input: &[u8], snapshot: &str) {
    let output = litebox_syscall_rewriter_arm64::hook_syscalls_in_elf(input, None).unwrap();
    let diff = similar::udiff::unified_diff(
        similar::Algorithm::Myers,
        &objdump(input),
        &objdump(&output),
        3,
        Some(("original", "rewritten")),
    );

    insta::assert_snapshot!(snapshot, diff);
}

#[test]
fn snapshot_test_hello_world_arm64() {
    run_snapshot_test(HELLO_INPUT_ARM64, "hello-arm64-diff");
}

#[test]
fn test_hook_basic() {
    // Test that hooking works and produces valid output
    let output =
        litebox_syscall_rewriter_arm64::hook_syscalls_in_elf(HELLO_INPUT_ARM64, Some(0xDEADBEEF))
            .unwrap();

    // The output should be larger than input (has trampoline section appended)
    assert!(output.len() > HELLO_INPUT_ARM64.len());

    // Check that the output starts with ELF magic
    assert_eq!(&output[0..4], b"\x7fELF");
}

#[test]
fn test_hook_already_hooked() {
    // Hook once
    let output =
        litebox_syscall_rewriter_arm64::hook_syscalls_in_elf(HELLO_INPUT_ARM64, None).unwrap();

    // Try to hook again - should fail with AlreadyHooked
    let result = litebox_syscall_rewriter_arm64::hook_syscalls_in_elf(&output, None);
    assert!(matches!(
        result,
        Err(litebox_syscall_rewriter_arm64::Error::AlreadyHooked)
    ));
}

#[test]
fn test_unsupported_object() {
    // Empty data
    let result = litebox_syscall_rewriter_arm64::hook_syscalls_in_elf(&[], None);
    assert!(result.is_err());

    // Random data
    let result =
        litebox_syscall_rewriter_arm64::hook_syscalls_in_elf(&[0x7f, 0x45, 0x4c, 0x46], None);
    assert!(result.is_err());
}
