// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Rewrite ARM64 ELF files to hook syscalls
//!
//! This crate sets up a trampoline point for every `svc #0` instruction in its input binary,
//! allowing for conveniently taking control of a binary without ptrace/systrap/seccomp.
//!
//! This approach is not 100% foolproof, and should not be considered a security boundary. Instead,
//! it is a slowly-improving best-effort technique. As an explicit non-goal, this technique will
//! **NOT** support dynamically generated `svc` instructions (for example, generated in a JIT).
//! However, as an explicit goal, it is intended to provide low-overhead hooking of syscalls,
//! without needing to undergo a user-kernel transition.
//!
//! This crate only supports AArch64 (ARM64) ELFs.

// Low-level instruction encoding requires many casts that are safe in context
#![allow(
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_lossless
)]

use std::collections::HashSet;

use thiserror::Error;
use yaxpeax_arch::{Decoder, U8Reader};
use yaxpeax_arm::armv8::a64::{InstDecoder, Instruction, Opcode};

/// Possible errors during hooking of `syscall` instructions
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Error {
    #[error("failed to parse: {0}")]
    ParseError(String),
    #[error("failed to generate object file: {0}")]
    GenerateObjFileError(String),
    #[error("unsupported executable (ARM64 only)")]
    UnsupportedObjectFile,
    #[error("executable is already hooked with trampoline")]
    AlreadyHooked,
    #[error("no .text section found")]
    NoTextSectionFound,
    #[error("no syscall instructions found")]
    NoSyscallInstructionsFound,
    #[error("failed to disassemble: {0}")]
    DisassemblyFailure(String),
    #[error("insufficient bytes before or after svc at {0:#x}")]
    InsufficientBytesBeforeOrAfter(u64),
}

type Result<T> = std::result::Result<T, Error>;

/// The prefix for any trampolines inserted by any version of this crate.
pub const TRAMPOLINE_SECTION_NAME_PREFIX: &str = ".trampolineLB";

/// The name of the section for the trampoline.
pub const TRAMPOLINE_SECTION_NAME: &str = ".trampolineLB0";

/// ARM64 instruction size (fixed 4 bytes)
const ARM64_INSN_SIZE: usize = 4;

/// A decoded ARM64 instruction with its address
#[derive(Clone, Debug)]
struct DecodedInsn {
    addr: u64,
    insn: Instruction,
    bytes: [u8; 4],
}

impl DecodedInsn {
    fn next_ip(&self) -> u64 {
        self.addr + ARM64_INSN_SIZE as u64
    }

    fn is_svc(&self) -> bool {
        self.insn.opcode == Opcode::SVC
    }

    /// Check if this instruction is `MSR TPIDR_EL0, Xn`
    /// Encoding: 0xD51BD040 | Rt, so mask top 27 bits
    fn is_msr_tpidr_el0(&self) -> bool {
        let opcode = u32::from_le_bytes(self.bytes);
        (opcode & 0xFFFF_FFE0) == 0xD51B_D040
    }

    /// Extract the source register Rt from `MSR TPIDR_EL0, Xt`
    /// Only valid when `is_msr_tpidr_el0()` returns true.
    fn msr_tpidr_el0_source_reg(&self) -> u8 {
        let opcode = u32::from_le_bytes(self.bytes);
        (opcode & 0x1F) as u8
    }

    fn is_branch(&self) -> bool {
        matches!(
            self.insn.opcode,
            Opcode::B
                | Opcode::BL
                | Opcode::BR
                | Opcode::BLR
                | Opcode::RET
                | Opcode::CBZ
                | Opcode::CBNZ
                | Opcode::TBZ
                | Opcode::TBNZ
                | Opcode::Bcc(_)
        )
    }

    fn branch_target(&self) -> Option<u64> {
        // For direct branches (B, BL, CBZ, CBNZ, TBZ, TBNZ, Bcc), extract target
        // The target is encoded as a PC-relative offset
        match self.insn.opcode {
            Opcode::B | Opcode::BL => {
                // Immediate offset is in operand 0
                if let Some(yaxpeax_arm::armv8::a64::Operand::PCOffset(offset)) =
                    self.insn.operands.first()
                {
                    Some(self.addr.wrapping_add_signed(*offset))
                } else {
                    None
                }
            }
            Opcode::Bcc(_) | Opcode::CBZ | Opcode::CBNZ | Opcode::TBZ | Opcode::TBNZ => {
                // Look for PCOffset operand
                for op in &self.insn.operands {
                    if let yaxpeax_arm::armv8::a64::Operand::PCOffset(offset) = op {
                        return Some(self.addr.wrapping_add_signed(*offset));
                    }
                }
                None
            }
            _ => None,
        }
    }
}

/// ARM64 instruction encoder helpers
#[allow(dead_code)]
#[allow(
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::manual_range_contains,
    clippy::cast_lossless
)]
mod encoder {
    /// Encode B (unconditional branch) instruction
    /// B imm26 -> PC + sign_extend(imm26 << 2)
    pub fn encode_b(from: u64, to: u64) -> Option<[u8; 4]> {
        let offset = to.wrapping_sub(from) as i64;
        // Branch offset must be within +/- 128MB (26-bit signed * 4)
        if offset < -(1 << 27) || offset >= (1 << 27) {
            return None;
        }
        // offset must be 4-byte aligned
        if offset & 3 != 0 {
            return None;
        }
        let imm26 = ((offset >> 2) as u32) & 0x03FF_FFFF;
        let insn = 0x14_000000 | imm26;
        Some(insn.to_le_bytes())
    }

    /// Encode BL (branch with link) instruction
    /// BL imm26 -> X30 = PC + 4; PC = PC + sign_extend(imm26 << 2)
    pub fn encode_bl(from: u64, to: u64) -> Option<[u8; 4]> {
        let offset = to.wrapping_sub(from) as i64;
        if offset < -(1 << 27) || offset >= (1 << 27) {
            return None;
        }
        if offset & 3 != 0 {
            return None;
        }
        let imm26 = ((offset >> 2) as u32) & 0x03FF_FFFF;
        let insn = 0x94_000000 | imm26;
        Some(insn.to_le_bytes())
    }

    /// Encode BR (branch to register) instruction
    /// BR Xn -> PC = Xn
    pub fn encode_br(reg: u8) -> [u8; 4] {
        assert!(reg < 32);
        // BR: 1101011 0000 11111 0000 00 Rn 00000
        let insn = 0xD61F_0000 | ((reg as u32) << 5);
        insn.to_le_bytes()
    }

    /// Encode RET instruction
    /// RET {Xn} -> PC = Xn (default X30)
    /// Note: RET has a different encoding from BR (includes return hint)
    pub fn encode_ret() -> [u8; 4] {
        // RET X30: D65F03C0
        // RET: 1101011 0010 11111 0000 00 Rn 00000
        let insn = 0xD65F_0000 | (30u32 << 5);
        insn.to_le_bytes()
    }
    /// Encode LDR Xt, [PC, #imm19] (literal)
    /// Load 64-bit value from PC-relative address
    pub fn encode_ldr_literal(rt: u8, offset: i32) -> Option<[u8; 4]> {
        assert!(rt < 32);
        // Offset must be 4-byte aligned and within +/- 1MB
        if offset & 3 != 0 {
            return None;
        }
        let imm19 = offset >> 2;
        if imm19 < -(1 << 18) || imm19 >= (1 << 18) {
            return None;
        }
        // LDR (literal): opc=01 011 imm19 Rt (64-bit)
        let insn = 0x5800_0000 | (((imm19 as u32) & 0x7FFFF) << 5) | (rt as u32);
        Some(insn.to_le_bytes())
    }

    /// Encode ADR Xd, label (PC-relative address)
    /// ADR Xd, #imm21 -> Xd = PC + imm21
    pub fn encode_adr(rd: u8, offset: i32) -> Option<[u8; 4]> {
        assert!(rd < 32);
        // imm21 range: +/- 1MB
        if offset < -(1 << 20) || offset >= (1 << 20) {
            return None;
        }
        let immlo = (offset & 0x3) as u32;
        let immhi = ((offset >> 2) & 0x7FFFF) as u32;
        // ADR: 0 immlo 10000 immhi Rd
        let insn = (immlo << 29) | 0x1000_0000 | (immhi << 5) | (rd as u32);
        Some(insn.to_le_bytes())
    }

    /// Encode ADRP Xd, label (PC-relative page address)
    /// ADRP Xd, #imm21 -> Xd = (PC & ~0xFFF) + (imm21 << 12)
    /// Range: ±4GB (page-aligned)
    pub fn encode_adrp(rd: u8, page_offset: i64) -> Option<[u8; 4]> {
        assert!(rd < 32);
        // page_offset must be page-aligned (multiple of 4096)
        // imm21 = page_offset >> 12, range: +/- 2^20 pages = +/- 4GB
        let imm21 = page_offset >> 12;
        if imm21 < -(1i64 << 20) || imm21 >= (1i64 << 20) {
            return None;
        }
        let imm21 = imm21 as u32;
        let immlo = imm21 & 0x3;
        let immhi = (imm21 >> 2) & 0x7FFFF;
        // ADRP: 1 immlo 10000 immhi Rd
        let insn = (1u32 << 31) | (immlo << 29) | 0x1000_0000 | (immhi << 5) | (rd as u32);
        Some(insn.to_le_bytes())
    }

    /// Encode NOP instruction
    pub fn encode_nop() -> [u8; 4] {
        0xD503_201F_u32.to_le_bytes()
    }

    /// Encode MOV Xd, #imm16 (MOVZ)
    pub fn encode_movz(rd: u8, imm16: u16, shift: u8) -> [u8; 4] {
        assert!(rd < 32);
        assert!(shift == 0 || shift == 16 || shift == 32 || shift == 48);
        let hw = (shift / 16) as u32;
        // MOVZ: 1 10 100101 hw imm16 Rd (64-bit)
        let insn = 0xD280_0000 | (hw << 21) | ((imm16 as u32) << 5) | (rd as u32);
        insn.to_le_bytes()
    }

    /// Encode MOVK Xd, #imm16, LSL #shift
    pub fn encode_movk(rd: u8, imm16: u16, shift: u8) -> [u8; 4] {
        assert!(rd < 32);
        assert!(shift == 0 || shift == 16 || shift == 32 || shift == 48);
        let hw = (shift / 16) as u32;
        // MOVK: 1 11 100101 hw imm16 Rd (64-bit)
        let insn = 0xF280_0000 | (hw << 21) | ((imm16 as u32) << 5) | (rd as u32);
        insn.to_le_bytes()
    }

    /// Encode a 64-bit immediate load into register using MOVZ + MOVK sequence
    /// Returns 1-4 instructions depending on the value
    pub fn encode_mov_imm64(rd: u8, value: u64) -> Vec<[u8; 4]> {
        let mut insns = Vec::new();

        // Find first non-zero 16-bit chunk for MOVZ
        let chunks = [
            (value & 0xFFFF) as u16,
            ((value >> 16) & 0xFFFF) as u16,
            ((value >> 32) & 0xFFFF) as u16,
            ((value >> 48) & 0xFFFF) as u16,
        ];

        let mut first = true;
        for (i, &chunk) in chunks.iter().enumerate() {
            if chunk != 0 || (first && i == 3) {
                // Always emit at least one instruction
                let shift = (i * 16) as u8;
                if first {
                    insns.push(encode_movz(rd, chunk, shift));
                    first = false;
                } else {
                    insns.push(encode_movk(rd, chunk, shift));
                }
            }
        }

        // Handle zero case
        if insns.is_empty() {
            insns.push(encode_movz(rd, 0, 0));
        }

        insns
    }

    /// Encode STP (store pair) for saving registers to stack
    /// STP Xt1, Xt2, [SP, #imm7*8]!  (pre-index)
    pub fn encode_stp_pre(rt1: u8, rt2: u8, imm7: i8) -> [u8; 4] {
        assert!(rt1 < 32 && rt2 < 32);
        assert!(imm7 >= -64 && imm7 < 64);
        // STP (pre-index): 10 101 0011 1 imm7 Rt2 Rn Rt1
        // Rn = SP (31)
        let simm7 = (imm7 as u32) & 0x7F;
        let insn = 0xA9BF_0000 | (simm7 << 15) | ((rt2 as u32) << 10) | (31 << 5) | (rt1 as u32);
        insn.to_le_bytes()
    }

    /// Encode LDP (load pair) for restoring registers from stack
    /// LDP Xt1, Xt2, [SP], #imm7*8  (post-index)
    pub fn encode_ldp_post(rt1: u8, rt2: u8, imm7: i8) -> [u8; 4] {
        assert!(rt1 < 32 && rt2 < 32);
        assert!(imm7 >= -64 && imm7 < 64);
        // LDP (post-index): 10 101 0001 1 imm7 Rt2 Rn Rt1
        let simm7 = (imm7 as u32) & 0x7F;
        let insn = 0xA8C0_0000 | (simm7 << 15) | ((rt2 as u32) << 10) | (31 << 5) | (rt1 as u32);
        insn.to_le_bytes()
    }

    /// Encode MOV (register) - ORR Xd, XZR, Xm
    /// MOV Xd, Xm is an alias for ORR Xd, XZR, Xm
    pub fn encode_mov_reg(rd: u8, rm: u8) -> [u8; 4] {
        assert!(rd < 32 && rm < 32);
        // ORR (shifted register): 10101010 000 Rm 000000 11111 Rd
        // sf=1 (64-bit), opc=01, shift=00, N=0, Rm, imm6=0, Rn=XZR(31), Rd
        let insn = 0xAA00_03E0 | ((rm as u32) << 16) | (rd as u32);
        insn.to_le_bytes()
    }

    /// Encode STR (store register, unsigned offset)
    /// STR Xt, [Xn, #pimm]  where pimm is scaled by 8
    pub fn encode_str_imm(rt: u8, rn: u8, pimm: u16) -> Option<[u8; 4]> {
        assert!(rt < 32 && rn < 32);
        // pimm must be a multiple of 8 and fit in 12 bits (0-32760)
        if !pimm.is_multiple_of(8) || pimm > 32760 {
            return None;
        }
        let imm12 = (pimm / 8) as u32;
        // STR (unsigned offset): 11 111 00100 imm12 Rn Rt
        let insn = 0xF900_0000 | (imm12 << 10) | ((rn as u32) << 5) | (rt as u32);
        Some(insn.to_le_bytes())
    }

    /// Encode LDR (load register, unsigned offset)
    /// LDR Xt, [Xn, #pimm]  where pimm is scaled by 8
    pub fn encode_ldr_imm(rt: u8, rn: u8, pimm: u16) -> Option<[u8; 4]> {
        assert!(rt < 32 && rn < 32);
        // pimm must be a multiple of 8 and fit in 12 bits (0-32760)
        if !pimm.is_multiple_of(8) || pimm > 32760 {
            return None;
        }
        let imm12 = (pimm / 8) as u32;
        // LDR (unsigned offset): 11 111 00101 imm12 Rn Rt
        let insn = 0xF940_0000 | (imm12 << 10) | ((rn as u32) << 5) | (rt as u32);
        Some(insn.to_le_bytes())
    }

    /// Encode SUB (immediate)
    /// SUB Xd, Xn, #imm12
    pub fn encode_sub_imm(rd: u8, rn: u8, imm12: u16) -> Option<[u8; 4]> {
        assert!(rd < 32 && rn < 32);
        if imm12 > 4095 {
            return None;
        }
        // SUB (immediate): sf=1 op=1 S=0 10001 shift=00 imm12 Rn Rd
        // 1 1 0 100010 0 imm12 Rn Rd = 0xD1000000
        let insn = 0xD100_0000 | ((imm12 as u32) << 10) | ((rn as u32) << 5) | (rd as u32);
        Some(insn.to_le_bytes())
    }

    /// Encode ADD (immediate)
    /// ADD Xd, Xn, #imm12
    pub fn encode_add_imm(rd: u8, rn: u8, imm12: u16) -> Option<[u8; 4]> {
        assert!(rd < 32 && rn < 32);
        if imm12 > 4095 {
            return None;
        }
        // ADD (immediate): sf=1 op=0 S=0 10001 shift=00 imm12 Rn Rd
        // 1 0 0 100010 0 imm12 Rn Rd = 0x91000000
        let insn = 0x9100_0000 | ((imm12 as u32) << 10) | ((rn as u32) << 5) | (rd as u32);
        Some(insn.to_le_bytes())
    }

    /// Encode MRS Xt, TPIDR_EL0
    /// Reads the thread pointer register into Xt.
    pub fn encode_mrs_tpidr_el0(rt: u8) -> [u8; 4] {
        assert!(rt < 32);
        // MRS Xt, TPIDR_EL0: 1101010100 11 1 1101 1110 0010 000 Rt
        // System register TPIDR_EL0 = S3_3_C13_C0_2 = op0=3,op1=3,CRn=13,CRm=0,op2=2
        // Encoding: 0xD53BD040 | Rt
        let insn = 0xD53B_D040 | (rt as u32);
        insn.to_le_bytes()
    }

    /// Encode MSR TPIDR_EL0, Xt
    /// Writes Xt to the thread pointer register.
    pub fn encode_msr_tpidr_el0(rt: u8) -> [u8; 4] {
        assert!(rt < 32);
        // MSR TPIDR_EL0, Xt: 1101010100 01 1 1101 1110 0010 000 Rt
        // Encoding: 0xD51BD040 | Rt
        let insn = 0xD51B_D040 | (rt as u32);
        insn.to_le_bytes()
    }

    /// Encode CMP Xn, Xm (alias for SUBS XZR, Xn, Xm)
    pub fn encode_cmp_reg(rn: u8, rm: u8) -> [u8; 4] {
        assert!(rn < 32 && rm < 32);
        // SUBS XZR, Xn, Xm: 1 1 1 01011 shift=00 0 Rm imm6=000000 Rn Rd=11111
        // sf=1, op=1, S=1 => 0xEB00001F
        let insn = 0xEB00_001F | ((rm as u32) << 16) | ((rn as u32) << 5);
        insn.to_le_bytes()
    }

    /// Encode B.cond (conditional branch)
    /// offset is in bytes, must be 4-byte aligned, range ±1MB
    pub fn encode_b_cond(cond: u8, offset: i32) -> Option<[u8; 4]> {
        assert!(cond < 16);
        if offset & 3 != 0 {
            return None;
        }
        let imm19 = offset >> 2;
        if imm19 < -(1 << 18) || imm19 >= (1 << 18) {
            return None;
        }
        // B.cond: 0101010 0 imm19 0 cond
        let insn = 0x5400_0000 | (((imm19 as u32) & 0x7FFFF) << 5) | (cond as u32);
        Some(insn.to_le_bytes())
    }

    /// Encode CMN (immediate) - Compare Negative: ADDS XZR, Xn, #imm12
    /// Sets condition flags based on Xn + imm12.
    /// CMN Xn, #1 sets ZF when Xn == 0xFFFFFFFFFFFFFFFF (-1).
    pub fn encode_cmn_imm(rn: u8, imm12: u16) -> Option<[u8; 4]> {
        assert!(rn < 32);
        if imm12 > 4095 {
            return None;
        }
        // ADDS (immediate): sf=1 op=0 S=1 100010 shift=0 imm12 Rn Rd=11111(XZR)
        // 1 0 1 100010 0 imm12 Rn 11111 = 0xB100001F
        let insn = 0xB100_001F | ((imm12 as u32) << 10) | ((rn as u32) << 5);
        Some(insn.to_le_bytes())
    }

    /// ARM64 condition code for EQ (equal, Z=1)
    pub const COND_EQ: u8 = 0;

    /// Encode LDR Xt, [Xn, #simm9]! (pre-index, signed offset)
    /// Used for post-increment pattern: LDR + ADD is more typical on ARM64.
    /// Range: -256 to 255 bytes.
    pub fn encode_ldr_pre(rt: u8, rn: u8, simm9: i16) -> Option<[u8; 4]> {
        assert!(rt < 32 && rn < 32);
        if simm9 < -256 || simm9 > 255 {
            return None;
        }
        // LDR (pre-index): 11 111 00010 0 simm9 11 Rn Rt
        let imm9 = (simm9 as u32) & 0x1FF;
        let insn = 0xF840_0C00 | (imm9 << 12) | ((rn as u32) << 5) | (rt as u32);
        Some(insn.to_le_bytes())
    }
}

/// Decode all instructions in a section
fn decode_section(section_base_addr: u64, section_data: &[u8]) -> Vec<DecodedInsn> {
    let decoder = InstDecoder::default();
    let mut instructions = Vec::new();
    let mut offset = 0usize;

    while offset + ARM64_INSN_SIZE <= section_data.len() {
        let bytes: [u8; 4] = section_data[offset..offset + 4].try_into().unwrap();
        let mut reader = U8Reader::new(&bytes);

        match decoder.decode(&mut reader) {
            Ok(insn) => {
                instructions.push(DecodedInsn {
                    addr: section_base_addr + offset as u64,
                    insn,
                    bytes,
                });
            }
            Err(_) => {
                // For invalid instructions, create a placeholder
                // This happens with data embedded in code sections
                instructions.push(DecodedInsn {
                    addr: section_base_addr + offset as u64,
                    insn: Instruction::default(),
                    bytes,
                });
            }
        }
        offset += ARM64_INSN_SIZE;
    }

    instructions
}

/// Update the `input_binary` with a call to `trampoline` instead of any `svc #0` instructions.
///
/// The `trampoline` must be an absolute address if specified; if unspecified, it will be set to
/// zeros, and it is the caller's decision to overwrite it at loading time.
///
/// If it succeeds, it produces an executable with a [`TRAMPOLINE_SECTION_NAME`] section whose first
/// 8 bytes point to the `trampoline` address.
#[expect(
    clippy::missing_panics_doc,
    reason = "any panics in here are not part of the public contract and should be fixed within this module"
)]
pub fn hook_syscalls_in_elf(input_binary: &[u8], trampoline: Option<u64>) -> Result<Vec<u8>> {
    if input_binary.is_empty() {
        return Err(Error::ParseError("empty input".to_string()));
    }

    let mut input_workaround: Vec<u64>;
    let input_binary: &[u8] = if (&raw const input_binary[0] as usize).is_multiple_of(8) {
        input_binary
    } else {
        // Workaround for object crate requiring 8-byte alignment
        input_workaround = vec![0u64; input_binary.len() / 8 + 1];
        let input_workaround_bytes: &mut [u8] = unsafe {
            core::slice::from_raw_parts_mut(
                input_workaround.as_mut_ptr().cast(),
                input_workaround.len() * 8,
            )
        };
        let input_workaround_bytes = &mut input_workaround_bytes[..input_binary.len()];
        input_workaround_bytes.copy_from_slice(input_binary);
        &*input_workaround_bytes
    };
    assert_eq!((&raw const input_binary[0] as usize) % 8, 0);

    let file_kind =
        object::FileKind::parse(input_binary).map_err(|e| Error::ParseError(e.to_string()))?;

    // Only support 64-bit ARM ELFs
    if file_kind != object::FileKind::Elf64 {
        return Err(Error::UnsupportedObjectFile);
    }

    let mut builder = object::build::elf::Builder::read64(input_binary)
        .map_err(|e| Error::ParseError(e.to_string()))?;

    // Check architecture is ARM64
    if builder.header.e_machine != object::elf::EM_AARCH64 {
        return Err(Error::UnsupportedObjectFile);
    }

    let text_sections = text_sections(&builder)?;
    let trampoline_section = setup_trampoline_section(&mut builder)?;

    // Get control transfer targets
    let control_transfer_targets = get_control_transfer_targets(&builder, &text_sections);

    let trampoline_base_addr = find_addr_for_trampoline_code(&builder);
    let mut trampoline_data = vec![];

    // Trampoline section layout:
    // Offset 0-7:   "LITEBOX0" magic/version
    // Offset 8-15:  Handler address (written by loader)
    // Offset 16-23: Pointer to per-thread host TLS lookup table (written at runtime)
    // Offset 24+:   Per-SVC trampoline entries
    //
    // The trampoline code looks up host TLS by scanning the table for an entry
    // matching the current thread's TPIDR_EL0 (guest TLS). Each table entry is
    // 16 bytes: [guest_tpidr (8 bytes), host_tls (8 bytes)].
    // This eliminates the race condition that existed when all threads shared
    // a single host TLS slot.

    // The magic prefix for the trampoline section
    trampoline_data.extend_from_slice("LITEBOX0".as_bytes());

    // The placeholder for the address of the new syscall entry point (offset 8)
    let trampoline = trampoline.unwrap_or(0);
    trampoline_data.extend_from_slice(&trampoline.to_le_bytes());

    // Placeholder for host TLS table pointer (offset 16)
    // Written at runtime by the loader/runner; points to a per-thread lookup table.
    // The trampoline scans this table to find the host TLS for the current thread.
    trampoline_data.extend_from_slice(&0u64.to_le_bytes());

    // Sigreturn trampoline at offset 24.
    // On ARM64, glibc does not set SA_RESTORER, so the shim needs a sigreturn
    // trampoline address to use as the restorer when delivering signals to the guest.
    // This trampoline sets x8 = __NR_rt_sigreturn (139) and then calls into the
    // syscall handler via the same mechanism as the SVC trampolines.
    assert_eq!(trampoline_data.len(), 24);
    generate_sigreturn_trampoline(trampoline_base_addr, &mut trampoline_data);

    let mut syscall_insns_found = false;
    for s in &text_sections {
        let s = builder.sections.get_mut(*s);
        let object::build::elf::SectionData::Data(data) = &mut s.data else {
            unimplemented!()
        };
        match hook_syscalls_in_section(
            &control_transfer_targets,
            s.sh_addr,
            data.to_mut(),
            trampoline_base_addr,
            &mut trampoline_data,
        ) {
            Ok(()) => {
                syscall_insns_found = true;
            }
            Err(Error::NoSyscallInstructionsFound) => {}
            Err(e) => return Err(e),
        }
    }

    if !syscall_insns_found {
        return Err(Error::NoSyscallInstructionsFound);
    }

    // Repurpose the section header fields to store trampoline info
    builder.sections.get_mut(trampoline_section).sh_addr = u32::from_le_bytes(*b"LTBX").into();
    builder.sections.get_mut(trampoline_section).sh_offset = trampoline_base_addr;
    builder.sections.get_mut(trampoline_section).sh_entsize = trampoline_data.len() as u64;

    let mut out = vec![];
    builder
        .write(&mut out)
        .map_err(|e| Error::GenerateObjFileError(e.to_string()))?;

    // Pad so that the trampoline starts at a page-aligned file offset.
    // The loader calculates: file_offset = file.size() - tramp_size
    // For mmap to work, file_offset must be page-aligned (0x1000).
    // So we need: (out.len() + padding + tramp_size) - tramp_size = out.len() + padding
    // to be page-aligned, i.e., out.len() + padding ≡ 0 (mod 0x1000)
    let padding_needed = {
        let remainder = out.len() % 0x1000;
        if remainder == 0 {
            0
        } else {
            0x1000 - remainder
        }
    };
    out.extend(core::iter::repeat_n(0u8, padding_needed));
    out.extend_from_slice(&trampoline_data);

    Ok(out)
}

/// Get the section IDs for the text sections
fn text_sections(
    builder: &object::build::elf::Builder<'_>,
) -> Result<Vec<object::build::elf::SectionId>> {
    let text_sections: Vec<_> = builder
        .sections
        .iter()
        .filter(|s| {
            s.sh_type == object::elf::SHT_PROGBITS
                && s.sh_flags & u64::from(object::elf::SHF_ALLOC) != 0
                && s.sh_flags & u64::from(object::elf::SHF_EXECINSTR) != 0
        })
        .map(object::build::elf::Section::id)
        .collect();
    if text_sections.is_empty() {
        return Err(Error::NoTextSectionFound);
    }
    Ok(text_sections)
}

/// Sets up the trampoline section
fn setup_trampoline_section(
    builder: &mut object::build::elf::Builder<'_>,
) -> Result<object::build::elf::SectionId> {
    if builder
        .sections
        .iter()
        .any(|s| s.name == TRAMPOLINE_SECTION_NAME.into())
    {
        return Err(Error::AlreadyHooked);
    }
    let s = builder.sections.add();
    *s.name.to_mut() = TRAMPOLINE_SECTION_NAME.into();
    s.sh_type = object::elf::SHT_PROGBITS;
    s.sh_flags = object::elf::SHF_ALLOC.into();
    s.sh_addralign = 8;
    s.sh_size = 0; // Must be 0 for loader to recognize as trampoline section
    Ok(s.id())
}

/// Find address for trampoline code
fn find_addr_for_trampoline_code(builder: &object::build::elf::Builder<'_>) -> u64 {
    let max_virtual_addr = builder
        .segments
        .iter()
        .filter(|seg| seg.p_type == object::elf::PT_LOAD)
        .map(|seg| seg.p_vaddr + seg.p_memsz)
        .max()
        .unwrap();

    // Place trampoline a safe distance from max_virtual_addr to avoid collision with brk.
    // The brk (heap) starts at max_virtual_addr rounded up to page boundary.
    // By placing our trampoline 4MB (0x400000) above that, we give the heap room to grow
    // before it would reach our trampoline. The shim's mmap calls for heap should
    // avoid this region.
    //
    // 4MB is chosen as a reasonable buffer that most programs won't exceed during
    // their initial brk expansion while still being within ARM64 branch range.
    let base = max_virtual_addr.next_multiple_of(0x1000);
    base + 0x400000 // 4MB above end of program
}

/// Get all control transfer targets in the text sections
fn get_control_transfer_targets(
    builder: &object::build::elf::Builder<'_>,
    text_sections: &[object::build::elf::SectionId],
) -> HashSet<u64> {
    let mut targets = HashSet::new();

    for s in text_sections {
        let s = builder.sections.get(*s);
        let object::build::elf::SectionData::Data(section_data) = &s.data else {
            continue;
        };

        let instructions = decode_section(s.sh_addr, section_data);
        for insn in &instructions {
            if let Some(target) = insn.branch_target() {
                targets.insert(target);
            }
        }
    }

    targets
}

/// Hook all syscalls and MSR TPIDR_EL0 instructions in a section
fn hook_syscalls_in_section(
    control_transfer_targets: &HashSet<u64>,
    section_base_addr: u64,
    section_data: &mut [u8],
    trampoline_base_addr: u64,
    trampoline_data: &mut Vec<u8>,
) -> Result<()> {
    let instructions = decode_section(section_base_addr, section_data);

    let mut found_hookable = false;

    for (i, insn) in instructions.iter().enumerate() {
        // Check for SVC #0 (syscall)
        let is_svc_0 = insn.is_svc() && {
            let opcode = u32::from_le_bytes(insn.bytes);
            let imm16 = (opcode >> 5) & 0xFFFF;
            imm16 == 0
        };

        // Check for MSR TPIDR_EL0, Xn
        let is_msr_tpidr = insn.is_msr_tpidr_el0();

        if !is_svc_0 && !is_msr_tpidr {
            continue;
        }

        found_hookable = true;

        // ARM64 B instruction can only reach +/- 128MB
        // Check if trampoline is in range
        let trampoline_target = trampoline_base_addr + trampoline_data.len() as u64;
        let offset = trampoline_target as i64 - insn.addr as i64;

        if offset.abs() >= (1 << 27) {
            // Trampoline out of range - need indirect branch
            // Look for space before the instruction to insert longer sequence
            let mut replace_start = insn.addr;
            let mut instructions_to_copy = Vec::new();

            for j in (0..i).rev() {
                let prev = &instructions[j];
                if prev.is_branch() {
                    break;
                }
                if control_transfer_targets.contains(&prev.addr) {
                    break;
                }
                instructions_to_copy.insert(0, prev.clone());
                replace_start = prev.addr;

                // We need 12 bytes minimum for: ADRP+ADD+BR
                if insn.next_ip() - replace_start >= 12 {
                    break;
                }
            }

            if insn.next_ip() - replace_start < 12 {
                // Try looking after
                let mut replace_end = insn.next_ip();
                let mut after_insns = Vec::new();
                for next in instructions.iter().skip(i + 1) {
                    if control_transfer_targets.contains(&next.addr) {
                        break;
                    }
                    after_insns.push(next.clone());
                    replace_end = next.next_ip();
                    if replace_end - replace_start >= 12 {
                        break;
                    }
                    if next.is_branch() {
                        break;
                    }
                }

                if replace_end - replace_start < 12 {
                    return Err(Error::InsufficientBytesBeforeOrAfter(insn.addr));
                }

                if is_svc_0 {
                    generate_trampoline_indirect(
                        section_base_addr,
                        section_data,
                        replace_start,
                        replace_end,
                        insn,
                        &instructions_to_copy,
                        &after_insns,
                        trampoline_base_addr,
                        trampoline_data,
                    )?;
                } else {
                    generate_msr_trampoline_indirect(
                        section_base_addr,
                        section_data,
                        replace_start,
                        replace_end,
                        insn,
                        insn.msr_tpidr_el0_source_reg(),
                        &instructions_to_copy,
                        &after_insns,
                        trampoline_base_addr,
                        trampoline_data,
                    )?;
                }
                continue;
            }

            if is_svc_0 {
                generate_trampoline_indirect(
                    section_base_addr,
                    section_data,
                    replace_start,
                    insn.next_ip(),
                    insn,
                    &instructions_to_copy,
                    &[],
                    trampoline_base_addr,
                    trampoline_data,
                )?;
            } else {
                generate_msr_trampoline_indirect(
                    section_base_addr,
                    section_data,
                    replace_start,
                    insn.next_ip(),
                    insn,
                    insn.msr_tpidr_el0_source_reg(),
                    &instructions_to_copy,
                    &[],
                    trampoline_base_addr,
                    trampoline_data,
                )?;
            }
        } else {
            // Trampoline in range - can use direct branch
            if is_svc_0 {
                generate_trampoline_direct(
                    section_base_addr,
                    section_data,
                    insn,
                    trampoline_base_addr,
                    trampoline_data,
                )?;
            } else {
                generate_msr_trampoline_direct(
                    section_base_addr,
                    section_data,
                    insn,
                    insn.msr_tpidr_el0_source_reg(),
                    trampoline_base_addr,
                    trampoline_data,
                )?;
            }
        }
    }

    if !found_hookable {
        return Err(Error::NoSyscallInstructionsFound);
    }

    Ok(())
}

/// Generate sigreturn trampoline at a fixed offset (24) in the trampoline section.
///
/// On ARM64, glibc does not set `SA_RESTORER` when calling `sigaction()`.
/// The kernel normally provides sigreturn via the vDSO, but since the guest runs
/// inside the sandbox, we need our own sigreturn trampoline. This trampoline:
///
/// 1. Sets x8 = 139 (`__NR_rt_sigreturn`)
/// 2. Saves x16, x17, x30 on the stack
/// 3. Looks up host TLS from the per-thread table
/// 4. Jumps to the syscall handler
///
/// The shim then handles `rt_sigreturn` by restoring the signal context.
///
/// When the signal handler returns (via `RET`), x30 (set by `write_signal_frame`)
/// points here. SP at that point equals the signal frame address. After `SUB SP, #32`
/// and the handler computing guest SP = (SP + 32), `sys_rt_sigreturn` reads the
/// `Ucontext` from the correct frame address.
fn generate_sigreturn_trampoline(trampoline_base_addr: u64, trampoline_data: &mut Vec<u8>) {
    // 1. MOVZ X8, #139 - set syscall number to __NR_rt_sigreturn
    trampoline_data.extend_from_slice(&encoder::encode_movz(8, 139, 0));

    // 2. SUB SP, SP, #32
    trampoline_data
        .extend_from_slice(&encoder::encode_sub_imm(31, 31, 32).expect("SUB SP encoding"));

    // 3. STR X16, [SP, #0]
    trampoline_data
        .extend_from_slice(&encoder::encode_str_imm(16, 31, 0).expect("STR X16 encoding"));

    // 4. STR X17, [SP, #8]
    trampoline_data
        .extend_from_slice(&encoder::encode_str_imm(17, 31, 8).expect("STR X17 encoding"));

    // 5. STR X30, [SP, #16]
    trampoline_data
        .extend_from_slice(&encoder::encode_str_imm(30, 31, 16).expect("STR X30 encoding"));

    // 6. LDR X16, [PC, #offset] - load table pointer from header[16]
    let table_ptr_location = trampoline_base_addr + 16;
    let current_pc = trampoline_base_addr + trampoline_data.len() as u64;
    let ldr_tls_offset = table_ptr_location as i64 - current_pc as i64;
    trampoline_data.extend_from_slice(
        &encoder::encode_ldr_literal(16, ldr_tls_offset as i32).expect("LDR table ptr encoding"),
    );

    // 7. MRS X17, TPIDR_EL0
    trampoline_data.extend_from_slice(&encoder::encode_mrs_tpidr_el0(17));

    // 8. LDR X18, [X16, #0] - loop start: load table[i].guest_tpidr
    trampoline_data
        .extend_from_slice(&encoder::encode_ldr_imm(18, 16, 0).expect("LDR X18 encoding"));

    // 9. CMP X18, X17
    trampoline_data.extend_from_slice(&encoder::encode_cmp_reg(18, 17));

    // 10. B.EQ +12 (skip to step 13)
    trampoline_data
        .extend_from_slice(&encoder::encode_b_cond(encoder::COND_EQ, 12).expect("B.EQ encoding"));

    // 11. ADD X16, X16, #16
    trampoline_data
        .extend_from_slice(&encoder::encode_add_imm(16, 16, 16).expect("ADD X16 encoding"));

    // 12. B -16 (back to step 8)
    let loop_start = trampoline_base_addr + trampoline_data.len() as u64 - 16;
    let current_addr = trampoline_base_addr + trampoline_data.len() as u64;
    trampoline_data
        .extend_from_slice(&encoder::encode_b(current_addr, loop_start).expect("B loop encoding"));

    // 13. LDR X18, [X16, #8] - load host_tls from matched entry
    trampoline_data
        .extend_from_slice(&encoder::encode_ldr_imm(18, 16, 8).expect("LDR host_tls encoding"));

    // 14. MOV X30, XZR - clear return address (rt_sigreturn restores full context)
    trampoline_data.extend_from_slice(&encoder::encode_mov_reg(30, 31));

    // 15. LDR X16, [PC, #offset] - load handler address from header[8]
    let handler_addr_location = trampoline_base_addr + 8;
    let current_pc = trampoline_base_addr + trampoline_data.len() as u64;
    let ldr_offset = handler_addr_location as i64 - current_pc as i64;
    trampoline_data.extend_from_slice(
        &encoder::encode_ldr_literal(16, ldr_offset as i32).expect("LDR handler encoding"),
    );

    // 16. BR X16
    trampoline_data.extend_from_slice(&encoder::encode_br(16));
}

/// Generate trampoline using direct branch (B instruction)
/// Used when trampoline is within +/- 128MB
fn generate_trampoline_direct(
    section_base_addr: u64,
    section_data: &mut [u8],
    svc_insn: &DecodedInsn,
    trampoline_base_addr: u64,
    trampoline_data: &mut Vec<u8>,
) -> Result<()> {
    let trampoline_entry = trampoline_base_addr + trampoline_data.len() as u64;
    let return_addr = svc_insn.next_ip();

    // Trampoline sequence that reserves stack space and looks up host TLS
    // from a per-thread table (avoiding the multi-thread race condition).
    //
    // Stack layout after SUB:
    //   [SP+0]:  saved x16
    //   [SP+8]:  saved x17
    //   [SP+16]: saved x30
    //   [SP+24]: unused (alignment)
    //
    // Sequence:
    //  1. SUB SP, SP, #32         - reserve 32 bytes on stack
    //  2. STR X16, [SP, #0]       - save guest x16
    //  3. STR X17, [SP, #8]       - save guest x17
    //  4. STR X30, [SP, #16]      - save guest x30
    //  5. LDR X16, [PC, #offset]  - load table pointer from trampoline header offset 16
    //  6. MRS X17, TPIDR_EL0      - get guest_tpidr (unique per thread)
    //  7. LDR X18, [X16, #0]      - load table[i].guest_tpidr
    //  8. CMP X18, X17            - compare with our guest_tpidr
    //  9. B.EQ +12                - match found → skip to step 12
    // 10. ADD X16, X16, #16       - advance to next table entry
    // 11. B -16                   - retry from step 7
    // 12. LDR X18, [X16, #8]     - load host_tls from matched entry
    // 13. ADR X30, return_addr    - set return address
    // 14. LDR X16, [PC, #offset] - load handler address from trampoline header offset 8
    // 15. BR X16                  - jump to handler
    //
    // syscall_callback knows: original guest SP = current SP + 32

    // 1. SUB SP, SP, #32
    if let Some(sub_insn) = encoder::encode_sub_imm(31, 31, 32) {
        trampoline_data.extend_from_slice(&sub_insn);
    } else {
        return Err(Error::InsufficientBytesBeforeOrAfter(svc_insn.addr));
    }

    // 2. STR X16, [SP, #0]
    if let Some(str_insn) = encoder::encode_str_imm(16, 31, 0) {
        trampoline_data.extend_from_slice(&str_insn);
    } else {
        return Err(Error::InsufficientBytesBeforeOrAfter(svc_insn.addr));
    }

    // 3. STR X17, [SP, #8]
    if let Some(str_insn) = encoder::encode_str_imm(17, 31, 8) {
        trampoline_data.extend_from_slice(&str_insn);
    } else {
        return Err(Error::InsufficientBytesBeforeOrAfter(svc_insn.addr));
    }

    // 4. STR X30, [SP, #16]
    if let Some(str_insn) = encoder::encode_str_imm(30, 31, 16) {
        trampoline_data.extend_from_slice(&str_insn);
    } else {
        return Err(Error::InsufficientBytesBeforeOrAfter(svc_insn.addr));
    }

    // 5. LDR X16, [PC, #offset] - load table pointer from trampoline header offset 16
    let table_ptr_location = trampoline_base_addr + 16;
    let current_pc = trampoline_base_addr + trampoline_data.len() as u64;
    let ldr_tls_offset = table_ptr_location as i64 - current_pc as i64;
    if let Some(ldr) = encoder::encode_ldr_literal(16, ldr_tls_offset as i32) {
        trampoline_data.extend_from_slice(&ldr);
    } else {
        return Err(Error::InsufficientBytesBeforeOrAfter(svc_insn.addr));
    }

    // 6. MRS X17, TPIDR_EL0 - get guest_tpidr
    trampoline_data.extend_from_slice(&encoder::encode_mrs_tpidr_el0(17));

    // 7. LDR X18, [X16, #0] - load table[i].guest_tpidr (loop start)
    if let Some(ldr) = encoder::encode_ldr_imm(18, 16, 0) {
        trampoline_data.extend_from_slice(&ldr);
    } else {
        return Err(Error::InsufficientBytesBeforeOrAfter(svc_insn.addr));
    }

    // 8. CMP X18, X17 - compare with our guest_tpidr
    trampoline_data.extend_from_slice(&encoder::encode_cmp_reg(18, 17));

    // 9. B.EQ +12 - skip to step 12 (3 instructions forward: ADD, B, then LDR)
    if let Some(beq) = encoder::encode_b_cond(0 /* EQ */, 12) {
        trampoline_data.extend_from_slice(&beq);
    } else {
        return Err(Error::InsufficientBytesBeforeOrAfter(svc_insn.addr));
    }

    // 10. ADD X16, X16, #16 - advance to next table entry
    if let Some(add) = encoder::encode_add_imm(16, 16, 16) {
        trampoline_data.extend_from_slice(&add);
    } else {
        return Err(Error::InsufficientBytesBeforeOrAfter(svc_insn.addr));
    }

    // 11. B -16 - back to step 7 (4 instructions back)
    let loop_start = trampoline_base_addr + trampoline_data.len() as u64;
    let loop_target = loop_start - 16; // back 4 instructions
    if let Some(b) = encoder::encode_b(loop_start, loop_target) {
        trampoline_data.extend_from_slice(&b);
    } else {
        return Err(Error::InsufficientBytesBeforeOrAfter(svc_insn.addr));
    }

    // 12. LDR X18, [X16, #8] - load host_tls from matched entry
    if let Some(ldr) = encoder::encode_ldr_imm(18, 16, 8) {
        trampoline_data.extend_from_slice(&ldr);
    } else {
        return Err(Error::InsufficientBytesBeforeOrAfter(svc_insn.addr));
    }

    // 13. Set X30 to return_addr using PC-relative addressing.
    // Must use PC-relative (not absolute MOV) so it works with ET_DYN binaries
    // that are loaded at an arbitrary base address.
    let current_pc = trampoline_base_addr + trampoline_data.len() as u64;
    let adr_offset = return_addr as i64 - current_pc as i64;
    if let Some(adr) = encoder::encode_adr(30, adr_offset as i32) {
        // ADR has ±1MB range
        trampoline_data.extend_from_slice(&adr);
    } else {
        // Fall back to ADRP+ADD for ±4GB range (still PC-relative)
        let target_page = return_addr & !0xFFF;
        let current_pc = trampoline_base_addr + trampoline_data.len() as u64;
        let pc_page = current_pc & !0xFFF;
        let page_offset = target_page as i64 - pc_page as i64;
        if let Some(adrp) = encoder::encode_adrp(30, page_offset) {
            trampoline_data.extend_from_slice(&adrp);
            let within_page = (return_addr & 0xFFF) as u16;
            if let Some(add) = encoder::encode_add_imm(30, 30, within_page) {
                trampoline_data.extend_from_slice(&add);
            } else {
                return Err(Error::InsufficientBytesBeforeOrAfter(svc_insn.addr));
            }
        } else {
            return Err(Error::InsufficientBytesBeforeOrAfter(svc_insn.addr));
        }
    }

    // 14. LDR X16, [PC, #offset] - load handler address from trampoline header offset 8
    let handler_addr_location = trampoline_base_addr + 8;
    let current_pc = trampoline_base_addr + trampoline_data.len() as u64;
    let ldr_offset = handler_addr_location as i64 - current_pc as i64;
    if let Some(ldr) = encoder::encode_ldr_literal(16, ldr_offset as i32) {
        trampoline_data.extend_from_slice(&ldr);
    } else {
        return Err(Error::InsufficientBytesBeforeOrAfter(svc_insn.addr));
    }

    // 15. BR X16
    trampoline_data.extend_from_slice(&encoder::encode_br(16));

    // Replace SVC with B to trampoline
    let b = encoder::encode_b(svc_insn.addr, trampoline_entry)
        .ok_or(Error::InsufficientBytesBeforeOrAfter(svc_insn.addr))?;
    let offset = (svc_insn.addr - section_base_addr) as usize;
    section_data[offset..offset + 4].copy_from_slice(&b);

    Ok(())
}

/// Generate MSR TPIDR_EL0 trampoline using direct branch (B instruction)
/// Used when trampoline is within +/- 128MB.
///
/// When the guest executes `MSR TPIDR_EL0, Xn`, we intercept it to:
/// 1. Perform the actual MSR (changing the guest's TPIDR_EL0)
/// 2. Update the host TLS lookup table so the old guest_tpidr entry now
///    contains the new guest_tpidr value. This ensures that subsequent
///    SVC trampolines can still find the host TLS for this thread.
///
/// If the old TPIDR_EL0 value is not found in the table (e.g., during
/// early dynamic linker init before the platform has registered this thread),
/// we skip the table update. The sentinel value 0xFFFFFFFFFFFFFFFF marks
/// the end of valid table entries.
///
/// Stack layout after SUB:
///   [SP+0]:  saved x16
///   [SP+8]:  saved x17
///   [SP+16]: saved x18
///   [SP+24]: saved new_tpidr (temporary)
fn generate_msr_trampoline_direct(
    section_base_addr: u64,
    section_data: &mut [u8],
    msr_insn: &DecodedInsn,
    source_reg: u8,
    trampoline_base_addr: u64,
    trampoline_data: &mut Vec<u8>,
) -> Result<()> {
    let trampoline_entry = trampoline_base_addr + trampoline_data.len() as u64;
    let return_addr = msr_insn.next_ip();

    // 1. SUB SP, SP, #32
    trampoline_data.extend_from_slice(
        &encoder::encode_sub_imm(31, 31, 32)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
    );

    // 2. STR X16, [SP, #0]
    trampoline_data.extend_from_slice(
        &encoder::encode_str_imm(16, 31, 0)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
    );

    // 3. STR X17, [SP, #8]
    trampoline_data.extend_from_slice(
        &encoder::encode_str_imm(17, 31, 8)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
    );

    // 4. STR X18, [SP, #16]
    trampoline_data.extend_from_slice(
        &encoder::encode_str_imm(18, 31, 16)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
    );

    // 5. MRS X16, TPIDR_EL0 - x16 = old_tpidr
    trampoline_data.extend_from_slice(&encoder::encode_mrs_tpidr_el0(16));

    // 6. Get new_tpidr into X17
    // Special handling: if source_reg is one of x16/x17/x18, it was already
    // saved to the stack, so we load from there instead.
    match source_reg {
        16 => {
            trampoline_data.extend_from_slice(
                &encoder::encode_ldr_imm(17, 31, 0)
                    .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
            );
        }
        17 => {
            trampoline_data.extend_from_slice(
                &encoder::encode_ldr_imm(17, 31, 8)
                    .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
            );
        }
        18 => {
            trampoline_data.extend_from_slice(
                &encoder::encode_ldr_imm(17, 31, 16)
                    .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
            );
        }
        _ => {
            trampoline_data.extend_from_slice(&encoder::encode_mov_reg(17, source_reg));
        }
    }

    // 7. MSR TPIDR_EL0, X17 - perform the actual MSR
    trampoline_data.extend_from_slice(&encoder::encode_msr_tpidr_el0(17));

    // 8. STR X17, [SP, #24] - save new_tpidr
    trampoline_data.extend_from_slice(
        &encoder::encode_str_imm(17, 31, 24)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
    );

    // 9. MOV X17, X16 - x17 = old_tpidr (for comparison key)
    trampoline_data.extend_from_slice(&encoder::encode_mov_reg(17, 16));

    // 10. LDR X16, [PC, #offset] - load table pointer from trampoline header offset 16
    let table_ptr_location = trampoline_base_addr + 16;
    let current_pc = trampoline_base_addr + trampoline_data.len() as u64;
    let ldr_offset = table_ptr_location as i64 - current_pc as i64;
    trampoline_data.extend_from_slice(
        &encoder::encode_ldr_literal(16, ldr_offset as i32)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
    );

    // --- Table scan loop with sentinel check ---
    // 11. LDR X18, [X16, #0] - load table[i].guest_tpidr (loop start)
    trampoline_data.extend_from_slice(
        &encoder::encode_ldr_imm(18, 16, 0)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
    );

    // 12. CMN X18, #1 - check for sentinel (0xFFFFFFFFFFFFFFFF)
    trampoline_data.extend_from_slice(
        &encoder::encode_cmn_imm(18, 1)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
    );

    // 13. B.EQ skip_update (+28) - sentinel found, no entry for this thread
    //     skip_update is 7 instructions ahead (CMP, B.EQ, ADD, B, LDR, STR, then skip_update LDR)
    trampoline_data.extend_from_slice(
        &encoder::encode_b_cond(encoder::COND_EQ, 28)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
    );

    // 14. CMP X18, X17 - compare with old_tpidr
    trampoline_data.extend_from_slice(&encoder::encode_cmp_reg(18, 17));

    // 15. B.EQ found (+12) - match found, skip to table update
    //     found is 3 instructions ahead: ADD, B, then LDR (at found label)
    trampoline_data.extend_from_slice(
        &encoder::encode_b_cond(encoder::COND_EQ, 12)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
    );

    // 16. ADD X16, X16, #16 - next entry
    trampoline_data.extend_from_slice(
        &encoder::encode_add_imm(16, 16, 16)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
    );

    // 17. B loop (-24) - back to step 11 (6 instructions back)
    let loop_start = trampoline_base_addr + trampoline_data.len() as u64;
    let loop_target = loop_start - 24;
    trampoline_data.extend_from_slice(
        &encoder::encode_b(loop_start, loop_target)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
    );

    // --- found: update table entry ---
    // 18. LDR X18, [SP, #24] - x18 = new_tpidr
    trampoline_data.extend_from_slice(
        &encoder::encode_ldr_imm(18, 31, 24)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
    );

    // 19. STR X18, [X16, #0] - update table[i].guest_tpidr = new_tpidr
    trampoline_data.extend_from_slice(
        &encoder::encode_str_imm(18, 16, 0)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
    );

    // --- skip_update: restore and return ---
    // 20. LDR X18, [SP, #16] - restore guest x18
    trampoline_data.extend_from_slice(
        &encoder::encode_ldr_imm(18, 31, 16)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
    );

    // 21. LDR X16, [SP, #0] - restore guest x16
    trampoline_data.extend_from_slice(
        &encoder::encode_ldr_imm(16, 31, 0)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
    );

    // 22. LDR X17, [SP, #8] - restore guest x17
    trampoline_data.extend_from_slice(
        &encoder::encode_ldr_imm(17, 31, 8)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
    );

    // 23. ADD SP, SP, #32 - restore stack
    trampoline_data.extend_from_slice(
        &encoder::encode_add_imm(31, 31, 32)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
    );

    // 24. B return_addr - branch back to instruction after original MSR
    let current_addr = trampoline_base_addr + trampoline_data.len() as u64;
    if let Some(b) = encoder::encode_b(current_addr, return_addr) {
        trampoline_data.extend_from_slice(&b);
    } else {
        // Fallback: ADRP+ADD+BR for far targets
        let target_page = return_addr & !0xFFF;
        let current_pc = trampoline_base_addr + trampoline_data.len() as u64;
        let pc_page = current_pc & !0xFFF;
        let page_offset = target_page as i64 - pc_page as i64;
        let adrp = encoder::encode_adrp(16, page_offset)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?;
        trampoline_data.extend_from_slice(&adrp);
        let within_page = (return_addr & 0xFFF) as u16;
        let add = encoder::encode_add_imm(16, 16, within_page)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?;
        trampoline_data.extend_from_slice(&add);
        trampoline_data.extend_from_slice(&encoder::encode_br(16));
    }

    // Replace original MSR with B to trampoline
    let b = encoder::encode_b(msr_insn.addr, trampoline_entry)
        .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?;
    let offset = (msr_insn.addr - section_base_addr) as usize;
    section_data[offset..offset + 4].copy_from_slice(&b);

    Ok(())
}

/// Generate trampoline using indirect branch (for far targets)
#[allow(clippy::too_many_arguments)]
fn generate_trampoline_indirect(
    section_base_addr: u64,
    section_data: &mut [u8],
    replace_start: u64,
    replace_end: u64,
    svc_insn: &DecodedInsn,
    before_insns: &[DecodedInsn],
    after_insns: &[DecodedInsn],
    trampoline_base_addr: u64,
    trampoline_data: &mut Vec<u8>,
) -> Result<()> {
    let trampoline_entry = trampoline_base_addr + trampoline_data.len() as u64;

    // Copy instructions before SVC to trampoline
    for insn in before_insns {
        trampoline_data.extend_from_slice(&insn.bytes);
    }

    // Trampoline sequence that reserves stack space and looks up host TLS
    // from a per-thread table (avoiding the multi-thread race condition).
    //
    // Stack layout after SUB:
    //   [SP+0]:  saved x16
    //   [SP+8]:  saved x17
    //   [SP+16]: saved x30
    //   [SP+24]: unused (alignment)
    //
    // Sequence:
    //  1. SUB SP, SP, #32         - reserve 32 bytes on stack
    //  2. STR X16, [SP, #0]       - save guest x16
    //  3. STR X17, [SP, #8]       - save guest x17
    //  4. STR X30, [SP, #16]      - save guest x30
    //  5. LDR X16, [PC, #offset]  - load table pointer from trampoline header offset 16
    //  6. MRS X17, TPIDR_EL0      - get guest_tpidr (unique per thread)
    //  7. LDR X18, [X16, #0]      - load table[i].guest_tpidr
    //  8. CMP X18, X17            - compare with our guest_tpidr
    //  9. B.EQ +12                - match found → skip to step 12
    // 10. ADD X16, X16, #16       - advance to next table entry
    // 11. B -16                   - retry from step 7
    // 12. LDR X18, [X16, #8]     - load host_tls from matched entry
    // 13. ADR X30, return_addr    - set return address
    // 14. LDR X16, [PC, #offset] - load handler address from trampoline header offset 8
    // 15. BR X16                  - jump to handler
    //
    // syscall_callback knows: original guest SP = current SP + 32

    // 1. SUB SP, SP, #32
    if let Some(sub_insn) = encoder::encode_sub_imm(31, 31, 32) {
        trampoline_data.extend_from_slice(&sub_insn);
    } else {
        return Err(Error::InsufficientBytesBeforeOrAfter(svc_insn.addr));
    }

    // 2. STR X16, [SP, #0]
    if let Some(str_insn) = encoder::encode_str_imm(16, 31, 0) {
        trampoline_data.extend_from_slice(&str_insn);
    } else {
        return Err(Error::InsufficientBytesBeforeOrAfter(svc_insn.addr));
    }

    // 3. STR X17, [SP, #8]
    if let Some(str_insn) = encoder::encode_str_imm(17, 31, 8) {
        trampoline_data.extend_from_slice(&str_insn);
    } else {
        return Err(Error::InsufficientBytesBeforeOrAfter(svc_insn.addr));
    }

    // 4. STR X30, [SP, #16]
    if let Some(str_insn) = encoder::encode_str_imm(30, 31, 16) {
        trampoline_data.extend_from_slice(&str_insn);
    } else {
        return Err(Error::InsufficientBytesBeforeOrAfter(svc_insn.addr));
    }

    // 5. LDR X16, [PC, #offset] - load table pointer from trampoline header offset 16
    let table_ptr_location = trampoline_base_addr + 16;
    let current_pc = trampoline_base_addr + trampoline_data.len() as u64;
    let ldr_tls_offset = table_ptr_location as i64 - current_pc as i64;
    if let Some(ldr) = encoder::encode_ldr_literal(16, ldr_tls_offset as i32) {
        trampoline_data.extend_from_slice(&ldr);
    } else {
        return Err(Error::InsufficientBytesBeforeOrAfter(svc_insn.addr));
    }

    // 6. MRS X17, TPIDR_EL0 - get guest_tpidr
    trampoline_data.extend_from_slice(&encoder::encode_mrs_tpidr_el0(17));

    // 7. LDR X18, [X16, #0] - load table[i].guest_tpidr (loop start)
    if let Some(ldr) = encoder::encode_ldr_imm(18, 16, 0) {
        trampoline_data.extend_from_slice(&ldr);
    } else {
        return Err(Error::InsufficientBytesBeforeOrAfter(svc_insn.addr));
    }

    // 8. CMP X18, X17 - compare with our guest_tpidr
    trampoline_data.extend_from_slice(&encoder::encode_cmp_reg(18, 17));

    // 9. B.EQ +12 - skip to step 12 (3 instructions forward: ADD, B, then LDR)
    if let Some(beq) = encoder::encode_b_cond(0 /* EQ */, 12) {
        trampoline_data.extend_from_slice(&beq);
    } else {
        return Err(Error::InsufficientBytesBeforeOrAfter(svc_insn.addr));
    }

    // 10. ADD X16, X16, #16 - advance to next table entry
    if let Some(add) = encoder::encode_add_imm(16, 16, 16) {
        trampoline_data.extend_from_slice(&add);
    } else {
        return Err(Error::InsufficientBytesBeforeOrAfter(svc_insn.addr));
    }

    // 11. B -16 - back to step 7 (4 instructions back)
    let loop_start = trampoline_base_addr + trampoline_data.len() as u64;
    let loop_target = loop_start - 16; // back 4 instructions
    if let Some(b) = encoder::encode_b(loop_start, loop_target) {
        trampoline_data.extend_from_slice(&b);
    } else {
        return Err(Error::InsufficientBytesBeforeOrAfter(svc_insn.addr));
    }

    // 12. LDR X18, [X16, #8] - load host_tls from matched entry
    if let Some(ldr) = encoder::encode_ldr_imm(18, 16, 8) {
        trampoline_data.extend_from_slice(&ldr);
    } else {
        return Err(Error::InsufficientBytesBeforeOrAfter(svc_insn.addr));
    }

    // Calculate where after_insns will be (after the syscall call sequence)
    // Current position + ADR + LDR + BR = 12 bytes minimum
    // (could be 16 if ADRP+ADD fallback is needed, but tentative assumes best case)
    let tentative_after_insns_start = trampoline_base_addr + trampoline_data.len() as u64 + 12;

    // Put return address in X30 (LR) - matching systrap handler convention
    let return_addr = if after_insns.is_empty() {
        svc_insn.next_ip()
    } else {
        // Need to return to trampoline continuation (after the syscall sequence)
        tentative_after_insns_start
    };

    // 13. Set X30 to return_addr using PC-relative addressing.
    // Must use PC-relative (not absolute MOV) so it works with ET_DYN binaries
    // that are loaded at an arbitrary base address.
    let current_pc = trampoline_base_addr + trampoline_data.len() as u64;
    let adr_offset = return_addr as i64 - current_pc as i64;
    if let Some(adr) = encoder::encode_adr(30, adr_offset as i32) {
        // ADR has ±1MB range
        trampoline_data.extend_from_slice(&adr);
    } else {
        // Fall back to ADRP+ADD for ±4GB range (still PC-relative)
        let target_page = return_addr & !0xFFF;
        let current_pc = trampoline_base_addr + trampoline_data.len() as u64;
        let pc_page = current_pc & !0xFFF;
        let page_offset = target_page as i64 - pc_page as i64;
        if let Some(adrp) = encoder::encode_adrp(30, page_offset) {
            trampoline_data.extend_from_slice(&adrp);
            let within_page = (return_addr & 0xFFF) as u16;
            if let Some(add) = encoder::encode_add_imm(30, 30, within_page) {
                trampoline_data.extend_from_slice(&add);
            } else {
                return Err(Error::InsufficientBytesBeforeOrAfter(svc_insn.addr));
            }
        } else {
            return Err(Error::InsufficientBytesBeforeOrAfter(svc_insn.addr));
        }
    }

    // 14. LDR X16, [PC, #offset] - load handler address from trampoline header offset 8
    let handler_addr_location = trampoline_base_addr + 8;
    let current_pc = trampoline_base_addr + trampoline_data.len() as u64;
    let ldr_offset = handler_addr_location as i64 - current_pc as i64;
    if let Some(ldr) = encoder::encode_ldr_literal(16, ldr_offset as i32) {
        trampoline_data.extend_from_slice(&ldr);
    } else {
        return Err(Error::InsufficientBytesBeforeOrAfter(svc_insn.addr));
    }

    // 15. BR X16
    trampoline_data.extend_from_slice(&encoder::encode_br(16));

    // Copy instructions after SVC (if any)
    for insn in after_insns {
        trampoline_data.extend_from_slice(&insn.bytes);
    }

    // Jump back to original code
    if !after_insns.is_empty() {
        let jump_back_target = after_insns.last().unwrap().next_ip();

        let current_addr = trampoline_base_addr + trampoline_data.len() as u64;
        if let Some(b) = encoder::encode_b(current_addr, jump_back_target) {
            trampoline_data.extend_from_slice(&b);
        } else {
            // B instruction range exceeded. Use ADRP+ADD+BR for PC-relative far jump.
            // This works correctly with ET_DYN binaries loaded at arbitrary base addresses.
            let target_page = jump_back_target & !0xFFF;
            let current_pc = trampoline_base_addr + trampoline_data.len() as u64;
            let pc_page = current_pc & !0xFFF;
            let page_offset = target_page as i64 - pc_page as i64;
            if let Some(adrp) = encoder::encode_adrp(16, page_offset) {
                trampoline_data.extend_from_slice(&adrp);
                let within_page = (jump_back_target & 0xFFF) as u16;
                if let Some(add) = encoder::encode_add_imm(16, 16, within_page) {
                    trampoline_data.extend_from_slice(&add);
                    trampoline_data.extend_from_slice(&encoder::encode_br(16));
                } else {
                    return Err(Error::InsufficientBytesBeforeOrAfter(svc_insn.addr));
                }
            } else {
                return Err(Error::InsufficientBytesBeforeOrAfter(svc_insn.addr));
            }
        }
    }

    // Replace original code with jump to trampoline using PC-relative addressing.
    // Must use PC-relative (not absolute literal) so it works with ET_DYN binaries
    // that are loaded at an arbitrary base address.
    let replace_len = (replace_end - replace_start) as usize;
    let replace_offset = (replace_start - section_base_addr) as usize;

    if replace_len >= 12 {
        // Use ADRP+ADD+BR (12 bytes, PC-relative, works with ET_DYN)
        let target_page = trampoline_entry & !0xFFF;
        let pc_page = replace_start & !0xFFF;
        let page_offset = target_page as i64 - pc_page as i64;
        let adrp = encoder::encode_adrp(16, page_offset)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(svc_insn.addr))?;
        let within_page = (trampoline_entry & 0xFFF) as u16;
        let add = encoder::encode_add_imm(16, 16, within_page)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(svc_insn.addr))?;
        section_data[replace_offset..replace_offset + 4].copy_from_slice(&adrp);
        section_data[replace_offset + 4..replace_offset + 8].copy_from_slice(&add);
        section_data[replace_offset + 8..replace_offset + 12]
            .copy_from_slice(&encoder::encode_br(16));

        // Fill remaining with NOPs
        for i in (12..replace_len).step_by(4) {
            section_data[replace_offset + i..replace_offset + i + 4]
                .copy_from_slice(&encoder::encode_nop());
        }
    } else {
        return Err(Error::InsufficientBytesBeforeOrAfter(svc_insn.addr));
    }

    Ok(())
}

/// Generate MSR TPIDR_EL0 trampoline using indirect branch (for far targets).
/// Used when the MSR instruction is more than 128MB from the trampoline section.
///
/// Like the SVC indirect variant, this requires extra space around the MSR instruction
/// for a longer jump sequence (ADRP+ADD+BR = 12 bytes). Instructions displaced from
/// around the MSR are copied into the trampoline and executed there.
#[allow(clippy::too_many_arguments)]
fn generate_msr_trampoline_indirect(
    section_base_addr: u64,
    section_data: &mut [u8],
    replace_start: u64,
    replace_end: u64,
    msr_insn: &DecodedInsn,
    source_reg: u8,
    before_insns: &[DecodedInsn],
    after_insns: &[DecodedInsn],
    trampoline_base_addr: u64,
    trampoline_data: &mut Vec<u8>,
) -> Result<()> {
    let trampoline_entry = trampoline_base_addr + trampoline_data.len() as u64;

    // Copy instructions before MSR to trampoline
    for insn in before_insns {
        trampoline_data.extend_from_slice(&insn.bytes);
    }

    // Generate the same MSR trampoline body as the direct variant
    // 1. SUB SP, SP, #32
    trampoline_data.extend_from_slice(
        &encoder::encode_sub_imm(31, 31, 32)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
    );

    // 2. STR X16, [SP, #0]
    trampoline_data.extend_from_slice(
        &encoder::encode_str_imm(16, 31, 0)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
    );

    // 3. STR X17, [SP, #8]
    trampoline_data.extend_from_slice(
        &encoder::encode_str_imm(17, 31, 8)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
    );

    // 4. STR X18, [SP, #16]
    trampoline_data.extend_from_slice(
        &encoder::encode_str_imm(18, 31, 16)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
    );

    // 5. MRS X16, TPIDR_EL0 - x16 = old_tpidr
    trampoline_data.extend_from_slice(&encoder::encode_mrs_tpidr_el0(16));

    // 6. Get new_tpidr into X17
    match source_reg {
        16 => {
            trampoline_data.extend_from_slice(
                &encoder::encode_ldr_imm(17, 31, 0)
                    .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
            );
        }
        17 => {
            trampoline_data.extend_from_slice(
                &encoder::encode_ldr_imm(17, 31, 8)
                    .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
            );
        }
        18 => {
            trampoline_data.extend_from_slice(
                &encoder::encode_ldr_imm(17, 31, 16)
                    .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
            );
        }
        _ => {
            trampoline_data.extend_from_slice(&encoder::encode_mov_reg(17, source_reg));
        }
    }

    // 7. MSR TPIDR_EL0, X17
    trampoline_data.extend_from_slice(&encoder::encode_msr_tpidr_el0(17));

    // 8. STR X17, [SP, #24]
    trampoline_data.extend_from_slice(
        &encoder::encode_str_imm(17, 31, 24)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
    );

    // 9. MOV X17, X16
    trampoline_data.extend_from_slice(&encoder::encode_mov_reg(17, 16));

    // 10. LDR X16, [PC, #offset] - load table pointer
    let table_ptr_location = trampoline_base_addr + 16;
    let current_pc = trampoline_base_addr + trampoline_data.len() as u64;
    let ldr_offset = table_ptr_location as i64 - current_pc as i64;
    trampoline_data.extend_from_slice(
        &encoder::encode_ldr_literal(16, ldr_offset as i32)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
    );

    // --- Table scan loop with sentinel check ---
    // 11. LDR X18, [X16, #0] (loop start)
    trampoline_data.extend_from_slice(
        &encoder::encode_ldr_imm(18, 16, 0)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
    );

    // 12. CMN X18, #1 - check for sentinel (0xFFFFFFFFFFFFFFFF)
    trampoline_data.extend_from_slice(
        &encoder::encode_cmn_imm(18, 1)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
    );

    // 13. B.EQ skip_update (+28) - sentinel found, no entry for this thread
    trampoline_data.extend_from_slice(
        &encoder::encode_b_cond(encoder::COND_EQ, 28)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
    );

    // 14. CMP X18, X17
    trampoline_data.extend_from_slice(&encoder::encode_cmp_reg(18, 17));

    // 15. B.EQ found (+12)
    trampoline_data.extend_from_slice(
        &encoder::encode_b_cond(encoder::COND_EQ, 12)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
    );

    // 16. ADD X16, X16, #16
    trampoline_data.extend_from_slice(
        &encoder::encode_add_imm(16, 16, 16)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
    );

    // 17. B loop (-24) - back to step 11 (6 instructions back)
    let loop_start = trampoline_base_addr + trampoline_data.len() as u64;
    let loop_target = loop_start - 24;
    trampoline_data.extend_from_slice(
        &encoder::encode_b(loop_start, loop_target)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
    );

    // --- found: update table entry ---
    // 18. LDR X18, [SP, #24] - x18 = new_tpidr
    trampoline_data.extend_from_slice(
        &encoder::encode_ldr_imm(18, 31, 24)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
    );

    // 19. STR X18, [X16, #0] - update table[i].guest_tpidr
    trampoline_data.extend_from_slice(
        &encoder::encode_str_imm(18, 16, 0)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
    );

    // --- skip_update: restore and return ---
    // 20. LDR X18, [SP, #16]
    trampoline_data.extend_from_slice(
        &encoder::encode_ldr_imm(18, 31, 16)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
    );

    // 21. LDR X16, [SP, #0]
    trampoline_data.extend_from_slice(
        &encoder::encode_ldr_imm(16, 31, 0)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
    );

    // 22. LDR X17, [SP, #8]
    trampoline_data.extend_from_slice(
        &encoder::encode_ldr_imm(17, 31, 8)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
    );

    // 23. ADD SP, SP, #32
    trampoline_data.extend_from_slice(
        &encoder::encode_add_imm(31, 31, 32)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?,
    );

    // Copy instructions after MSR (if any)
    for insn in after_insns {
        trampoline_data.extend_from_slice(&insn.bytes);
    }

    // Jump back to original code
    let jump_back_target = if after_insns.is_empty() {
        msr_insn.next_ip()
    } else {
        after_insns.last().unwrap().next_ip()
    };

    let current_addr = trampoline_base_addr + trampoline_data.len() as u64;
    if let Some(b) = encoder::encode_b(current_addr, jump_back_target) {
        trampoline_data.extend_from_slice(&b);
    } else {
        // Far jump: ADRP+ADD+BR
        let target_page = jump_back_target & !0xFFF;
        let current_pc = trampoline_base_addr + trampoline_data.len() as u64;
        let pc_page = current_pc & !0xFFF;
        let page_offset = target_page as i64 - pc_page as i64;
        let adrp = encoder::encode_adrp(16, page_offset)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?;
        trampoline_data.extend_from_slice(&adrp);
        let within_page = (jump_back_target & 0xFFF) as u16;
        let add = encoder::encode_add_imm(16, 16, within_page)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?;
        trampoline_data.extend_from_slice(&add);
        trampoline_data.extend_from_slice(&encoder::encode_br(16));
    }

    // Replace original code with jump to trampoline using PC-relative addressing
    let replace_len = (replace_end - replace_start) as usize;
    let replace_offset = (replace_start - section_base_addr) as usize;

    if replace_len >= 12 {
        // Use ADRP+ADD+BR (12 bytes, PC-relative)
        let target_page = trampoline_entry & !0xFFF;
        let pc_page = replace_start & !0xFFF;
        let page_offset = target_page as i64 - pc_page as i64;
        let adrp = encoder::encode_adrp(16, page_offset)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?;
        let within_page = (trampoline_entry & 0xFFF) as u16;
        let add = encoder::encode_add_imm(16, 16, within_page)
            .ok_or(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr))?;
        section_data[replace_offset..replace_offset + 4].copy_from_slice(&adrp);
        section_data[replace_offset + 4..replace_offset + 8].copy_from_slice(&add);
        section_data[replace_offset + 8..replace_offset + 12]
            .copy_from_slice(&encoder::encode_br(16));

        // Fill remaining with NOPs
        for i in (12..replace_len).step_by(4) {
            section_data[replace_offset + i..replace_offset + i + 4]
                .copy_from_slice(&encoder::encode_nop());
        }
    } else {
        return Err(Error::InsufficientBytesBeforeOrAfter(msr_insn.addr));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_b() {
        // B +4 (next instruction)
        let b = encoder::encode_b(0x1000, 0x1004).unwrap();
        assert_eq!(b, [0x01, 0x00, 0x00, 0x14]); // B #4

        // B -4 (previous instruction)
        let b = encoder::encode_b(0x1004, 0x1000).unwrap();
        assert_eq!(b, [0xFF, 0xFF, 0xFF, 0x17]); // B #-4
    }

    #[test]
    fn test_encode_nop() {
        let nop = encoder::encode_nop();
        assert_eq!(nop, [0x1F, 0x20, 0x03, 0xD5]);
    }

    #[test]
    fn test_encode_br() {
        // BR X16: D61F0200
        let br = encoder::encode_br(16);
        assert_eq!(br, [0x00, 0x02, 0x1F, 0xD6]);

        // RET (X30): D65F03C0 - this is different from BR X30!
        let ret = encoder::encode_ret();
        assert_eq!(ret, [0xC0, 0x03, 0x5F, 0xD6]);

        // BR X30 (not RET - different encoding): D61F03C0
        let br_x30 = encoder::encode_br(30);
        assert_eq!(br_x30, [0xC0, 0x03, 0x1F, 0xD6]);
    }

    #[test]
    fn test_encode_movz() {
        // MOVZ X0, #0x1234
        let movz = encoder::encode_movz(0, 0x1234, 0);
        // Expected: 0xD2 0x82 0x46 0x80 -> little endian
        let decoded = u32::from_le_bytes(movz);
        assert_eq!(decoded & 0xFF80_0000, 0xD280_0000); // MOVZ 64-bit
        assert_eq!((decoded >> 5) & 0xFFFF, 0x1234); // imm16
        assert_eq!(decoded & 0x1F, 0); // Rd
    }

    #[test]
    fn test_encode_mov_imm64() {
        // Small value - single MOVZ
        let insns = encoder::encode_mov_imm64(0, 0x1234);
        assert_eq!(insns.len(), 1);

        // Large value - multiple instructions
        let insns = encoder::encode_mov_imm64(0, 0x1234_5678_9ABC_DEF0);
        assert!(insns.len() > 1);
    }
}
