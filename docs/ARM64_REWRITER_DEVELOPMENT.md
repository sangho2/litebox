# ARM64 Syscall Rewriter Development Log

This document captures the complete development history, findings, successes, failures, and lessons learned during the implementation of ARM64 syscall rewriter support for LiteBox.

## Table of Contents

1. [Project Overview](#project-overview)
2. [Implementation Timeline](#implementation-timeline)
3. [Technical Architecture](#technical-architecture)
4. [Bug Fixes and Discoveries](#bug-fixes-and-discoveries)
5. [Current Blocker](#current-blocker)
6. [Code Artifacts](#code-artifacts)
7. [Lessons Learned](#lessons-learned)
8. [Future Work](#future-work)

---

## Project Overview

### Goal
Add ARM64 (aarch64) support to LiteBox using a syscall rewriter backend that:
1. Scans ELF binaries for `SVC #0` (syscall) instructions
2. Replaces each SVC with a branch to a generated trampoline
3. Trampolines intercept syscalls and redirect to the shim for emulation

### Why Rewriter?
- **Seccomp backend issues**: The alternative seccomp-based syscall interception has timing issues where SIGSYS signals arrive before the handler is ready
- **Performance**: Rewriting avoids signal overhead for every syscall
- **Consistency**: Matches the x86/x64 approach used in `litebox_syscall_rewriter`

---

## Implementation Timeline

### Phase 1: ARM64 Platform Support (Completed)
- Added ARM64 `PtRegs` structure with proper register layout
- Implemented TLS (Thread Local Storage) using `tpidr_el0` register
- Created ARM64 signal handling (`Sigcontext`, signal frame)
- Implemented `switch_to_guest` and `syscall_callback` in ARM64 assembly

### Phase 2: Syscall Rewriter Crate (Completed)
- Created `litebox_syscall_rewriter_arm64` crate
- Used `yaxpeax-arm` for ARM64 instruction decoding
- Implemented manual instruction encoding (ARM64 has fixed 4-byte instructions)
- Created trampoline generation logic

### Phase 3: Integration and Debugging (In Progress)
- Integrated rewriter with runner CLI
- Fixed multiple register preservation issues
- Currently debugging a crash after syscall resumption

---

## Technical Architecture

### Trampoline Section Layout

```
Trampoline Section (mapped at high address, e.g., 0x8f7000):
┌─────────────────────────────────────────────────────────────┐
│ Header (48 bytes)                                           │
│ ├── Offset 0-7:   "LITEBOX0" magic                         │
│ ├── Offset 8-15:  Handler address (syscall_callback)       │
│ ├── Offset 16-23: Host TLS (written by switch_to_guest)    │
│ ├── Offset 24-31: Saved guest x16                          │
│ ├── Offset 32-39: Saved guest x17                          │
│ └── Offset 40-47: Saved guest x30                          │
├─────────────────────────────────────────────────────────────┤
│ Entry 0 (for first SVC, ~44 bytes)                         │
├─────────────────────────────────────────────────────────────┤
│ Entry 1 (for second SVC, ~44 bytes)                        │
├─────────────────────────────────────────────────────────────┤
│ ... more entries ...                                        │
└─────────────────────────────────────────────────────────────┘
```

### Per-SVC Trampoline Entry Sequence

```asm
; 1. Save x17 to stack (we need a scratch register)
STR X17, [SP, #-16]!     ; Push x17, SP -= 16

; 2. Compute trampoline base address
ADR X17, trampoline_base ; X17 = address of header

; 3. Load host TLS from header
LDR X18, [X17, #16]      ; X18 = host TPIDR_EL0

; 4. Save guest x16 to header
STR X16, [X17, #24]      ; header.saved_x16 = guest x16

; 5. Pop original x17 into x16
LDR X16, [SP], #16       ; X16 = original x17, SP += 16

; 6. Save guest x17 (now in x16) to header
STR X16, [X17, #32]      ; header.saved_x17 = guest x17

; 7. Save guest x30 to header
STR X30, [X17, #40]      ; header.saved_x30 = guest x30

; 8. Set return address (where to resume after syscall)
MOVZ X30, #return_lo     ; X30 = return address (2-instruction sequence)
MOVK X30, #return_hi, LSL #16

; 9. Load syscall handler address
LDR X16, [X17, #8]       ; X16 = syscall_callback address

; 10. Jump to handler
BR X16                   ; Begin syscall processing
```

### Register Usage

| Register | Purpose in Trampoline | Purpose in syscall_callback |
|----------|----------------------|---------------------------|
| x0-x7    | Syscall arguments    | Preserved for syscall handling |
| x8       | Syscall number       | Preserved |
| x9-x15   | Guest values         | x9, x10 used as scratch |
| x16      | Scratch, then handler addr | Loaded from header (saved guest value) |
| x17      | Scratch (base addr)  | Loaded from header (saved guest value) |
| x18      | Host TLS             | Host TLS for all operations |
| x19-x29  | Guest callee-saved   | Preserved |
| x30      | Scratch (return addr)| Loaded from header (saved guest value) |
| SP       | Guest stack          | Saved to ctx, switched to host stack |

---

## Bug Fixes and Discoveries

### Bug 1: Syscall Return Value Not Set (FIXED)

**Symptoms**: brk() syscall returned wrong value, causing memory corruption

**Root Cause**: In `handle_syscall_request`, ARM64 case was missing:
```rust
// BEFORE (missing ARM64 case):
#[cfg(target_arch = "x86")]
{ ctx.eax = return_value; }
#[cfg(target_arch = "x86_64")]
{ ctx.rax = return_value; }
// ARM64 was not handled!

// AFTER (fixed):
#[cfg(target_arch = "aarch64")]
{ ctx.regs[0] = return_value; }
```

**File**: `litebox_shim_linux/src/lib.rs`

---

### Bug 2: x28 Being Zeroed (FIXED)

**Symptoms**: Crash in memcpy at `str x7, [x28, #1176]` - x28 was 0

**Root Cause**: Old comment said "x28 holds host TLS" but we changed to use x18:
```asm
; BEFORE (wrong):
stp x9, x29, [x10, #224]  ; Stored 0 for x28!

; AFTER (fixed):
stp x28, x29, [x10, #224]  ; Store actual guest x28
```

**File**: `litebox_platform_linux_userland/src/lib.rs`

---

### Bug 3: switch_to_guest Jumping to Wrong Address (FIXED)

**Symptoms**: After syscall, guest code looped infinitely

**Root Cause**: Code used `ret` which jumps to x30 (LR), but we needed to jump to ctx.pc:
```asm
; BEFORE (wrong):
ldr x30, [x0, #240]    ; Load x30 (LR)
ldp x0, x1, [x0, #0]
ret                     ; Jumps to x30, not to ctx.pc!

; AFTER (fixed):
ldr x18, [x0, #256]    ; Load ctx.pc into x18
; ... restore other registers ...
ldp x0, x1, [x0, #0]
br x18                  ; Jump to ctx.pc
```

**File**: `litebox_platform_linux_userland/src/lib.rs`

---

### Bug 4: x30 (LR) Clobbered by Trampoline (FIXED)

**Symptoms**: After syscall return, guest did `ret` to wrong address

**Root Cause**: Trampoline's `ADR X30, return_addr` clobbered guest's original LR before we could save it

**Solution**: Save guest x30 to trampoline header before clobbering:
```asm
; Save guest x30 BEFORE overwriting it
STR X30, [X17, #40]      ; Save to header
ADR X30, return_addr     ; Now safe to clobber
```

**File**: `litebox_syscall_rewriter_arm64/src/lib.rs`

---

### Bug 5: x16/x17 Corrupted (FIXED)

**Symptoms**: Crash in `__memcpy_generic` after brk syscall

**Root Cause**: Trampoline uses x16 and x17 as scratch before syscall_callback could save them

**Solution**: Expand header to store x16, x17, x30 and save them early in trampoline:
```asm
; Use stack to save x17 first
STR X17, [SP, #-16]!     ; Push x17
ADR X17, base            ; Now can use x17 as scratch
; ... load host TLS ...
STR X16, [X17, #24]      ; Save guest x16
LDR X16, [SP], #16       ; Pop original x17 into x16
STR X16, [X17, #32]      ; Save guest x17
```

**Files**: 
- `litebox_syscall_rewriter_arm64/src/lib.rs`
- `litebox_platform_linux_userland/src/lib.rs`

---

### Bug 6: Duplicate encode_ldr_imm Functions (FIXED)

**Symptoms**: Build error - function defined twice with different signatures

**Root Cause**: Added new function without removing old one

**Solution**: Removed the old implementation at line 188, kept the improved one at line 338

**File**: `litebox_syscall_rewriter_arm64/src/lib.rs`

---

## Resolved Bugs

### Bug 7: Guest Crashes at PC=0 After Resuming (FIXED)

**Symptoms**:
- Two brk syscalls complete successfully
- Guest resumes at valid PC (0x40bd58)
- Guest then crashes trying to execute at PC=0

**Root Cause**:
The LDR post-index instruction for restoring SP was incorrectly encoded:
- Bug: `0xF840_43F0` decoded to `LDR X16, [SP], #4` (immediate = 4)
- Fix: `0xF841_07F0` decodes to `LDR X16, [SP], #16` (immediate = 16)

The trampoline's push/pop sequence was:
1. `STR X17, [SP, #-16]!` - decrements SP by 16
2. `LDR X16, [SP], #4` (BUG) - increments SP by only 4

This left SP unbalanced by 12 bytes per syscall, eventually corrupting the guest's
stack frame and causing the `LDP X29, X30, [SP], #32` to load x30=0.

**Fix**:
Changed `litebox_syscall_rewriter_arm64/src/lib.rs`:
```rust
// BEFORE (wrong):
let ldr_pop = 0xF840_43F0u32; // LDR X16, [SP], #4 (incorrect!)

// AFTER (fixed):
let ldr_pop = 0xF841_07F0u32; // LDR X16, [SP], #16
```

**Verification**:
- Hello world test now runs successfully
- SP remains stable across multiple syscalls
- All unit tests pass

---

## Code Artifacts

### New Crates Created

```
litebox_syscall_rewriter_arm64/
├── Cargo.toml           # Dependencies: yaxpeax-arm, object, thiserror
├── src/
│   └── lib.rs           # ~900 lines: decoder, encoder, trampoline generator
└── tests/
    ├── hello-arm64      # Test binary
    ├── snapshots/       # Insta snapshots
    └── snapshot_tests.rs
```

### Key Functions in litebox_syscall_rewriter_arm64

| Function | Purpose |
|----------|---------|
| `rewrite_syscalls()` | Main entry point, processes ELF |
| `decode_section()` | Finds all SVC instructions |
| `generate_trampoline_direct()` | Creates trampoline for near branches |
| `generate_trampoline_indirect()` | Creates trampoline for far branches |
| `encoder::encode_adr()` | Encodes ADR instruction |
| `encoder::encode_b()` | Encodes B (branch) instruction |
| `encoder::encode_ldr_imm()` | Encodes LDR with immediate offset |
| `encoder::encode_str_imm()` | Encodes STR with immediate offset |
| `encoder::encode_mov_imm64()` | Encodes 64-bit immediate load (MOVZ+MOVK) |

### Modified Platform Files

| File | Changes |
|------|---------|
| `litebox_platform_linux_userland/src/lib.rs` | switch_to_guest jumps to ctx.pc via x18; syscall_callback loads saved regs from header for rewriter mode |
| `litebox_shim_linux/src/lib.rs` | ARM64 syscall return value (ctx.regs[0]) |
| `litebox_common_linux/src/loader.rs` | Trampoline section loading and mapping |

---

## Lessons Learned

### 1. ARM64 Calling Convention Complexity
- x16/x17 are "intra-procedure-call scratch registers" - can be clobbered by PLT stubs
- x18 is the "platform register" - used for TLS on Linux
- x30 is the link register - must be preserved carefully

### 2. Trampoline Design Challenges
- Can't assume any register is available as scratch
- Must save registers BEFORE using them, but need a register to compute where to save
- Solution: Use stack temporarily, then restore

### 3. Switch_to_guest Must Use PC, Not LR
- ARM64 `ret` jumps to x30 (LR), not to a "return address" from stack
- Must load ctx.pc into a register and use `br` to jump

### 4. Stack Balance is Critical
- Any SP modification in trampoline must be perfectly balanced
- Even 16-byte misalignment can corrupt caller's saved frame

### 5. Debug Incrementally
- Each bug fix revealed the next bug
- Coredump analysis with GDB was essential
- Adding verification assertions caught issues early
- **Hardcoded instruction encodings should always be verified with an assembler**

---

## Future Work

### Short-term
1. Clean up debug output
2. Run full test suite
3. Enable runner tests (currently ignored pending backend fixes)

### Medium-term
1. Fix seccomp backend timing issues
2. Add support for position-independent executables (PIE)
3. Handle SVE/NEON context in signals

### Long-term
1. Performance optimization
2. Support for dynamic libraries with rewriter
3. Consider hotpatching approach (like x86)

---

## Testing Commands

```bash
# Build
cargo build -p litebox_runner_linux_arm64_userland

# Run with rewriter backend
./target/debug/litebox_runner_linux_arm64_userland \
    --unstable \
    --interception-backend rewriter \
    --rewrite-syscalls \
    --initial-files /tmp/test_static.tar \
    /tmp/test_static

# Analyze coredump
coredumpctl debug -1 -A "-batch -ex 'bt' -ex 'info registers'"

# Check trampoline encoding
dd if=/tmp/rewritten_binary bs=1 skip=$((0x911000)) count=64 | od -A x -t x4
```

---

## References

- ARM Architecture Reference Manual (ARMv8-A)
- Linux kernel source: `arch/arm64/include/uapi/asm/sigcontext.h`
- yaxpeax-arm crate documentation
- LiteBox x86 syscall rewriter implementation

---

*Last updated: 2026-02-05*
