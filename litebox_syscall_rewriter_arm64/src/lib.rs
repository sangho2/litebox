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

    /// Encode LDR Xt, [Xn, #imm12] (unsigned offset)
    /// Load 64-bit value from memory
    pub fn encode_ldr_imm(rt: u8, rn: u8, imm12: u16) -> [u8; 4] {
        assert!(rt < 32 && rn < 32);
        assert!(imm12 < 4096);
        // LDR (immediate, unsigned offset): 1x111 00101 imm12 Rn Rt
        // size=11 (64-bit), opc=01
        let insn = 0xF940_0000 | ((imm12 as u32) << 10) | ((rn as u32) << 5) | (rt as u32);
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

    // The magic prefix for the trampoline section
    // This constant should be consistent with the definitions in the shim
    trampoline_data.extend_from_slice("LITEBOX0".as_bytes());

    // The placeholder for the address of the new syscall entry point
    let trampoline = trampoline.unwrap_or(0);
    trampoline_data.extend_from_slice(&trampoline.to_le_bytes());

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

    // Ensure the start address of the trampoline code is page-aligned
    let remain = out.len() % 0x1000;
    out.extend_from_slice(&vec![0; if remain == 0 { 0 } else { 0x1000 - remain }]);
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

    max_virtual_addr.next_multiple_of(0x1000)
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

/// Hook all syscalls in a section
fn hook_syscalls_in_section(
    control_transfer_targets: &HashSet<u64>,
    section_base_addr: u64,
    section_data: &mut [u8],
    trampoline_base_addr: u64,
    trampoline_data: &mut Vec<u8>,
) -> Result<()> {
    let instructions = decode_section(section_base_addr, section_data);

    let mut found_syscall = false;

    for (i, insn) in instructions.iter().enumerate() {
        // Look for SVC #0 (syscall)
        if !insn.is_svc() {
            continue;
        }

        // Check if this is SVC #0 specifically
        // The immediate value should be 0 for syscalls
        let is_svc_0 = {
            let opcode = u32::from_le_bytes(insn.bytes);
            // SVC encoding: 11010100 000 imm16 00001
            // imm16 is bits 5-20
            let imm16 = (opcode >> 5) & 0xFFFF;
            imm16 == 0
        };

        if !is_svc_0 {
            continue;
        }

        found_syscall = true;

        // For ARM64, we need to replace the SVC with a branch to the trampoline.
        // ARM64 has fixed 4-byte instructions, and the SVC is only 4 bytes.
        // We need at least 4 bytes for a B (branch) instruction.
        //
        // Strategy:
        // 1. Replace SVC with B to trampoline
        // 2. Trampoline: set up return address in X30 (LR), then jump to handler
        // 3. Handler returns to the instruction after SVC

        // Check if we have space before the SVC for the trampoline
        // We need to find instructions before that are not control transfer targets
        let mut replace_start = insn.addr;
        let mut instructions_to_copy = Vec::new();

        // ARM64 B instruction can only reach +/- 128MB
        // Check if trampoline is in range
        let trampoline_target = trampoline_base_addr + trampoline_data.len() as u64;
        let offset = trampoline_target as i64 - insn.addr as i64;

        if offset.abs() >= (1 << 27) {
            // Trampoline out of range - need indirect branch
            // Look for space before SVC to insert longer sequence
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

                // We need 16 bytes minimum for: LDR X16, [PC+8]; BR X16; .quad addr
                if insn.next_ip() - replace_start >= 16 {
                    break;
                }
            }

            if insn.next_ip() - replace_start < 16 {
                // Try looking after
                let mut replace_end = insn.next_ip();
                let mut after_insns = Vec::new();
                for next in instructions.iter().skip(i + 1) {
                    if control_transfer_targets.contains(&next.addr) {
                        break;
                    }
                    after_insns.push(next.clone());
                    replace_end = next.next_ip();
                    if replace_end - replace_start >= 16 {
                        break;
                    }
                    if next.is_branch() {
                        break;
                    }
                }

                if replace_end - replace_start < 16 {
                    return Err(Error::InsufficientBytesBeforeOrAfter(insn.addr));
                }

                // Generate trampoline with indirect branch
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
                continue;
            }

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
            // Trampoline in range - can use direct branch
            generate_trampoline_direct(
                section_base_addr,
                section_data,
                insn,
                trampoline_base_addr,
                trampoline_data,
            )?;
        }
    }

    if !found_syscall {
        return Err(Error::NoSyscallInstructionsFound);
    }

    Ok(())
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

    // In the trampoline:
    // 1. ADR X9, return_addr (put return address in X9, will be moved to appropriate reg by handler)
    // 2. LDR X16, [PC, #8] (load handler address)
    // 3. BR X16 (jump to handler)
    // 4. .quad handler_address

    // ADR X9, return_addr
    let adr_offset =
        return_addr as i64 - (trampoline_base_addr + trampoline_data.len() as u64) as i64;
    if let Some(adr) = encoder::encode_adr(9, adr_offset as i32) {
        trampoline_data.extend_from_slice(&adr);
    } else {
        // Fall back to loading return address via LDR literal
        // We'll emit the address after the BR
        let ldr = encoder::encode_ldr_literal(9, 12).unwrap(); // skip 3 instructions (12 bytes)
        trampoline_data.extend_from_slice(&ldr);
    }

    // LDR X16, [PC, #8]
    let ldr = encoder::encode_ldr_literal(16, 8).unwrap();
    trampoline_data.extend_from_slice(&ldr);

    // BR X16
    trampoline_data.extend_from_slice(&encoder::encode_br(16));

    // .quad handler_address (placeholder - will be filled by loader from trampoline section header)
    // The handler address is at offset 8 in the trampoline data (after "LITEBOX0")
    // The handler address placeholder - will be patched by loader using trampoline section header
    // The loader reads the handler address from offset 8 in trampoline data (after "LITEBOX0")
    trampoline_data.extend_from_slice(&0u64.to_le_bytes());

    // Replace SVC with B to trampoline
    let b = encoder::encode_b(svc_insn.addr, trampoline_entry)
        .ok_or(Error::InsufficientBytesBeforeOrAfter(svc_insn.addr))?;
    let offset = (svc_insn.addr - section_base_addr) as usize;
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

    // Put return address in X9
    let return_addr = if after_insns.is_empty() {
        svc_insn.next_ip()
    } else {
        // Need to return to trampoline continuation
        trampoline_base_addr + trampoline_data.len() as u64 + 24 // after ADR+LDR+BR+.quad
    };

    // ADR X9, return_addr
    let adr_offset =
        return_addr as i64 - (trampoline_base_addr + trampoline_data.len() as u64) as i64;
    if let Some(adr) = encoder::encode_adr(9, adr_offset as i32) {
        trampoline_data.extend_from_slice(&adr);
    } else {
        // Use MOV sequence for far addresses
        for insn in encoder::encode_mov_imm64(9, return_addr) {
            trampoline_data.extend_from_slice(&insn);
        }
    }

    // LDR X16, [PC, #8]
    let ldr = encoder::encode_ldr_literal(16, 8).unwrap();
    trampoline_data.extend_from_slice(&ldr);

    // BR X16
    trampoline_data.extend_from_slice(&encoder::encode_br(16));

    // .quad handler_address (placeholder)
    trampoline_data.extend_from_slice(&0u64.to_le_bytes());

    // Copy instructions after SVC (if any)
    for insn in after_insns {
        trampoline_data.extend_from_slice(&insn.bytes);
    }

    // Jump back to original code
    if !after_insns.is_empty() || return_addr != svc_insn.next_ip() {
        let jump_back_target = if after_insns.is_empty() {
            svc_insn.next_ip()
        } else {
            after_insns.last().unwrap().next_ip()
        };

        let current_addr = trampoline_base_addr + trampoline_data.len() as u64;
        if let Some(b) = encoder::encode_b(current_addr, jump_back_target) {
            trampoline_data.extend_from_slice(&b);
        } else {
            // Need indirect jump for far target
            let ldr = encoder::encode_ldr_literal(16, 8).unwrap();
            trampoline_data.extend_from_slice(&ldr);
            trampoline_data.extend_from_slice(&encoder::encode_br(16));
            trampoline_data.extend_from_slice(&jump_back_target.to_le_bytes());
        }
    }

    // Replace original code with jump to trampoline
    let replace_len = (replace_end - replace_start) as usize;
    let replace_offset = (replace_start - section_base_addr) as usize;

    // First 16 bytes: LDR X16, [PC, #8]; BR X16; .quad trampoline_entry
    if replace_len >= 16 {
        let ldr = encoder::encode_ldr_literal(16, 8).unwrap();
        section_data[replace_offset..replace_offset + 4].copy_from_slice(&ldr);
        section_data[replace_offset + 4..replace_offset + 8]
            .copy_from_slice(&encoder::encode_br(16));
        section_data[replace_offset + 8..replace_offset + 16]
            .copy_from_slice(&trampoline_entry.to_le_bytes());

        // Fill remaining with NOPs
        for i in (16..replace_len).step_by(4) {
            section_data[replace_offset + i..replace_offset + i + 4]
                .copy_from_slice(&encoder::encode_nop());
        }
    } else {
        return Err(Error::InsufficientBytesBeforeOrAfter(svc_insn.addr));
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
