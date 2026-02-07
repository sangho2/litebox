// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Definitions for aarch64 signal context structures.

use zerocopy::{FromBytes, IntoBytes};

/// ARM64 signal context structure.
/// See: https://elixir.bootlin.com/linux/v5.19.17/source/arch/arm64/include/uapi/asm/sigcontext.h
///
/// The kernel's `__reserved` field has `__attribute__((aligned(16)))`, which
/// inserts 8 bytes of padding after `pstate` (offset 272, ends at 280) so
/// that `__reserved` starts at offset 288 (16-byte aligned). Total: 4384 bytes.
#[repr(C, align(16))]
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
    /// Padding to align `__reserved` to 16 bytes (matching kernel's
    /// `__attribute__((aligned(16)))` on `__reserved`).
    #[doc(hidden)]
    pub _align_pad: [u8; 8],
    // Space for extension records (FPSIMD, SVE, etc.)
    // The kernel writes variable-length data here; we reserve space.
    #[doc(hidden)]
    pub __reserved: [u8; 4096],
}

const _: () = assert!(core::mem::size_of::<Sigcontext>() == 4384);

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
