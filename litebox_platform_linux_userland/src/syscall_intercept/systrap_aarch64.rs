// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! The systrap platform for ARM64 relies on seccomp's `SECCOMP_RET_TRAP` feature to intercept system calls.

use core::ffi::{c_int, c_uint};

use litebox::utils::ReinterpretSignedExt as _;

/// Custom structure to reinterpret siginfo_t for syscall information
#[repr(C)]
struct SyscallSiginfo {
    signo: c_int,
    errno: c_int,
    code: c_int,
    #[cfg(target_pointer_width = "64")]
    __pad: c_int,
    call_addr: *mut libc::c_void,
    syscall: c_int,
    arch: c_uint,
}

/// Signal handler for SIGSYS on ARM64.
///
/// Note: only async-signal-safe functions should be used in this handler.
/// See full list at <https://www.man7.org/linux/man-pages/man7/signal-safety.7.html>
extern "C" fn sigsys_handler(sig: c_int, info: *mut libc::siginfo_t, context: *mut libc::c_void) {
    unsafe {
        assert!(sig == libc::SIGSYS);

        // Check if we're in guest mode by reading the in_guest TLS variable
        let in_guest: u8;
        core::arch::asm!(
            "mrs {tmp}, tpidr_el0",
            "ldrb {out:w}, [{tmp}, #:tprel_lo12:in_guest]",
            tmp = out(reg) _,
            out = out(reg) in_guest,
            options(nostack, preserves_flags, readonly)
        );

        // If not in guest mode, this is an unexpected syscall from the host.
        // For now, we panic. In the future, we might want to handle specific
        // syscalls that the platform needs to make.
        if in_guest == 0 {
            // Not in guest mode - this shouldn't happen with proper backdoor usage
            // Write error message using raw write syscall with backdoor
            let msg = b"FATAL: SIGSYS received outside guest mode\n";
            let _ = syscalls::raw::syscall4(
                syscalls::Sysno::write,
                2, // stderr
                msg.as_ptr() as usize,
                msg.len(),
                crate::syscall_intercept::SYSCALL_ARG_MAGIC,
            );
            std::process::abort();
        }

        let custom_info = &*info.cast::<SyscallSiginfo>();
        let addr = custom_info.call_addr;

        // Ensure the address is valid
        if addr.is_null() {
            std::process::abort();
        }

        // Get the ucontext and update the PC to point to syscall_callback
        let ucontext = &mut *(context.cast::<libc::ucontext_t>());

        // Store the return address in x30 (link register), as expected by `syscall_callback`.
        // The return address should be the instruction after the svc.
        ucontext.uc_mcontext.regs[30] = addr as u64;

        // Set the program counter to the syscall dispatcher
        ucontext.uc_mcontext.pc = crate::syscall_callback as *const () as u64;
    }
}

fn register_sigsys_handler() {
    let mut sig_mask = core::mem::MaybeUninit::<libc::sigset_t>::uninit();
    unsafe { libc::sigemptyset(sig_mask.as_mut_ptr()) };
    let sig_action = libc::sigaction {
        sa_sigaction: sigsys_handler as *const () as usize,
        sa_flags: litebox_common_linux::signal::SaFlags::SIGINFO
            .bits()
            .reinterpret_as_signed(),
        // SAFETY: Initialized by `libc::sigemptyset`
        sa_mask: unsafe { sig_mask.assume_init() },
        sa_restorer: None,
    };

    let ret = unsafe { libc::sigaction(libc::SIGSYS, &raw const sig_action, std::ptr::null_mut()) };
    assert_eq!(ret, 0, "Failed to register SIGSYS handler: {ret}");
}

#[cfg(not(test))]
fn register_seccomp_filter() {
    use seccompiler::{
        BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
        SeccompRule,
    };

    fn backdoor_on_arg(arg_number: u8) -> SeccompRule {
        SeccompRule::new(vec![backdoor_cond_on_arg(arg_number)]).unwrap()
    }

    fn backdoor_cond_on_arg(arg_number: u8) -> SeccompCondition {
        SeccompCondition::new(
            arg_number,
            SeccompCmpArgLen::Qword,
            SeccompCmpOp::Eq,
            super::SYSCALL_ARG_MAGIC as u64,
        )
        .unwrap()
    }

    // allow list - ARM64 uses different syscall numbers than x86_64
    let rules = vec![
        (libc::SYS_read, vec![backdoor_on_arg(3)]),
        (libc::SYS_write, vec![backdoor_on_arg(3)]),
        (
            libc::SYS_mmap,
            vec![
                // A backdoor to allow invoking mmap.
                SeccompRule::new(vec![
                    SeccompCondition::new(
                        3,
                        SeccompCmpArgLen::Dword,
                        SeccompCmpOp::MaskedEq(u64::from(super::MMAP_FLAG_MAGIC)),
                        u64::from(super::MMAP_FLAG_MAGIC),
                    )
                    .unwrap(),
                ])
                .unwrap(),
            ],
        ),
        (libc::SYS_mprotect, vec![backdoor_on_arg(3)]),
        (libc::SYS_munmap, vec![backdoor_on_arg(2)]),
        (
            libc::SYS_rt_sigaction,
            vec![
                // Allow rt_sigaction for non-SIGSYS signals
                SeccompRule::new(vec![
                    SeccompCondition::new(
                        0,
                        SeccompCmpArgLen::Dword,
                        SeccompCmpOp::Ne,
                        litebox_common_linux::signal::Signal::SIGSYS
                            .as_i32()
                            .try_into()
                            .unwrap(),
                    )
                    .unwrap(),
                ])
                .unwrap(),
                SeccompRule::new(vec![
                    // The second argument `act` is null, so it does not change the signal handler.
                    SeccompCondition::new(1, SeccompCmpArgLen::Qword, SeccompCmpOp::Eq, 0).unwrap(),
                ])
                .unwrap(),
            ],
        ),
        (
            // allow rt_sigprocmask that does not block SIGSYS
            libc::SYS_rt_sigprocmask,
            vec![
                // A backdoor to allow invoking rt_sigprocmask.
                backdoor_on_arg(4),
                SeccompRule::new(vec![
                    // The second argument `set` is null, so it does not change the block set.
                    SeccompCondition::new(1, SeccompCmpArgLen::Qword, SeccompCmpOp::Eq, 0).unwrap(),
                ])
                .unwrap(),
            ],
        ),
        (libc::SYS_rt_sigreturn, vec![]),
        (libc::SYS_sched_yield, vec![]),
        (libc::SYS_mremap, vec![backdoor_on_arg(5)]),
        (libc::SYS_sigaltstack, vec![]),
        (
            libc::SYS_futex,
            vec![
                SeccompRule::new(vec![
                    SeccompCondition::new(
                        1,
                        SeccompCmpArgLen::Dword,
                        SeccompCmpOp::MaskedEq(0x7f),
                        libc::FUTEX_WAIT as u64,
                    )
                    .unwrap(),
                    backdoor_cond_on_arg(5),
                ])
                .unwrap(),
                SeccompRule::new(vec![
                    SeccompCondition::new(
                        1,
                        SeccompCmpArgLen::Dword,
                        SeccompCmpOp::MaskedEq(0x7f),
                        libc::FUTEX_WAKE as u64,
                    )
                    .unwrap(),
                    backdoor_cond_on_arg(5),
                ])
                .unwrap(),
            ],
        ),
        (libc::SYS_exit, vec![backdoor_on_arg(1)]),
        (libc::SYS_exit_group, vec![backdoor_on_arg(1)]),
        (libc::SYS_tgkill, vec![]),
        (libc::SYS_setitimer, vec![]),
        (libc::SYS_clone3, vec![backdoor_on_arg(2)]),
    ];
    let rule_map: std::collections::BTreeMap<i64, Vec<SeccompRule>> = rules.into_iter().collect();

    let filter = SeccompFilter::new(
        rule_map,
        SeccompAction::Trap,
        SeccompAction::Allow,
        seccompiler::TargetArch::aarch64,
    )
    .unwrap();
    // TODO: bpf program can be compiled offline
    let bpf_prog: BpfProgram = filter.try_into().unwrap();

    // After this call, ALL syscalls (except those with backdoor magic) will trigger SIGSYS.
    // TODO: The current implementation has issues because libc/std makes syscalls after
    // apply_filter returns. We need to use raw syscalls with backdoor magic for all
    // operations after this point.
    seccompiler::apply_filter(&bpf_prog).unwrap();
}

/// Initialize the syscall interception mechanism for ARM64.
///
/// This function sets up the syscall handler and registers seccomp
/// filters and the SIGSYS signal handler.
pub(crate) fn init_sys_intercept() {
    register_sigsys_handler();

    // Cargo unit test does not forward signals to tests.
    // Use integration tests to test it.
    #[cfg(not(test))]
    register_seccomp_filter();
}
