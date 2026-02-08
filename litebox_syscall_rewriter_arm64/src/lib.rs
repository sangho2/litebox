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
#[allow(dead_code, clippy::manual_range_contains)]
mod encoder {
    /// Encode a branch-immediate instruction (B or BL) with the given opcode base.
    /// offset must be 4-byte aligned and within ±128MB.
    fn encode_branch_imm26(from: u64, to: u64, opcode_base: u32) -> Option<[u8; 4]> {
        let offset = to.wrapping_sub(from) as i64;
        if offset < -(1 << 27) || offset >= (1 << 27) || offset & 3 != 0 {
            return None;
        }
        let imm26 = ((offset >> 2) as u32) & 0x03FF_FFFF;
        Some((opcode_base | imm26).to_le_bytes())
    }

    /// Encode B (unconditional branch): PC += sign_extend(imm26 << 2)
    pub fn encode_b(from: u64, to: u64) -> Option<[u8; 4]> {
        encode_branch_imm26(from, to, 0x14_000000)
    }

    /// Encode BR (branch to register): PC = Xn
    pub fn encode_br(reg: u8) -> [u8; 4] {
        assert!(reg < 32);
        (0xD61F_0000 | ((reg as u32) << 5)).to_le_bytes()
    }

    /// Encode RET (return via X30)
    pub fn encode_ret() -> [u8; 4] {
        (0xD65F_0000 | (30u32 << 5)).to_le_bytes()
    }

    /// Encode LDR Xt, [PC, #imm19] (literal, ±1MB, 4-byte aligned)
    pub fn encode_ldr_literal(rt: u8, offset: i32) -> Option<[u8; 4]> {
        assert!(rt < 32);
        if offset & 3 != 0 {
            return None;
        }
        let imm19 = offset >> 2;
        if imm19 < -(1 << 18) || imm19 >= (1 << 18) {
            return None;
        }
        Some((0x5800_0000 | (((imm19 as u32) & 0x7FFFF) << 5) | (rt as u32)).to_le_bytes())
    }

    /// Encode ADR Xd, #imm21 (PC-relative, ±1MB)
    pub fn encode_adr(rd: u8, offset: i32) -> Option<[u8; 4]> {
        assert!(rd < 32);
        if offset < -(1 << 20) || offset >= (1 << 20) {
            return None;
        }
        let immlo = (offset & 0x3) as u32;
        let immhi = ((offset >> 2) & 0x7FFFF) as u32;
        Some(((immlo << 29) | 0x1000_0000 | (immhi << 5) | (rd as u32)).to_le_bytes())
    }

    /// Encode ADRP Xd, #imm21 (PC-relative page address, ±4GB)
    pub fn encode_adrp(rd: u8, page_offset: i64) -> Option<[u8; 4]> {
        assert!(rd < 32);
        let imm21 = page_offset >> 12;
        if imm21 < -(1i64 << 20) || imm21 >= (1i64 << 20) {
            return None;
        }
        let imm21 = imm21 as u32;
        let immlo = imm21 & 0x3;
        let immhi = (imm21 >> 2) & 0x7FFFF;
        Some(
            ((1u32 << 31) | (immlo << 29) | 0x1000_0000 | (immhi << 5) | (rd as u32)).to_le_bytes(),
        )
    }

    /// Encode NOP instruction
    pub fn encode_nop() -> [u8; 4] {
        0xD503_201F_u32.to_le_bytes()
    }

    /// Encode a MOV-wide instruction (MOVZ or MOVK) with the given opcode base.
    fn encode_mov_wide(rd: u8, imm16: u16, shift: u8, opcode_base: u32) -> [u8; 4] {
        assert!(rd < 32);
        assert!(matches!(shift, 0 | 16 | 32 | 48));
        let hw = (shift / 16) as u32;
        (opcode_base | (hw << 21) | ((imm16 as u32) << 5) | (rd as u32)).to_le_bytes()
    }

    /// Encode MOVZ Xd, #imm16, LSL #shift
    pub fn encode_movz(rd: u8, imm16: u16, shift: u8) -> [u8; 4] {
        encode_mov_wide(rd, imm16, shift, 0xD280_0000)
    }

    /// Encode MOVK Xd, #imm16, LSL #shift
    pub fn encode_movk(rd: u8, imm16: u16, shift: u8) -> [u8; 4] {
        encode_mov_wide(rd, imm16, shift, 0xF280_0000)
    }

    /// Encode a 64-bit immediate load using MOVZ + MOVK sequence (1-4 instructions)
    pub fn encode_mov_imm64(rd: u8, value: u64) -> Vec<[u8; 4]> {
        let chunks: [(u16, u8); 4] = [
            ((value & 0xFFFF) as u16, 0),
            (((value >> 16) & 0xFFFF) as u16, 16),
            (((value >> 32) & 0xFFFF) as u16, 32),
            (((value >> 48) & 0xFFFF) as u16, 48),
        ];

        let mut insns = Vec::new();
        let mut first = true;
        for (chunk, shift) in chunks {
            if chunk != 0 || (first && shift == 48) {
                insns.push(if first {
                    first = false;
                    encode_movz(rd, chunk, shift)
                } else {
                    encode_movk(rd, chunk, shift)
                });
            }
        }
        if insns.is_empty() {
            insns.push(encode_movz(rd, 0, 0));
        }
        insns
    }

    /// Encode MOV Xd, Xm (alias for ORR Xd, XZR, Xm)
    pub fn encode_mov_reg(rd: u8, rm: u8) -> [u8; 4] {
        assert!(rd < 32 && rm < 32);
        (0xAA00_03E0 | ((rm as u32) << 16) | (rd as u32)).to_le_bytes()
    }

    /// Encode a load/store with unsigned immediate offset (STR or LDR).
    /// `pimm` is scaled by 8 (range 0-32760, must be 8-byte aligned).
    fn encode_ld_st_imm(rt: u8, rn: u8, pimm: u16, opcode_base: u32) -> Option<[u8; 4]> {
        assert!(rt < 32 && rn < 32);
        if !pimm.is_multiple_of(8) || pimm > 32760 {
            return None;
        }
        let imm12 = (pimm / 8) as u32;
        Some((opcode_base | (imm12 << 10) | ((rn as u32) << 5) | (rt as u32)).to_le_bytes())
    }

    /// Encode STR Xt, [Xn, #pimm] (unsigned offset, scaled by 8)
    pub fn encode_str_imm(rt: u8, rn: u8, pimm: u16) -> Option<[u8; 4]> {
        encode_ld_st_imm(rt, rn, pimm, 0xF900_0000)
    }

    /// Encode LDR Xt, [Xn, #pimm] (unsigned offset, scaled by 8)
    pub fn encode_ldr_imm(rt: u8, rn: u8, pimm: u16) -> Option<[u8; 4]> {
        encode_ld_st_imm(rt, rn, pimm, 0xF940_0000)
    }

    /// Encode an add/sub immediate instruction with the given opcode base.
    fn encode_add_sub_imm(rd: u8, rn: u8, imm12: u16, opcode_base: u32) -> Option<[u8; 4]> {
        assert!(rd < 32 && rn < 32);
        if imm12 > 4095 {
            return None;
        }
        Some(
            (opcode_base | ((imm12 as u32) << 10) | ((rn as u32) << 5) | (rd as u32)).to_le_bytes(),
        )
    }

    /// Encode SUB Xd, Xn, #imm12
    pub fn encode_sub_imm(rd: u8, rn: u8, imm12: u16) -> Option<[u8; 4]> {
        encode_add_sub_imm(rd, rn, imm12, 0xD100_0000)
    }

    /// Encode ADD Xd, Xn, #imm12
    pub fn encode_add_imm(rd: u8, rn: u8, imm12: u16) -> Option<[u8; 4]> {
        encode_add_sub_imm(rd, rn, imm12, 0x9100_0000)
    }

    /// Encode MRS/MSR TPIDR_EL0 with the given base opcode.
    fn encode_tpidr_el0(rt: u8, opcode_base: u32) -> [u8; 4] {
        assert!(rt < 32);
        (opcode_base | (rt as u32)).to_le_bytes()
    }

    /// Encode MRS Xt, TPIDR_EL0 (read thread pointer)
    pub fn encode_mrs_tpidr_el0(rt: u8) -> [u8; 4] {
        encode_tpidr_el0(rt, 0xD53B_D040)
    }

    /// Encode MSR TPIDR_EL0, Xt (write thread pointer)
    pub fn encode_msr_tpidr_el0(rt: u8) -> [u8; 4] {
        encode_tpidr_el0(rt, 0xD51B_D040)
    }

    /// Encode CMP Xn, Xm (alias for SUBS XZR, Xn, Xm)
    pub fn encode_cmp_reg(rn: u8, rm: u8) -> [u8; 4] {
        assert!(rn < 32 && rm < 32);
        (0xEB00_001F | ((rm as u32) << 16) | ((rn as u32) << 5)).to_le_bytes()
    }

    /// Encode B.cond (conditional branch, ±1MB, 4-byte aligned offset)
    pub fn encode_b_cond(cond: u8, offset: i32) -> Option<[u8; 4]> {
        assert!(cond < 16);
        if offset & 3 != 0 {
            return None;
        }
        let imm19 = offset >> 2;
        if imm19 < -(1 << 18) || imm19 >= (1 << 18) {
            return None;
        }
        Some((0x5400_0000 | (((imm19 as u32) & 0x7FFFF) << 5) | (cond as u32)).to_le_bytes())
    }

    /// Encode CMN Xn, #imm12 (ADDS XZR, Xn, #imm12 — sets flags for Xn + imm12)
    pub fn encode_cmn_imm(rn: u8, imm12: u16) -> Option<[u8; 4]> {
        assert!(rn < 32);
        if imm12 > 4095 {
            return None;
        }
        Some((0xB100_001F | ((imm12 as u32) << 10) | ((rn as u32) << 5)).to_le_bytes())
    }

    /// ARM64 condition code for EQ (equal, Z=1)
    pub const COND_EQ: u8 = 0;
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

/// Emit an encoded instruction into `trampoline_data`, returning an error if encoding fails.
macro_rules! emit {
    ($trampoline_data:expr, $err_addr:expr, $encode_call:expr) => {
        $trampoline_data.extend_from_slice(
            &$encode_call.ok_or(Error::InsufficientBytesBeforeOrAfter($err_addr))?,
        )
    };
}

/// Emit a PC-relative address load into X30, using ADR if within ±1MB, else ADRP+ADD.
fn emit_pc_rel_addr_to_x30(
    target_addr: u64,
    trampoline_base_addr: u64,
    trampoline_data: &mut Vec<u8>,
    err_addr: u64,
) -> Result<()> {
    let current_pc = trampoline_base_addr + trampoline_data.len() as u64;
    let adr_offset = target_addr as i64 - current_pc as i64;
    if let Some(adr) = encoder::encode_adr(30, adr_offset as i32) {
        trampoline_data.extend_from_slice(&adr);
    } else {
        let target_page = target_addr & !0xFFF;
        let pc_page = (trampoline_base_addr + trampoline_data.len() as u64) & !0xFFF;
        let page_offset = target_page as i64 - pc_page as i64;
        emit!(
            trampoline_data,
            err_addr,
            encoder::encode_adrp(30, page_offset)
        );
        let within_page = (target_addr & 0xFFF) as u16;
        emit!(
            trampoline_data,
            err_addr,
            encoder::encode_add_imm(30, 30, within_page)
        );
    }
    Ok(())
}

/// Emit ADRP+ADD+BR for a far jump to `target` using scratch register X16.
fn emit_far_jump(
    target: u64,
    trampoline_base_addr: u64,
    trampoline_data: &mut Vec<u8>,
    err_addr: u64,
) -> Result<()> {
    let target_page = target & !0xFFF;
    let pc_page = (trampoline_base_addr + trampoline_data.len() as u64) & !0xFFF;
    let page_offset = target_page as i64 - pc_page as i64;
    emit!(
        trampoline_data,
        err_addr,
        encoder::encode_adrp(16, page_offset)
    );
    let within_page = (target & 0xFFF) as u16;
    emit!(
        trampoline_data,
        err_addr,
        encoder::encode_add_imm(16, 16, within_page)
    );
    trampoline_data.extend_from_slice(&encoder::encode_br(16));
    Ok(())
}

/// Emit B if in range, otherwise ADRP+ADD+BR, for jumping back to original code.
fn emit_jump_back(
    target: u64,
    trampoline_base_addr: u64,
    trampoline_data: &mut Vec<u8>,
    err_addr: u64,
) -> Result<()> {
    let current_addr = trampoline_base_addr + trampoline_data.len() as u64;
    if let Some(b) = encoder::encode_b(current_addr, target) {
        trampoline_data.extend_from_slice(&b);
    } else {
        emit_far_jump(target, trampoline_base_addr, trampoline_data, err_addr)?;
    }
    Ok(())
}

/// Emit the SVC trampoline body: save regs, look up host TLS, set return addr, jump to handler.
///
/// This is the shared body between direct and indirect SVC trampolines:
///  1. SUB SP, SP, #32 / save X16, X17, X30
///  2. Load TLS table pointer, scan for matching guest TPIDR_EL0
///  3. Set X30 = return_addr (PC-relative)
///  4. Load handler address, BR X16
fn emit_svc_trampoline_body(
    return_addr: u64,
    trampoline_base_addr: u64,
    trampoline_data: &mut Vec<u8>,
    err_addr: u64,
) -> Result<()> {
    // Save registers on stack
    emit!(
        trampoline_data,
        err_addr,
        encoder::encode_sub_imm(31, 31, 32)
    );
    emit!(
        trampoline_data,
        err_addr,
        encoder::encode_str_imm(16, 31, 0)
    );
    emit!(
        trampoline_data,
        err_addr,
        encoder::encode_str_imm(17, 31, 8)
    );
    emit!(
        trampoline_data,
        err_addr,
        encoder::encode_str_imm(30, 31, 16)
    );

    // Load TLS table pointer from header offset 16
    let table_ptr_location = trampoline_base_addr + 16;
    let current_pc = trampoline_base_addr + trampoline_data.len() as u64;
    emit!(
        trampoline_data,
        err_addr,
        encoder::encode_ldr_literal(16, (table_ptr_location as i64 - current_pc as i64) as i32)
    );

    // MRS X17, TPIDR_EL0
    trampoline_data.extend_from_slice(&encoder::encode_mrs_tpidr_el0(17));

    // TLS table scan loop
    emit!(
        trampoline_data,
        err_addr,
        encoder::encode_ldr_imm(18, 16, 0)
    ); // LDR X18, [X16]
    trampoline_data.extend_from_slice(&encoder::encode_cmp_reg(18, 17)); // CMP X18, X17
    emit!(
        trampoline_data,
        err_addr,
        encoder::encode_b_cond(encoder::COND_EQ, 12)
    ); // B.EQ +12
    emit!(
        trampoline_data,
        err_addr,
        encoder::encode_add_imm(16, 16, 16)
    ); // ADD X16, X16, #16
    let loop_start = trampoline_base_addr + trampoline_data.len() as u64;
    emit!(
        trampoline_data,
        err_addr,
        encoder::encode_b(loop_start, loop_start - 16)
    ); // B -16

    // Load host TLS from matched entry
    emit!(
        trampoline_data,
        err_addr,
        encoder::encode_ldr_imm(18, 16, 8)
    );

    // Set X30 to return_addr (PC-relative)
    emit_pc_rel_addr_to_x30(return_addr, trampoline_base_addr, trampoline_data, err_addr)?;

    // Load handler address from header offset 8, jump to it
    let handler_addr_location = trampoline_base_addr + 8;
    let current_pc = trampoline_base_addr + trampoline_data.len() as u64;
    emit!(
        trampoline_data,
        err_addr,
        encoder::encode_ldr_literal(
            16,
            (handler_addr_location as i64 - current_pc as i64) as i32
        )
    );
    trampoline_data.extend_from_slice(&encoder::encode_br(16));

    Ok(())
}

/// Emit the MSR TPIDR_EL0 trampoline body: save regs, perform MSR, update TLS table, restore.
///
/// When the guest writes TPIDR_EL0, we intercept to update the TLS lookup table
/// so subsequent SVC trampolines can still find the host TLS for this thread.
/// If the old TPIDR is not in the table (early init), the update is skipped.
fn emit_msr_trampoline_body(
    source_reg: u8,
    trampoline_base_addr: u64,
    trampoline_data: &mut Vec<u8>,
    err_addr: u64,
) -> Result<()> {
    // Save registers on stack
    emit!(
        trampoline_data,
        err_addr,
        encoder::encode_sub_imm(31, 31, 32)
    );
    emit!(
        trampoline_data,
        err_addr,
        encoder::encode_str_imm(16, 31, 0)
    );
    emit!(
        trampoline_data,
        err_addr,
        encoder::encode_str_imm(17, 31, 8)
    );
    emit!(
        trampoline_data,
        err_addr,
        encoder::encode_str_imm(18, 31, 16)
    );

    // MRS X16, TPIDR_EL0 — old_tpidr
    trampoline_data.extend_from_slice(&encoder::encode_mrs_tpidr_el0(16));

    // Get new_tpidr into X17 (source may have been clobbered by saves)
    match source_reg {
        16 => emit!(
            trampoline_data,
            err_addr,
            encoder::encode_ldr_imm(17, 31, 0)
        ),
        17 => emit!(
            trampoline_data,
            err_addr,
            encoder::encode_ldr_imm(17, 31, 8)
        ),
        18 => emit!(
            trampoline_data,
            err_addr,
            encoder::encode_ldr_imm(17, 31, 16)
        ),
        _ => trampoline_data.extend_from_slice(&encoder::encode_mov_reg(17, source_reg)),
    }

    // Perform the actual MSR TPIDR_EL0, X17
    trampoline_data.extend_from_slice(&encoder::encode_msr_tpidr_el0(17));

    // Save new_tpidr, set up old_tpidr as search key
    emit!(
        trampoline_data,
        err_addr,
        encoder::encode_str_imm(17, 31, 24)
    );
    trampoline_data.extend_from_slice(&encoder::encode_mov_reg(17, 16));

    // Load TLS table pointer from header offset 16
    let table_ptr_location = trampoline_base_addr + 16;
    let current_pc = trampoline_base_addr + trampoline_data.len() as u64;
    emit!(
        trampoline_data,
        err_addr,
        encoder::encode_ldr_literal(16, (table_ptr_location as i64 - current_pc as i64) as i32)
    );

    // Table scan loop with sentinel check
    emit!(
        trampoline_data,
        err_addr,
        encoder::encode_ldr_imm(18, 16, 0)
    ); // LDR X18, [X16]
    emit!(trampoline_data, err_addr, encoder::encode_cmn_imm(18, 1)); // CMN X18, #1 (sentinel)
    emit!(
        trampoline_data,
        err_addr,
        encoder::encode_b_cond(encoder::COND_EQ, 28)
    ); // B.EQ skip_update
    trampoline_data.extend_from_slice(&encoder::encode_cmp_reg(18, 17)); // CMP X18, X17
    emit!(
        trampoline_data,
        err_addr,
        encoder::encode_b_cond(encoder::COND_EQ, 12)
    ); // B.EQ found
    emit!(
        trampoline_data,
        err_addr,
        encoder::encode_add_imm(16, 16, 16)
    ); // ADD X16, X16, #16
    let loop_start = trampoline_base_addr + trampoline_data.len() as u64;
    emit!(
        trampoline_data,
        err_addr,
        encoder::encode_b(loop_start, loop_start - 24)
    ); // B -24

    // found: update table entry
    emit!(
        trampoline_data,
        err_addr,
        encoder::encode_ldr_imm(18, 31, 24)
    ); // LDR X18, [SP, #24]
    emit!(
        trampoline_data,
        err_addr,
        encoder::encode_str_imm(18, 16, 0)
    ); // STR X18, [X16]

    // skip_update: restore registers
    emit!(
        trampoline_data,
        err_addr,
        encoder::encode_ldr_imm(18, 31, 16)
    );
    emit!(
        trampoline_data,
        err_addr,
        encoder::encode_ldr_imm(16, 31, 0)
    );
    emit!(
        trampoline_data,
        err_addr,
        encoder::encode_ldr_imm(17, 31, 8)
    );
    emit!(
        trampoline_data,
        err_addr,
        encoder::encode_add_imm(31, 31, 32)
    );

    Ok(())
}

/// Replace original instructions at `[replace_start..replace_end)` with ADRP+ADD+BR to
/// `trampoline_entry`, filling any remaining space with NOPs.
fn patch_section_with_indirect_jump(
    section_base_addr: u64,
    section_data: &mut [u8],
    replace_start: u64,
    replace_end: u64,
    trampoline_entry: u64,
    err_addr: u64,
) -> Result<()> {
    let replace_len = (replace_end - replace_start) as usize;
    let replace_offset = (replace_start - section_base_addr) as usize;

    if replace_len < 12 {
        return Err(Error::InsufficientBytesBeforeOrAfter(err_addr));
    }

    let target_page = trampoline_entry & !0xFFF;
    let pc_page = replace_start & !0xFFF;
    let page_offset = target_page as i64 - pc_page as i64;
    let adrp = encoder::encode_adrp(16, page_offset)
        .ok_or(Error::InsufficientBytesBeforeOrAfter(err_addr))?;
    let within_page = (trampoline_entry & 0xFFF) as u16;
    let add = encoder::encode_add_imm(16, 16, within_page)
        .ok_or(Error::InsufficientBytesBeforeOrAfter(err_addr))?;
    section_data[replace_offset..replace_offset + 4].copy_from_slice(&adrp);
    section_data[replace_offset + 4..replace_offset + 8].copy_from_slice(&add);
    section_data[replace_offset + 8..replace_offset + 12].copy_from_slice(&encoder::encode_br(16));

    for i in (12..replace_len).step_by(4) {
        section_data[replace_offset + i..replace_offset + i + 4]
            .copy_from_slice(&encoder::encode_nop());
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

    emit_svc_trampoline_body(
        return_addr,
        trampoline_base_addr,
        trampoline_data,
        svc_insn.addr,
    )?;

    // Replace SVC with B to trampoline
    let b = encoder::encode_b(svc_insn.addr, trampoline_entry)
        .ok_or(Error::InsufficientBytesBeforeOrAfter(svc_insn.addr))?;
    let offset = (svc_insn.addr - section_base_addr) as usize;
    section_data[offset..offset + 4].copy_from_slice(&b);

    Ok(())
}

/// Generate MSR TPIDR_EL0 trampoline using direct branch (B instruction).
/// Used when trampoline is within +/- 128MB.
///
/// When the guest executes `MSR TPIDR_EL0, Xn`, we intercept it to:
/// 1. Perform the actual MSR (changing the guest's TPIDR_EL0)
/// 2. Update the host TLS lookup table so the old guest_tpidr entry now
///    contains the new guest_tpidr value.
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

    emit_msr_trampoline_body(
        source_reg,
        trampoline_base_addr,
        trampoline_data,
        msr_insn.addr,
    )?;

    // Jump back to instruction after original MSR
    emit_jump_back(
        return_addr,
        trampoline_base_addr,
        trampoline_data,
        msr_insn.addr,
    )?;

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

    // Copy displaced instructions before SVC
    for insn in before_insns {
        trampoline_data.extend_from_slice(&insn.bytes);
    }

    // Calculate return address: either back to original code or to after_insns in trampoline.
    // The SVC body emits 12 bytes minimum after the host_tls load (ADR + LDR + BR),
    // so after_insns start at current + body_size.
    let tentative_after_insns_start = trampoline_base_addr + trampoline_data.len() as u64 + 60; // approximate body size; recalculated by emit_pc_rel_addr_to_x30 at exact offset

    let return_addr = if after_insns.is_empty() {
        svc_insn.next_ip()
    } else {
        tentative_after_insns_start
    };

    emit_svc_trampoline_body(
        return_addr,
        trampoline_base_addr,
        trampoline_data,
        svc_insn.addr,
    )?;

    // Copy displaced instructions after SVC
    for insn in after_insns {
        trampoline_data.extend_from_slice(&insn.bytes);
    }

    // Jump back to original code (after all displaced instructions)
    if !after_insns.is_empty() {
        let jump_back_target = after_insns.last().unwrap().next_ip();
        emit_jump_back(
            jump_back_target,
            trampoline_base_addr,
            trampoline_data,
            svc_insn.addr,
        )?;
    }

    // Replace original code with indirect jump to trampoline
    patch_section_with_indirect_jump(
        section_base_addr,
        section_data,
        replace_start,
        replace_end,
        trampoline_entry,
        svc_insn.addr,
    )?;

    Ok(())
}

/// Generate MSR TPIDR_EL0 trampoline using indirect branch (for far targets).
/// Used when the MSR instruction is more than 128MB from the trampoline section.
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

    // Copy displaced instructions before MSR
    for insn in before_insns {
        trampoline_data.extend_from_slice(&insn.bytes);
    }

    emit_msr_trampoline_body(
        source_reg,
        trampoline_base_addr,
        trampoline_data,
        msr_insn.addr,
    )?;

    // Copy displaced instructions after MSR
    for insn in after_insns {
        trampoline_data.extend_from_slice(&insn.bytes);
    }

    // Jump back to original code
    let jump_back_target = if after_insns.is_empty() {
        msr_insn.next_ip()
    } else {
        after_insns.last().unwrap().next_ip()
    };
    emit_jump_back(
        jump_back_target,
        trampoline_base_addr,
        trampoline_data,
        msr_insn.addr,
    )?;

    // Replace original code with indirect jump to trampoline
    patch_section_with_indirect_jump(
        section_base_addr,
        section_data,
        replace_start,
        replace_end,
        trampoline_entry,
        msr_insn.addr,
    )?;

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
