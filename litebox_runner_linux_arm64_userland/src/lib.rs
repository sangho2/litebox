// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! ARM64 Linux userland runner for LiteBox.
//!
//! This runner is specifically designed for ARM64 (aarch64) architecture and supports
//! both systrap (seccomp SIGSYS-based) and rewriter backends for syscall interception.

use anyhow::{anyhow, Result};
use clap::Parser;
use litebox::fs::{FileSystem as _, Mode};
use litebox_platform_multiplex::Platform;
use memmap2::Mmap;
use std::os::linux::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

extern crate alloc;

/// Run Linux programs with LiteBox on unmodified Linux (ARM64)
#[derive(Parser, Debug)]
pub struct CliArgs {
    /// The program and arguments passed to it (e.g., `python3 --version`)
    #[arg(required = true, trailing_var_arg = true, value_hint = clap::ValueHint::CommandWithArguments)]
    pub program_and_arguments: Vec<String>,
    /// Environment variables passed to the program (`K=V` pairs; can be invoked multiple times)
    #[arg(long = "env")]
    pub environment_variables: Vec<String>,
    /// Forward the existing environment variables
    #[arg(long = "forward-env")]
    pub forward_environment_variables: bool,
    /// Allow using unstable options
    #[arg(short = 'Z', long = "unstable")]
    pub unstable: bool,
    /// Pre-fill files into the initial file system state
    #[arg(long = "insert-file", value_hint = clap::ValueHint::FilePath,
          requires = "unstable", help_heading = "Unstable Options")]
    pub insert_files: Vec<PathBuf>,
    /// Pre-fill the files in this tar file into the initial file system state
    #[arg(long = "initial-files", value_name = "PATH_TO_TAR", value_hint = clap::ValueHint::FilePath,
          requires = "unstable", help_heading = "Unstable Options")]
    pub initial_files: Option<PathBuf>,
    /// Apply syscall-rewriter to the ELF file before running it
    ///
    /// This is meant as a convenience feature; real deployments would likely prefer ahead-of-time
    /// rewrite things to amortize costs.
    #[arg(
        long = "rewrite-syscalls",
        requires = "unstable",
        help_heading = "Unstable Options"
    )]
    pub rewrite_syscalls: bool,
    /// Choice of interception backend
    #[arg(
        value_enum,
        long = "interception-backend",
        requires = "unstable",
        help_heading = "Unstable Options",
        default_value = "seccomp"
    )]
    pub interception_backend: InterceptionBackend,
    /// Connect to a TUN device with this name
    #[arg(
        long = "tun-device-name",
        requires = "unstable",
        help_heading = "Unstable Options"
    )]
    pub tun_device_name: Option<String>,
}

/// Backends supported for intercepting syscalls
#[non_exhaustive]
#[derive(Debug, Clone, clap::ValueEnum)]
pub enum InterceptionBackend {
    /// Use seccomp-based syscall interception (systrap)
    Seccomp,
    /// Depend purely on rewritten syscalls to intercept them
    Rewriter,
}

fn mmapped_file_data(path: impl AsRef<Path>) -> Result<&'static [u8]> {
    let path = path.as_ref();
    let file = std::fs::File::open(path)?;
    // SAFETY: We assume that the file given to us is not going to change _externally_ while in
    // the middle of execution. Since we are mapping it as read-only and mapping it only once,
    // we are not planning to change it either. With both these in mind, this call is safe.
    //
    // We need to leak the `Mmap` object, so that it stays alive until the end of the program,
    // rather than being unmapped at function finish (i.e., to get the `'static` lifetime).
    Ok(Box::leak(Box::new(unsafe { Mmap::map(&file) }.map_err(
        |e| anyhow!("Could not read tar file at {}: {}", path.display(), e),
    )?)))
}

/// Run Linux programs with LiteBox on unmodified Linux (ARM64)
///
/// # Panics
///
/// Can panic if any particulars of the environment are not set up as expected.
pub fn run(cli_args: CliArgs) -> Result<()> {
    if !cli_args.insert_files.is_empty() {
        unimplemented!(
            "this should (hopefully soon) have a nicer interface to support loading in files"
        )
    }

    let (ancestor_modes_and_users, prog_data): (
        Vec<(litebox::fs::Mode, u32)>,
        alloc::borrow::Cow<'static, [u8]>,
    ) = {
        let prog = std::path::absolute(Path::new(&cli_args.program_and_arguments[0])).unwrap();
        let ancestors: Vec<_> = prog.ancestors().collect();
        let modes: Vec<_> = ancestors
            .into_iter()
            .rev()
            .skip(1)
            .map(|path| {
                let metadata = path.metadata().unwrap();
                (
                    litebox::fs::Mode::from_bits(metadata.st_mode()).unwrap(),
                    metadata.st_uid(),
                )
            })
            .collect();
        let data = mmapped_file_data(prog)?;
        let data = if cli_args.rewrite_syscalls {
            let rewritten = litebox_syscall_rewriter_arm64::hook_syscalls_in_elf(data, None)
                .map_err(|e| anyhow!("Failed to rewrite syscalls: {e}"))?;
            rewritten.into()
        } else {
            data.into()
        };
        (modes, data)
    };
    let tar_data: &'static [u8] = if let Some(tar_file) = cli_args.initial_files.as_ref() {
        if tar_file.extension().and_then(|x| x.to_str()) != Some("tar") {
            anyhow::bail!("Expected a .tar file, found {}", tar_file.display());
        }
        mmapped_file_data(tar_file)?
    } else {
        litebox::fs::tar_ro::EMPTY_TAR_FILE
    };

    let platform = Platform::new(cli_args.tun_device_name.as_deref());
    litebox_platform_multiplex::set_platform(platform);
    let mut shim_builder = litebox_shim_linux::LinuxShimBuilder::new();
    let litebox = shim_builder.litebox();
    let initial_file_system = {
        let mut in_mem = litebox::fs::in_mem::FileSystem::new(litebox);
        let prog = std::path::absolute(Path::new(&cli_args.program_and_arguments[0])).unwrap();
        let ancestors: Vec<_> = prog.ancestors().collect();
        let mut prev_user = 0;
        for (path, &mode_and_user) in ancestors
            .into_iter()
            .skip(1)
            .rev()
            .skip(1)
            .zip(&ancestor_modes_and_users)
        {
            if prev_user == 0 {
                in_mem.with_root_privileges(|fs| {
                    fs.mkdir(path.to_str().unwrap(), mode_and_user.0).unwrap();
                    if mode_and_user.1 != 0 {
                        fs.chown(path.to_str().unwrap(), Some(1000), Some(1000))
                            .unwrap();
                    }
                });
            } else {
                in_mem
                    .mkdir(path.to_str().unwrap(), mode_and_user.0)
                    .unwrap();
            }
            prev_user = mode_and_user.1;
        }

        let open_file =
            |fs: &mut litebox::fs::in_mem::FileSystem<litebox_platform_multiplex::Platform>,
             path,
             mode| {
                let fd = fs
                    .open(
                        path,
                        litebox::fs::OFlags::WRONLY | litebox::fs::OFlags::CREAT,
                        mode,
                    )
                    .unwrap();
                fs.initialize_primarily_read_heavy_file(&fd, prog_data.clone());
                fs.close(&fd).unwrap();
            };
        let last = ancestor_modes_and_users.last().unwrap();
        if prev_user == 0 {
            in_mem.with_root_privileges(|fs| {
                open_file(fs, prog.to_str().unwrap(), last.0);
                if last.1 != 0 {
                    fs.chown(prog.to_str().unwrap(), Some(1000), Some(1000))
                        .unwrap();
                }
            });
        } else {
            open_file(&mut in_mem, prog.to_str().unwrap(), last.0);
        }
        in_mem.with_root_privileges(|fs| {
            let mode = Mode::RWXU | Mode::RWXG | Mode::RWXO;
            if let Err(err) = fs.mkdir("/tmp", mode) {
                match err {
                    litebox::fs::errors::MkdirError::AlreadyExists => {
                        fs.chmod("/tmp", mode).expect("Failed to call chmod");
                    }
                    _ => panic!(),
                }
            }
        });

        let tar_ro = litebox::fs::tar_ro::FileSystem::new(litebox, tar_data.into());
        shim_builder.default_fs(in_mem, tar_ro)
    };

    let prog = std::path::absolute(Path::new(&cli_args.program_and_arguments[0])).unwrap();
    let prog_path = prog.to_str().ok_or_else(|| {
        anyhow!(
            "Could not convert program path {:?} to a string",
            cli_args.program_and_arguments[0]
        )
    })?;

    shim_builder.set_fs(initial_file_system);
    let shim = shim_builder.build();

    let shutdown = std::sync::Arc::new(core::sync::atomic::AtomicBool::new(false));
    let net_worker = if cli_args.tun_device_name.is_some() {
        let shim = shim.clone();
        let shutdown_clone = shutdown.clone();
        let child = std::thread::spawn(move || {
            const DEFAULT_TIMEOUT: core::time::Duration = core::time::Duration::from_millis(5);
            pin_thread_to_cpu(0);

            while !shutdown_clone.load(core::sync::atomic::Ordering::Relaxed) {
                let timeout = loop {
                    match shim.perform_network_interaction() {
                        litebox::net::PlatformInteractionReinvocationAdvice::CallAgainImmediately => {}
                        litebox::net::PlatformInteractionReinvocationAdvice::WaitOnDeviceOrSocketInteraction{ timeout } => {
                            break timeout;
                        }
                    }
                };
                litebox_platform_multiplex::platform()
                    .wait_on_tun(Some(timeout.unwrap_or(DEFAULT_TIMEOUT)));
            }
            while shim.perform_network_interaction().call_again_immediately() {}
        });
        Some(child)
    } else {
        None
    };

    let argv = cli_args
        .program_and_arguments
        .iter()
        .map(|x| std::ffi::CString::new(x.bytes().collect::<Vec<u8>>()).unwrap())
        .collect();
    let envp: Vec<_> = cli_args
        .environment_variables
        .iter()
        .map(|x| std::ffi::CString::new(x.bytes().collect::<Vec<u8>>()).unwrap())
        .collect();
    let envp = if cli_args.forward_environment_variables {
        envp.into_iter()
            .chain(std::env::vars().map(|(k, v)| {
                std::ffi::CString::new(
                    k.bytes()
                        .chain([b'='])
                        .chain(v.bytes())
                        .collect::<Vec<u8>>(),
                )
                .unwrap()
            }))
            .collect()
    } else {
        envp
    };

    // Get task params BEFORE enabling seccomp (requires gettid, getuid, etc.)
    let task_params = platform.init_task();

    let program = shim.load_program(task_params, prog_path, argv, envp)?;

    // Enable syscall interception based on backend choice
    match cli_args.interception_backend {
        InterceptionBackend::Seccomp => {
            // Enable AFTER all host syscalls are done
            platform.enable_seccomp_based_syscall_interception();
        }
        InterceptionBackend::Rewriter => {
            // Rewriter backend: syscalls are hooked via binary rewriting
            // No runtime interception needed - the rewritten binary jumps to our handler
        }
    }

    #[cfg(feature = "lock_tracing")]
    litebox::sync::start_recording();

    let _ = unsafe {
        litebox_platform_linux_userland::run_thread(
            program.entrypoints,
            &mut litebox_common_linux::PtRegs::default(),
        )
    };

    #[cfg(feature = "lock_tracing")]
    {
        litebox::sync::stop_recording();
        let events = litebox::sync::flush_to_jsonl();
        if !events.is_empty() {
            use std::io::Write;
            if let Ok(mut file) = std::fs::File::create("/tmp/locks.jsonl") {
                for line in &events {
                    let _ = writeln!(file, "{line}");
                }
            }
        }
    }

    if let Some(net_worker) = net_worker {
        shutdown.store(true, core::sync::atomic::Ordering::Relaxed);
        net_worker.join().unwrap();
    }
    std::process::exit(program.process.wait())
}

/// Pin the current thread to a specific CPU core
fn pin_thread_to_cpu(cpu: usize) {
    unsafe {
        let mut set = std::mem::zeroed();
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(cpu, &mut set);

        if libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &raw const set) != 0 {
            eprintln!("Warning: Failed to pin thread to CPU core {cpu}");
        }
    }
}
