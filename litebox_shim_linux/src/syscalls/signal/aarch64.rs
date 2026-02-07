// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

use crate::syscalls::signal::{DeliverFault, SignalState};
use crate::MutPtr;
use core::mem::offset_of;
use litebox::platform::{RawConstPointer as _, RawMutPointer as _};
use litebox::utils::TruncateExt as _;
use litebox_common_linux::{
    signal::{aarch64::Sigcontext, SaFlags, SigAction, Siginfo, Ucontext},
    PtRegs,
};
use zerocopy::{FromBytes, IntoBytes};

#[repr(C)]
#[derive(Clone, FromBytes, IntoBytes)]
struct SignalFrame {
    ucontext: Ucontext,
    siginfo: Siginfo,
}

pub(super) fn uctx_addr(ctx: &PtRegs) -> usize {
    ctx.sp
}

pub(super) fn sp(ctx: &PtRegs) -> usize {
    ctx.sp
}

pub(super) fn get_signal_frame(sp: usize, _action: &SigAction) -> usize {
    let mut frame_addr = sp;

    // Space for the signal frame
    frame_addr -= core::mem::size_of::<SignalFrame>();

    // ARM64 requires 16-byte stack alignment
    frame_addr &= !15;

    frame_addr
}

impl SignalState {
    pub(super) fn write_signal_frame(
        &self,
        frame_addr: usize,
        siginfo: &Siginfo,
        action: &SigAction,
        ctx: &mut PtRegs,
        sigreturn_trampoline: Option<usize>,
    ) -> Result<(), DeliverFault> {
        // Determine the restorer address. On ARM64, glibc does not set
        // SA_RESTORER, so we fall back to the platform's sigreturn trampoline.
        let restorer = if action.flags.contains(SaFlags::RESTORER) {
            action.restorer
        } else if let Some(trampoline_addr) = sigreturn_trampoline {
            trampoline_addr
        } else {
            return Err(DeliverFault);
        };

        // Build the sigcontext from the current register state
        let mut regs = [0u64; 31];
        for (i, r) in ctx.regs.iter().enumerate() {
            regs[i] = *r as u64;
        }

        // Set fault_address from last exception info
        let last_exception = self.last_exception.get();

        let frame = SignalFrame {
            ucontext: Ucontext {
                flags: 0,
                link: 0,
                stack: self.altstack.get(),
                sigmask: self.blocked.get(),
                _sigmask_reserved: [0; 120],
                _mcontext_align_pad: [0; 8],
                mcontext: Sigcontext {
                    fault_address: last_exception.far as u64,
                    regs,
                    sp: ctx.sp as u64,
                    pc: ctx.pc as u64,
                    pstate: ctx.pstate as u64,
                    _align_pad: [0; 8],
                    __reserved: [0; 4096],
                },
            },
            siginfo: siginfo.clone(),
        };

        let frame_ptr = MutPtr::from_usize(frame_addr);
        frame_ptr.write_at_offset(0, frame).ok_or(DeliverFault)?;

        // Set up the register state to call the signal handler
        ctx.sp = frame_addr;
        ctx.pc = action.sigaction;
        // x0 = signal number
        #[allow(clippy::cast_sign_loss)] // Signal number is always positive
        {
            ctx.regs[0] = siginfo.signo as u32 as usize;
        }
        // x1 = pointer to siginfo
        ctx.regs[1] = frame_addr.wrapping_add(offset_of!(SignalFrame, siginfo));
        // x2 = pointer to ucontext
        ctx.regs[2] = frame_addr.wrapping_add(offset_of!(SignalFrame, ucontext));
        // x30 (link register) = restorer
        ctx.regs[30] = restorer;

        Ok(())
    }
}

pub(super) fn restore_sigcontext(
    ctx: &mut PtRegs,
    sigctx: &litebox_common_linux::signal::aarch64::Sigcontext,
) -> usize {
    for i in 0..31 {
        ctx.regs[i] = sigctx.regs[i].truncate();
    }
    ctx.sp = sigctx.sp.truncate();
    ctx.pc = sigctx.pc.truncate();
    ctx.pstate = sigctx.pstate.truncate();

    // Return value is in x0
    ctx.regs[0]
}
