// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Definitions for aarch64 signal context structures.

use zerocopy::{FromBytes, IntoBytes};

/// ARM64 signal context structure.
/// See: https://elixir.bootlin.com/linux/v5.19.17/source/arch/arm64/include/uapi/asm/sigcontext.h
#[repr(C)]
#[derive(Clone, FromBytes, IntoBytes)]
pub struct Sigcontext {
    pub fault_address: u64,
    /// General purpose registers x0-x30
    pub regs: [u64; 31],
    /// Stack pointer
    pub sp: u64,
    /// Program counter
    pub pc: u64,
    /// Processor state (PSTATE)
    pub pstate: u64,
    // Space for extension records (FPSIMD, SVE, etc.)
    // The kernel writes variable-length data here; we reserve space.
    #[doc(hidden)]
    pub __reserved: [u8; 4096],
}

/// ARM64 FPSIMD context (floating point / SIMD state).
#[repr(C)]
#[derive(Clone)]
pub struct FpsimdContext {
    pub head: AuxHead,
    pub fpsr: u32,
    pub fpcr: u32,
    pub vregs: [u128; 32],
}

/// Header for auxiliary signal context records.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AuxHead {
    pub magic: u32,
    pub size: u32,
}

pub const FPSIMD_MAGIC: u32 = 0x46508001;
