// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

use std::{fs, path::PathBuf};

use clap::Parser;

#[derive(Parser, Debug)]
#[command(about = "Rewrite ARM64 ELF files to hook syscalls")]
struct Args {
    /// Input ELF file
    input: PathBuf,
    /// Output file
    output: PathBuf,
    /// Trampoline address (optional, default 0 for runtime patching)
    #[arg(long)]
    trampoline: Option<String>,
    /// Allow rewriting files with no syscall instructions (copy unchanged)
    #[arg(long)]
    allow_no_syscalls: bool,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let input_data = fs::read(&args.input)?;
    let trampoline = args
        .trampoline
        .as_ref()
        .map(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).expect("Invalid hex address"));

    match litebox_syscall_rewriter_arm64::hook_syscalls_in_elf(&input_data, trampoline) {
        Ok(output_data) => {
            fs::write(&args.output, &output_data)?;
            println!(
                "Rewrote {} -> {} ({} bytes -> {} bytes)",
                args.input.display(),
                args.output.display(),
                input_data.len(),
                output_data.len()
            );
        }
        Err(litebox_syscall_rewriter_arm64::Error::NoSyscallInstructionsFound)
            if args.allow_no_syscalls =>
        {
            // No syscalls found but --allow-no-syscalls was set, just copy the file
            fs::write(&args.output, &input_data)?;
            println!(
                "Copied {} -> {} (no syscalls found, {} bytes)",
                args.input.display(),
                args.output.display(),
                input_data.len()
            );
        }
        Err(e) => return Err(e.into()),
    }

    Ok(())
}
