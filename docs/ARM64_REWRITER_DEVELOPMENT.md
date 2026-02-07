# ARM64 Syscall Rewriter Development Log

This document captures the complete development history, findings, successes, failures, and lessons learned during the implementation of ARM64 syscall rewriter support for LiteBox.

## Table of Contents

1. [Project Overview](#project-overview)
2. [Implementation Timeline](#implementation-timeline)
3. [Technical Architecture](#technical-architecture)
4. [Bug Fixes and Discoveries](#bug-fixes-and-discoveries)
5. [Code Artifacts](#code-artifacts)
6. [Lessons Learned](#lessons-learned)
7. [Future Work](#future-work)
8. [Current Status](#current-status)

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

### Phase 3: Integration and Debugging (Completed)
- Integrated rewriter with runner CLI
- Fixed multiple register preservation issues
- Fixed stack corruption bug on initial guest entry
- All basic tests now passing

### Phase 4: Dynamic Linking and Syscall Support (Completed)
- Fixed ARM64 `FileStat` struct layout (128 bytes vs x86's 144 bytes)
- Added trampoline propagation from interpreter to main binary for dynamically linked ELFs
- Replaced absolute address encoding with PC-relative `ADRP+ADD` (±4GB) for ET_DYN binaries
- Created `litebox_rtld_audit_arm64` for LD_AUDIT-based trampoline discovery
- Implemented `faccessat`, `statfs`, `statx` syscalls
- Fixed `O_DIRECT`/`O_NDELAY` support in filesystem backends
- `ls` now works inside the sandbox (dynamically linked)

### Phase 5: Multi-Threading and TLS Support (Completed)
- Fixed missing SETTLS for ARM64 new threads (`clone` syscall)
- Fixed broken `set_guest_tpidr`/`get_guest_tpidr` — replaced incorrect pointer arithmetic with inline assembly using `:tprel_lo12:`
- Replaced racy single shared host TLS slot at `trampoline_base+16` with a per-thread host TLS table
- Table stores `(guest_tpidr, host_tls)` entries, scanned by the trampoline to recover host TLS
- Fixed signal handler host TLS recovery using power-of-2 aligned alt-stacks with host TLS stored at `aligned_base + ALT_STACK_ALLOC_SIZE - 8`
- Fixed signal frame clobbering host TLS slot by reducing `ss_size` by 16 in `sigaltstack` configuration

### Phase 6: MSR TPIDR_EL0 Interception (Completed)
- Intercept `MSR TPIDR_EL0, Xn` instructions in the rewriter (unprivileged on ARM64, guest can freely change TPIDR_EL0)
- Generated MSR trampolines that: save registers, perform the actual MSR, scan the host TLS table for the old entry, update the entry's `guest_tpidr` to the new value
- Two trampoline variants: direct (within ±128MB) and indirect (far targets using ADRP+ADD+BR)
- Fixed rtld_audit `do_syscall` to use the per-thread TLS table instead of the old single-slot layout
- Node.js (with 12+ shared libraries) now runs successfully inside the sandbox

---

## Technical Architecture

### Trampoline Section Layout

```
Trampoline Section (mapped at high address, e.g., 0x8f7000):
┌─────────────────────────────────────────────────────────────┐
│ Header (24 bytes)                                           │
│ ├── Offset 0-7:   "LITEBOX0" magic                         │
│ ├── Offset 8-15:  Handler address (syscall_callback)       │
│ └── Offset 16-23: Pointer to per-thread host TLS table     │
├─────────────────────────────────────────────────────────────┤
│ Entry 0 (for first SVC/MSR, ~32-64 bytes)                  │
├─────────────────────────────────────────────────────────────┤
│ Entry 1 (for second SVC/MSR, ~32-64 bytes)                 │
├─────────────────────────────────────────────────────────────┤
│ ... more entries ...                                        │
└─────────────────────────────────────────────────────────────┘
```

### Per-SVC Trampoline Entry Sequence

```asm
; 1. Reserve stack space for saving registers
SUB SP, SP, #32          ; SP -= 32 (16-byte aligned)

; 2. Save guest registers that trampoline will clobber
STR X16, [SP, #0]        ; Save guest x16
STR X17, [SP, #8]        ; Save guest x17
STR X30, [SP, #16]       ; Save guest x30

; 3. Recover host TLS via per-thread table lookup
MRS X18, TPIDR_EL0       ; X18 = current guest TPIDR_EL0
LDR X17, [PC, #offset]   ; X17 = table pointer from header offset 16
; Loop: scan table for entry where table[i].guest_tpidr == X18
;   X16 = table[i].guest_tpidr
;   CMN X16, #1           ; Check for sentinel (0xFFFFFFFFFFFFFFFF)
;   B.EQ done             ; End of table
;   CMP X16, X18
;   B.EQ found
;   ADD X17, X17, #16     ; Next entry
;   B loop
; found:
;   LDR X18, [X17, #8]    ; X18 = host_tls

; 4. Set return address (where to resume after syscall)
ADR X30, return_addr     ; X30 = instruction after original SVC
; (or ADRP+ADD for far addresses)

; 5. Load syscall handler address
LDR X16, [PC, #offset]   ; X16 = syscall_callback from header offset 8

; 6. Jump to handler
BR X16                   ; Begin syscall processing
```

### Per-MSR TPIDR_EL0 Trampoline Entry Sequence

```asm
; 1. Reserve stack space
SUB SP, SP, #32

; 2. Save registers
STR X16, [SP, #0]        ; Save x16
STR X17, [SP, #8]        ; Save x17
STR X18, [SP, #16]       ; Save x18 (will hold new tpidr)

; 3. Read old TPIDR_EL0 and get new value
MRS X16, TPIDR_EL0       ; X16 = old TPIDR_EL0
MOV X18, Xn              ; X18 = new TPIDR value from source register
;   (special handling if Xn is x16/x17/x18 — read from stack)

; 4. Perform the actual MSR
MSR TPIDR_EL0, X18       ; Set new TPIDR_EL0

; 5. Scan host TLS table for old entry and update guest_tpidr
LDR X17, [PC, #offset]   ; X17 = table pointer from header offset 16
; Loop: find entry where table[i].guest_tpidr == X16 (old value)
;   CMN tmp, #1           ; Sentinel check
;   B.EQ skip_update      ; Not found (pre-registration MSR)
;   STR X18, [X17, #0]    ; Update entry's guest_tpidr to new value
; skip_update:

; 6. Restore registers and return
LDR X18, [SP, #16]
LDR X17, [SP, #8]
LDR X16, [SP, #0]
ADD SP, SP, #32
B return_addr             ; Branch back to instruction after original MSR
```

### Stack Layout During Syscall

When `syscall_callback` is entered, the guest stack has:
```
┌─────────────────────────────────────────┐
│ [SP+0]:  Saved guest X16               │
│ [SP+8]:  Saved guest X17               │
│ [SP+16]: Saved guest X30               │
│ [SP+24]: (unused, alignment padding)   │
├─────────────────────────────────────────┤
│ [SP+32]: Original guest stack...       │
│          (argc, argv, envp for _start) │
└─────────────────────────────────────────┘
```

`syscall_callback` computes `original_guest_sp = SP + 32` and stores this in `ctx.sp`.

### Register Usage

| Register | Purpose in Trampoline | Purpose in syscall_callback |
|----------|----------------------|---------------------------|
| x0-x7    | Syscall arguments    | Preserved for syscall handling |
| x8       | Syscall number       | Preserved |
| x9-x15   | Guest values         | x9, x10 used as scratch |
| x16      | Scratch, then handler addr | Loaded from stack [SP+0] |
| x17      | Scratch              | Loaded from stack [SP+8] |
| x18      | Host TLS (from table) | Host TLS for all operations |
| x19-x29  | Guest callee-saved   | Preserved |
| x30      | Scratch (return addr)| Loaded from stack [SP+16] |
| SP       | Decremented by 32    | Original SP = current SP + 32 |

### Per-Thread Host TLS Table

The host TLS table replaces the original racy single-slot design at `trampoline_base+16`. A pointer to the table is stored at `trampoline_base+16` and the table itself is allocated via mmap (4096 bytes).

**Entry format:**

| Offset | Size | Content |
|--------|------|---------|
| 0 | 8B | `guest_tpidr` — current TPIDR_EL0 value for this thread |
| 8 | 8B | `host_tls` — host's TPIDR_EL0 value |

- 256 entries max (256 * 16 = 4096 bytes)
- Empty/free slots: `guest_tpidr = 0xFFFFFFFFFFFFFFFF` (sentinel `HOST_TLS_TABLE_EMPTY`)
- On thread creation, `update_host_tls_table()` claims a free slot or updates existing
- The SVC trampoline scans the table at every syscall to recover host TLS
- The MSR trampoline scans the table to update `guest_tpidr` when the guest changes TPIDR_EL0

### Alt-Stack Layout (Per Thread)

Signal handlers need host TLS recovery without access to the trampoline header (since the guest may have changed TPIDR_EL0). Each thread gets a power-of-2 aligned alt-stack:

```
[guard (0x1000)] [usable signal stack (ss_size = SIZE-16)] [host_tls (8B)] [pad (8B)]
^                                                          ^               ^
base                                                       base+SIZE-8     base+SIZE
```

- `ALT_STACK_ALLOC_SIZE = 0x10000` (64KB), power-of-2 aligned
- Host TLS stored at `aligned_base + ALT_STACK_ALLOC_SIZE - 8`
- Signal handler recovers host TLS by masking SP: `SP & ~(ALT_STACK_ALLOC_SIZE - 1) + ALT_STACK_ALLOC_SIZE - 8`
- `ss_size` is reduced by 16 to prevent signal frame from clobbering the host TLS slot

### MSR TPIDR_EL0 Instruction Encoding

ARM64 allows unprivileged access to TPIDR_EL0, meaning the guest C runtime can freely change it (e.g., during TLS setup). The rewriter intercepts these instructions.

- `MSR TPIDR_EL0, Xt`: `0xD51BD040 | Rt` (mask `0xFFFFFFE0`)
- `MRS Xt, TPIDR_EL0`: `0xD53BD040 | Rt`
- Source register: `opcode & 0x1F`

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

### Bug 8: Initial Stack Corruption - argc Overwritten (FIXED)

**Symptoms**:
- Guest crashes with SIGSEGV at PC=0x400b98 (inside `__libc_start_main`)
- Fault address is garbage: 0x1000081ae2ed0
- Crash occurs before any syscall is made

**Root Cause**:
The original design had `switch_to_guest` subtract 32 bytes from SP before entering guest code,
storing host TLS at `[SP+0]`. This worked for subsequent syscalls but corrupted the initial
stack layout.

On Linux, when a process starts, the stack looks like:
```
[SP+0]:  argc
[SP+8]:  argv[0]
[SP+16]: argv[1]
...
```

By subtracting 32 and storing host TLS at `[SP+0]`, we overwrote `argc` with a pointer.
When `_start` executed `ldr x1, [sp]` to load argc, it got the host TLS pointer instead,
causing downstream code to crash when dereferencing this garbage value.

**Solution**:
Changed the design so the **trampoline** reserves stack space, not `switch_to_guest`:

1. **Trampoline now does**:
   ```asm
   SUB SP, SP, #32          ; Reserve space
   STR X16, [SP, #0]        ; Save x16
   STR X17, [SP, #8]        ; Save x17
   STR X30, [SP, #16]       ; Save x30
   LDR X18, [PC, #offset]   ; Load host TLS from header (not stack!)
   ```

2. **switch_to_guest now does**:
   ```asm
   ; Just store host TLS at header offset 16 for trampoline to read
   str x18, [x11, #16]      ; trampoline_base+16 = host TLS
   ; Do NOT modify SP
   mov sp, x1               ; SP = ctx.sp (unmodified)
   ```

3. **syscall_callback offsets updated**:
   ```asm
   ldr x11, [sp, #0]        ; Load guest x16 (was [SP+8])
   ldr x12, [sp, #8]        ; Load guest x17 (was [SP+16])
   ldr x13, [sp, #16]       ; Load guest x30 (was [SP+24])
   ```

**Files Changed**:
- `litebox_syscall_rewriter_arm64/src/lib.rs` - Added SUB SP, load TLS from header
- `litebox_platform_linux_userland/src/lib.rs` - Removed SP modification from switch_to_guest

**Verification**:
- Hello world test runs successfully
- Initial stack (argc, argv) preserved correctly
- All rewriter tests pass

---

### Bug 9: ARM64 FileStat Struct Layout Mismatch (FIXED)

**Symptoms**: `newfstatat` syscall corrupted the guest stack, causing crashes after stat-related calls.

**Root Cause**: The `FileStat` struct used the x86_64 layout (144 bytes) instead of the ARM64 layout (128 bytes). The ARM64 kernel writes 128 bytes but our struct had 144-byte size, causing incorrect field alignment and stack corruption when the struct was stack-allocated.

**Solution**: Added a separate ARM64 `FileStat` definition with the correct field layout matching `struct stat` from `arch/arm64/include/asm/stat.h`.

**File**: `litebox_common_linux/src/lib.rs`

---

### Bug 10: Trampoline Address Not Propagated for Dynamically Linked Binaries (FIXED)

**Symptoms**: Dynamically linked binaries crashed immediately — the main binary had no trampoline section and no fallback to the interpreter's trampoline.

**Root Cause**: For dynamically linked ELFs, `SVC #0` instructions exist in the interpreter (ld.so) and shared libraries, not the main binary. The main binary is loaded with `--allow-no-syscalls` (no trampoline), but its `trampoline_addr` was never set from the interpreter's trampoline.

**Solution**: After loading the interpreter, propagate its `trampoline_addr` back to the main binary's load result so the runner has a valid trampoline base for `switch_to_guest`.

**File**: `litebox_shim_linux/src/loader/elf.rs` (~line 268-282)

---

### Bug 11: Absolute Address Encoding in Trampoline for ET_DYN Binaries (FIXED)

**Symptoms**: Shared libraries (ET_DYN) loaded at runtime addresses far from the trampoline caused incorrect branch targets — the trampoline used unrelocated absolute addresses.

**Root Cause**: When the trampoline is >1MB from the code being rewritten, `ADR` (±1MB range) fails. The fallback used `encode_mov_imm64()` which loads absolute (link-time) addresses. For position-independent code (ET_DYN), these addresses are wrong at runtime since the binary is loaded at a dynamic base.

**Solution**: Replaced `encode_mov_imm64()` with PC-relative `ADRP+ADD` sequences (±4GB range) in all 4 locations where return addresses are encoded. Added `encode_adrp()` function to the encoder module.

**File**: `litebox_syscall_rewriter_arm64/src/lib.rs`

---

### Bug 12: `ls` Crashes with SIGABRT on `O_DIRECT | O_NDELAY` (FIXED)

**Symptoms**: Running `ls` inside the sandbox caused `SIGABRT` (via `unimplemented!()` panic).

**Root Cause**: `ls` opens files with `O_DIRECT | O_NDELAY` flags. These flags were not included in the supported `OFlags` set in any of the three filesystem implementations, hitting the `unimplemented!()` fallback.

**Solution**: Added `O_DIRECT` and `O_NDELAY` to the supported OFlags in all three filesystem backends. These flags are accepted and silently ignored (appropriate for an in-memory/tar filesystem).

**Files**:
- `litebox/src/fs/layered.rs`
- `litebox/src/fs/in_mem.rs`
- `litebox/src/fs/tar_ro.rs`

---

### Bug 13: Missing `faccessat` Syscall (FIXED)

**Symptoms**: `ls` and other dynamically linked programs call `faccessat` (syscall 48 on ARM64) during startup. Without a handler, the syscall returned `-ENOSYS`, causing runtime failures.

**Root Cause**: `faccessat` was not implemented. On ARM64, there is no `access` syscall — everything goes through `faccessat`.

**Solution**: Implemented full `faccessat` support:
1. Added `Faccessat` variant to `SyscallRequest` enum with dirfd, path, mode parsing
2. Added dispatch in the shim
3. Handler uses `FsPath::new()` for dirfd resolution, then delegates to existing `sys_access()` logic

**Files**:
- `litebox_common_linux/src/lib.rs` — enum variant + parsing
- `litebox_shim_linux/src/lib.rs` — dispatch
- `litebox_shim_linux/src/syscalls/file.rs` — `sys_faccessat()` handler

---

### Bug 14: Missing `statfs` Syscall (FIXED)

**Symptoms**: Programs calling `statfs` (e.g., to check filesystem type) received `-ENOSYS`.

**Root Cause**: `statfs` was listed in the silenced syscall list (returning `-ENOSYS` silently) but never actually implemented.

**Solution**: Implemented `statfs` returning tmpfs-like values:
- `f_type = 0x01021994` (TMPFS_MAGIC)
- `f_bsize = 4096`
- `f_blocks/f_bfree/f_bavail` = large values (1M blocks)
- Added `StatFs` struct with compile-time size assertions (120 bytes on 64-bit, 64 bytes on 32-bit)

**Files**:
- `litebox_common_linux/src/lib.rs` — `StatFs` struct + parsing
- `litebox_shim_linux/src/lib.rs` — dispatch
- `litebox_shim_linux/src/syscalls/file.rs` — `sys_statfs()` handler

---

### Bug 15: Missing `statx` Syscall (FIXED)

**Symptoms**: Modern glibc uses `statx` instead of `fstatat`/`newfstatat`. Programs calling `statx` received `-ENOSYS`, breaking file metadata queries.

**Root Cause**: `statx` was listed in the silenced syscall list but never implemented.

**Solution**: Full `statx` implementation:
- Added `Statx` struct (256 bytes) and `StatxTimestamp` struct matching kernel layout
- Added `statx_mask` module with constants (`STATX_TYPE`, `STATX_MODE`, `STATX_SIZE`, etc.)
- Handler converts existing `FileStatus` to `Statx`, populating fields based on the requested mask
- Handles `FsPath::Absolute`, `CwdRelative`, `Cwd`, and `Fd` path variants

**Files**:
- `litebox_common_linux/src/lib.rs` — structs, constants, parsing
- `litebox_shim_linux/src/lib.rs` — dispatch
- `litebox_shim_linux/src/syscalls/file.rs` — `sys_statx()` handler

---

### Bug 16: Test Hardcoded Debian libc Path (FIXED)

**Symptoms**: `test_runner_with_ls` failed on non-Debian systems (e.g., Arch Linux) because it hardcoded `/lib/aarch64-linux-gnu` as the libc directory.

**Root Cause**: The test assumed Debian's multiarch directory layout. On Arch-based systems, libc lives at `/usr/lib/libc.so.6`.

**Solution**: Changed the test to dynamically discover the libc directory from the dependency resolution results instead of hardcoding a path.

**File**: `litebox_runner_linux_arm64_userland/tests/run.rs`

---

### Bug 17: Missing `TASK_ADDR_MAX` for aarch64 in Tests (FIXED)

**Symptoms**: `cargo test -p litebox` failed to compile on aarch64 — the `TASK_ADDR_MAX` constant was only defined for x86 and x86_64.

**Root Cause**: The memory management test mock defined `TASK_ADDR_MAX` for x86 and x86_64 but not aarch64.

**Solution**: Added `#[cfg(all(target_arch = "aarch64", target_os = "linux"))] const TASK_ADDR_MAX: usize = 0x0000_FFFF_FFFF_F000;`

**File**: `litebox/src/mm/tests.rs`

---

### Bug 18: Missing SETTLS for ARM64 New Threads (FIXED)

**Symptoms**: Threads created via `clone` with `CLONE_SETTLS` flag crashed immediately because their TPIDR_EL0 was not initialized.

**Root Cause**: The `clone` syscall handler set TPIDR_EL0 for x86/x86_64 but was missing the ARM64 case. On ARM64, TPIDR_EL0 must be set from the `newtls` argument passed to clone.

**Solution**: Added ARM64 SETTLS handling in the clone syscall path using inline assembly `MSR TPIDR_EL0, <newtls>`.

**File**: `litebox_shim_linux/src/syscalls/process.rs`

---

### Bug 19: Broken `set_guest_tpidr`/`get_guest_tpidr` (FIXED)

**Symptoms**: TLS accessors returned garbage values.

**Root Cause**: The functions used incorrect pointer arithmetic to access a thread-local variable. The compiler-generated code for accessing `#[thread_local]` statics on ARM64 requires specific addressing modes.

**Solution**: Replaced pointer arithmetic with inline assembly using `:tprel_lo12:` relocations to correctly access the thread-local storage slot.

**File**: `litebox_platform_linux_userland/src/lib.rs`

---

### Bug 20: Racy Single-Slot Host TLS at `trampoline_base+16` (FIXED)

**Symptoms**: Multi-threaded programs (like Node.js) crashed because multiple threads overwrote each other's host TLS value at the single shared slot.

**Root Cause**: `switch_to_guest` wrote the host TLS to `trampoline_base+16` before entering guest code. With multiple threads, each thread's write clobbered the previous thread's value.

**Solution**: Replaced the single slot with a per-thread host TLS table:
1. Allocated a 4KB mmap'd table (256 entries × 16 bytes)
2. Store a pointer to the table at `trampoline_base+16` instead of host TLS directly
3. Each thread registers its `(guest_tpidr, host_tls)` pair in the table
4. SVC trampoline scans the table using current TPIDR_EL0 to find matching host TLS
5. Sentinel value `0xFFFFFFFFFFFFFFFF` marks empty slots

**Files**:
- `litebox_platform_linux_userland/src/lib.rs` — table allocation, `update_host_tls_table()`, `switch_to_guest` modifications
- `litebox_syscall_rewriter_arm64/src/lib.rs` — SVC trampoline rewritten with table lookup loop

---

### Bug 21: Signal Handler Cannot Recover Host TLS (FIXED)

**Symptoms**: Signal handlers (e.g., SIGSEGV for guard page handling) crashed because they couldn't find the host TLS value — TPIDR_EL0 contained the guest's value.

**Root Cause**: Signal handlers run on a separate alt-stack and need host TLS to call into the shim. With the per-thread table, they'd need to scan it, but the table pointer itself requires host TLS to locate.

**Solution**: Power-of-2 aligned alt-stacks with host TLS embedded at a known offset:
1. Allocate alt-stacks at `ALT_STACK_ALLOC_SIZE` (64KB) alignment
2. Store host TLS at `aligned_base + ALT_STACK_ALLOC_SIZE - 8`
3. Signal handler recovers host TLS by masking SP to find the alt-stack base
4. Reduced `ss_size` by 16 bytes to prevent signal frame growth from overwriting the host TLS slot

**File**: `litebox_platform_linux_userland/src/lib.rs`

---

### Bug 22: MSR TPIDR_EL0 Desynchronizes Host TLS Table (FIXED)

**Symptoms**: After the guest C runtime sets up TLS (via `MSR TPIDR_EL0`), subsequent SVC trampolines read the new TPIDR_EL0, can't find it in the host TLS table (which has the old value), and fail to recover host TLS.

**Root Cause**: `MSR TPIDR_EL0, Xn` is unprivileged on ARM64. The guest freely changes TPIDR_EL0 during C runtime initialization, but the host TLS table entry still has the old `guest_tpidr` value.

**Solution**: Intercept `MSR TPIDR_EL0, Xn` instructions in the rewriter, similar to how `SVC #0` is intercepted:
1. `is_msr_tpidr_el0()` detection: checks `(u32 & 0xFFFFFFE0) == 0xD51BD040`
2. `generate_msr_trampoline_direct()`: for near targets (within ±128MB)
3. `generate_msr_trampoline_indirect()`: for far targets (uses ADRP+ADD+BR)
4. The trampoline performs the MSR, then scans the host TLS table for the old entry and updates `guest_tpidr` to the new value
5. Sentinel check (`CMN X18, #1`) handles the case where MSR executes before the platform registers the thread (graceful skip)

**File**: `litebox_syscall_rewriter_arm64/src/lib.rs`

---

### Bug 23: rtld_audit `do_syscall` Using Old Single-Slot Layout (FIXED)

**Symptoms**: Shared library syscalls crashed after the per-thread table refactor.

**Root Cause**: The `do_syscall()` function in `rtld_audit_arm64.c` was reading `trampoline_data+16` as host TLS directly. After the refactor, offset 16 contains a table pointer, not host TLS.

**Solution**: Updated `do_syscall` to:
1. Read the table pointer from `trampoline_data+16`
2. Read current `tpidr_el0` (guest_tpidr) via MRS
3. Scan the table for matching entry (up to 256 entries, sentinel = `0xFFFFFFFFFFFFFFFF`)
4. Load `host_tls` from the matched entry

**File**: `litebox_rtld_audit_arm64/rtld_audit_arm64.c`

---

## Code Artifacts

### New Crates Created

```
litebox_syscall_rewriter_arm64/
├── Cargo.toml           # Dependencies: yaxpeax-arm, object, thiserror
├── src/
│   └── lib.rs           # ~1830 lines: decoder, encoder, SVC/MSR trampoline generator
└── tests/
    ├── hello-arm64      # Test binary
    ├── snapshots/       # Insta snapshots
    └── snapshot_tests.rs

litebox_rtld_audit_arm64/
├── Cargo.toml
├── rtld_audit_arm64.c   # ~540 lines: LD_AUDIT shared library with per-thread TLS table lookup
└── litebox_rtld_audit_arm64.so  # Pre-compiled shared library
```

### Key Functions in litebox_syscall_rewriter_arm64

| Function | Purpose |
|----------|---------|
| `rewrite_syscalls()` | Main entry point, processes ELF |
| `decode_section()` | Finds all SVC and MSR TPIDR_EL0 instructions |
| `generate_trampoline_direct()` | Creates SVC trampoline for near branches (±128MB) |
| `generate_trampoline_indirect()` | Creates SVC trampoline for far branches |
| `generate_msr_trampoline_direct()` | Creates MSR TPIDR_EL0 trampoline for near branches |
| `generate_msr_trampoline_indirect()` | Creates MSR TPIDR_EL0 trampoline for far branches |
| `is_msr_tpidr_el0()` | Detects MSR TPIDR_EL0 instructions |
| `msr_tpidr_el0_source_reg()` | Extracts source register from MSR encoding |

### Encoder Functions (`mod encoder`)

| Function | Purpose |
|----------|---------|
| `encode_b()` | B (branch) instruction |
| `encode_bl()` | BL (branch with link) instruction |
| `encode_br()` | BR (branch register) instruction |
| `encode_ret()` | RET instruction |
| `encode_ldr_literal()` | LDR with PC-relative offset |
| `encode_ldr_imm()` | LDR with immediate offset |
| `encode_ldr_pre()` | LDR with pre-index (e.g., `LDR Xt, [SP], #imm`) |
| `encode_str_imm()` | STR with immediate offset |
| `encode_adr()` | ADR instruction (±1MB) |
| `encode_adrp()` | ADRP instruction (±4GB, page-aligned) |
| `encode_nop()` | NOP instruction |
| `encode_movz()` | MOVZ (move wide with zero) |
| `encode_movk()` | MOVK (move wide with keep) |
| `encode_mov_imm64()` | 64-bit immediate load (MOVZ+MOVK×3) |
| `encode_stp_pre()` | STP with pre-index |
| `encode_ldp_post()` | LDP with post-index |
| `encode_mov_reg()` | MOV (register to register) |
| `encode_sub_imm()` | SUB with immediate |
| `encode_add_imm()` | ADD with immediate |
| `encode_mrs_tpidr_el0()` | MRS Xt, TPIDR_EL0 |
| `encode_msr_tpidr_el0()` | MSR TPIDR_EL0, Xt |
| `encode_cmp_reg()` | CMP (register compare) |
| `encode_cmn_imm()` | CMN Xn, #imm (compare negative) |
| `encode_b_cond()` | B.cond (conditional branch) |

### Modified Platform Files

| File | Changes |
|------|---------|
| `litebox_platform_linux_userland/src/lib.rs` | `switch_to_guest` with per-thread TLS table update; `syscall_callback` loads saved regs from stack; signal handler with alt-stack TLS recovery; `update_host_tls_table()` for thread registration |
| `litebox_shim_linux/src/lib.rs` | ARM64 syscall return value (ctx.regs[0]) |
| `litebox_shim_linux/src/syscalls/process.rs` | ARM64 SETTLS for clone/new threads |
| `litebox_common_linux/src/loader.rs` | Trampoline section loading and mapping |
| `litebox_rtld_audit_arm64/rtld_audit_arm64.c` | `do_syscall` with per-thread TLS table lookup; shared library trampoline patching with table pointer propagation |

---

## Lessons Learned

### 1. ARM64 Calling Convention Complexity
- x16/x17 are "intra-procedure-call scratch registers" - can be clobbered by PLT stubs
- x18 is the "platform register" - used for TLS on Linux
- x30 is the link register - must be preserved carefully

### 2. Trampoline Design Challenges
- Can't assume any register is available as scratch
- Must save registers BEFORE using them, but need a register to compute where to save
- Solution: Use PC-relative addressing to load host TLS from trampoline header

### 3. Switch_to_guest Must Use PC, Not LR
- ARM64 `ret` jumps to x30 (LR), not to a "return address" from stack
- Must load ctx.pc into a register and use `br` to jump

### 4. Stack Balance is Critical
- Any SP modification in trampoline must be perfectly balanced
- Even 16-byte misalignment can corrupt caller's saved frame

### 5. Don't Modify Guest Stack Before First Syscall
- Initial guest stack has argc, argv, envp - modifying it corrupts process startup
- Let the trampoline reserve stack space when syscall occurs, not on initial entry

### 6. Debug Incrementally
- Each bug fix revealed the next bug
- Coredump analysis with GDB was essential
- Adding verification assertions caught issues early
- **Hardcoded instruction encodings should always be verified with an assembler**

### 7. Multi-Threading Requires Careful TLS Management
- A single shared TLS slot is racy with multiple threads
- Per-thread tables with sentinel-terminated scanning is robust
- Signal handlers need an independent path to recover host TLS (alt-stack embedding)

### 8. Guest Can Change TPIDR_EL0 Freely
- `MSR TPIDR_EL0, Xn` is unprivileged on ARM64, unlike x86's `WRGSBASE`
- Must intercept these instructions to keep the host TLS table synchronized
- The MSR may execute before the platform registers the thread — sentinel checks prevent infinite loops

---

## Future Work

### Short-term (Cleanup)
1. Remove temporary debug `eprintln!` logging from `litebox_platform_linux_userland/src/lib.rs` (7 statements with `[DEBUG]` prefix)
2. Fix `dead_code` warning on `backend` field in `litebox_runner_linux_arm64_userland/tests/run.rs`
3. Run systrap tests for regression (`test_static_exec_with_systrap`, `test_dynamic_lib_with_systrap`)

### Short-term (Features)
1. Implement `faccessat2` (syscall 439) — only `faccessat` (syscall 48) is currently supported
2. Respect `statx` flags parameter (currently `_flags` is ignored — could honor `AT_SYMLINK_NOFOLLOW`)

### Medium-term
1. Fix seccomp backend timing issues
2. Handle SVE/NEON context in signals
3. Implement remaining silenced syscalls as needed
4. Optimize host TLS table lookup (consider hash-based or direct-indexed approach for many threads)

### Long-term
1. Performance optimization
2. Consider hotpatching approach (like x86)

---

## Testing Commands

```bash
# Build
cargo build -p litebox_runner_linux_arm64_userland

# Run rewriter tests
cargo test -p litebox_runner_linux_arm64_userland test_static_exec_with_rewriter -- --nocapture
cargo test -p litebox_runner_linux_arm64_userland test_dynamic_lib_with_rewriter -- --nocapture
cargo test -p litebox_runner_linux_arm64_userland test_node_with_rewriter -- --nocapture

# Run systrap tests
cargo test -p litebox_runner_linux_arm64_userland test_static_exec_with_systrap -- --nocapture
cargo test -p litebox_runner_linux_arm64_userland test_dynamic_lib_with_systrap -- --nocapture

# Run clippy (zero warnings required, no #[allow(...)] attributes)
cargo clippy -p litebox -p litebox_common_linux -p litebox_shim_linux \
  -p litebox_syscall_rewriter_arm64 -p litebox_runner_linux_arm64_userland \
  -p litebox_platform_linux_userland

# Clean rewriter caches (MUST do after any rewriter or rtld_audit changes)
rm -f target/debug/build/litebox_runner_linux_arm64_userland-*/out/*.hooked
rm -f target/debug/build/litebox_runner_linux_arm64_userland-*/out/*.tar
rm -f target/debug/build/litebox_runner_linux_arm64_userland-*/out/*.cache-checksum
rm -rf target/debug/build/litebox_runner_linux_arm64_userland-*/out/tar_files_*

# Run with rewriter backend (manual)
./target/debug/litebox_runner_linux_arm64_userland \
    --unstable \
    --interception-backend rewriter \
    --rewrite-syscalls \
    --initial-files /tmp/test_static.tar \
    /tmp/test_static

# Analyze coredump
coredumpctl debug -1 -A "-batch -ex 'bt' -ex 'info registers'"
```

---

## Current Status

### All Rewriter Tests Passing
- `test_static_exec_with_rewriter` — static binary (hello world)
- `test_dynamic_lib_with_rewriter` — dynamically linked binary
- `test_node_with_rewriter` — Node.js with 12+ shared libraries

### Clippy Clean
Zero warnings across all packages.

### Uncommitted Changes
5 files modified (+1,179 / -196 lines), all unstaged:
- `litebox_platform_linux_userland/src/lib.rs` — per-thread TLS table, signal handling, debug logging
- `litebox_rtld_audit_arm64/litebox_rtld_audit_arm64.so` — recompiled shared library
- `litebox_rtld_audit_arm64/rtld_audit_arm64.c` — table lookup in `do_syscall`
- `litebox_shim_linux/src/syscalls/process.rs` — ARM64 SETTLS
- `litebox_syscall_rewriter_arm64/src/lib.rs` — MSR interception, TLS table lookup in trampolines

### Remaining Cleanup
1. Remove 7 debug `eprintln!` statements (search `[DEBUG]` in `litebox_platform_linux_userland/src/lib.rs`)
2. Fix `dead_code` warning on `backend` field in test runner
3. Verify systrap tests still pass

---

## References

- ARM Architecture Reference Manual (ARMv8-A)
- Linux kernel source: `arch/arm64/include/uapi/asm/sigcontext.h`
- yaxpeax-arm crate documentation
- LiteBox x86 syscall rewriter implementation

---

*Last updated: 2026-02-07*
