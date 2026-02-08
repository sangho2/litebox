# ARM64 vs x86_64: Syscall Rewriter Comparison

A side-by-side technical comparison of the two rewriter backends in LiteBox.

**Source files:**

| Component | x86_64 | ARM64 |
|-----------|--------|-------|
| Rewriter | `litebox_syscall_rewriter/src/lib.rs` (662 lines) | `litebox_syscall_rewriter_arm64/src/lib.rs` (~1480 lines) |
| Platform runtime | `litebox_platform_linux_userland/src/lib.rs` (`#[cfg(target_arch = "x86_64")]`) | Same file (`#[cfg(target_arch = "aarch64")]`) |
| Audit library | N/A | `litebox_rtld_audit_arm64/rtld_audit_arm64.c` |

---

## 1. Architecture Fundamentals

| Property | x86_64 | ARM64 (AArch64) |
|----------|--------|-----------------|
| Instruction size | Variable (1-15 bytes) | Fixed (4 bytes) |
| Syscall instruction | `SYSCALL` (2 bytes: `0F 05`) | `SVC #0` (4 bytes: `01 00 00 D4`) |
| TLS registers | Two: `FS` (user), `GS` (kernel/TLS) | One: `TPIDR_EL0` |
| Branch encoding | `JMP rel32` (5 bytes, +/-2 GB) | `B imm26` (4 bytes, +/-128 MB) |
| Link register | None; `CALL` pushes return addr to stack | `X30` (LR) |
| Syscall ABI return addr | `RCX` (hardware, set by `SYSCALL`) | `X30` / `ELR_EL1` |
| Scratch registers | None guaranteed across `SYSCALL` | `X16`, `X17` (intra-procedure-call scratch) |

### The Core Tradeoff

x86_64's variable-length encoding creates the **space problem**: the `SYSCALL` instruction is only 2 bytes, but a `JMP rel32` replacement needs 5. The rewriter must "borrow" adjacent instructions to make room.

ARM64's fixed 4-byte encoding means `SVC #0` and `B target` are both exactly 4 bytes -- a direct 1:1 replacement is possible when the trampoline is within +/-128 MB. However, ARM64 has the **TLS problem**: with only one TLS register, the rewriter must also intercept `MSR TPIDR_EL0` writes.

---

## 2. What Gets Rewritten

| | x86_64 | ARM64 |
|-|--------|-------|
| Primary target | `SYSCALL` (`0F 05`) | `SVC #0` (`D4000001`) |
| Secondary target | `INT 0x80` (x86_32 compat) | `MSR TPIDR_EL0, Xn` |
| Why secondary? | Legacy 32-bit syscall path | Must track guest TLS writes to maintain host/guest TLS mapping |

x86_64 has separate `FS`/`GS` segment base registers, so TLS is not a concern -- the host uses `GS` and the guest uses `FS` (or vice versa), and they don't interfere. ARM64 shares a single `TPIDR_EL0` between host and guest, requiring an interception mechanism.

---

## 3. Rewriter: Instruction Scanning

### x86_64

Uses the `iced-x86` crate -- a full x86 decoder:

```rust
let mut decoder = iced_x86::Decoder::new(64, section_data, DecoderOptions::NONE);
decoder.set_ip(section_base_addr);
let instructions = decoder.iter().collect::<Vec<_>>();
for (i, inst) in instructions.iter().enumerate() {
    if inst.code() != iced_x86::Code::Syscall { continue; }
    // ...
}
```

The decoder provides `flow_control()` and full operand analysis, making it straightforward to identify control flow boundaries.

### ARM64

Uses `yaxpeax-arm` for decoding plus custom bit-pattern checks for specific instructions:

```rust
fn is_svc(&self) -> bool {
    (u32::from_le_bytes(self.bytes) & 0xFFE0001F) == 0xD4000001
}
fn is_msr_tpidr_el0(&self) -> bool {
    (u32::from_le_bytes(self.bytes) & 0xFFFFFFE0) == 0xD51BD040
}
```

The fixed-width encoding makes bit-pattern matching reliable without full decoding. Branch targets are extracted by decoding the immediate fields of `B`, `BL`, `B.cond`, `CBZ`, `CBNZ`, `TBZ`, `TBNZ` instructions.

---

## 4. Patch-Site Strategies

### x86_64: Borrow Before / After / Both

The core challenge: `SYSCALL` = 2 bytes, `JMP rel32` = 5 bytes. Three strategies:

**Borrow-before (primary):** Walk backwards from `SYSCALL` to find enough preceding instructions to reach 5+ bytes total, without crossing branch targets or control-flow instructions.

```
Original:                      Patched:
  mov eax, 1        (5B)        jmp trampoline     (5B)
  syscall            (2B)        nop; nop            (2B)

Trampoline:
  mov eax, 1                   ; copied displaced instruction
  lea rcx, [rip+disp]         ; return addr -> RCX (mimics SYSCALL ABI)
  jmp [rip+disp]              ; indirect jump to handler (via header[8])
```

**Borrow-after (fallback):** If there isn't enough space before (e.g., the `SYSCALL` is at a branch target), borrow instructions after it.

**Borrow-both (x86_32 only):** Last resort when neither direction alone provides 5 bytes. Only needed for `INT 0x80` (also 2 bytes) in 32-bit code with tightly packed instructions.

### ARM64: Direct / Indirect

**Direct (primary):** When the trampoline section is within +/-128 MB of the instruction, replace the 4-byte instruction with a 4-byte `B` (unconditional branch).

```
Original:              Patched:
  svc #0    (4B)        b trampoline    (4B)
```

No borrowing needed. The replacement is a perfect 1:1 swap.

**Indirect (fallback):** When the trampoline is beyond +/-128 MB, use a 3-instruction sequence (12 bytes). This requires borrowing adjacent instructions, similar to x86_64:

```
Original:                          Patched:
  mov x1, x0          (4B)          adrp x16, trampoline@page    (4B)
  svc #0              (4B)          add  x16, x16, #page_off     (4B)
  mov x2, x0          (4B)          br   x16                     (4B)

Trampoline:
  mov x1, x0                      ; copied displaced instruction(s)
  <svc trampoline body>           ; save regs, TLS lookup, jump to handler
  <jump back>                     ; return to instruction after patch site
```

The `ADRP + ADD + BR` sequence provides full 64-bit address reach via page-relative addressing.

### Strategy Comparison

| | x86_64 | ARM64 |
|-|--------|-------|
| Minimum patch size | 5 bytes (`JMP rel32`) | 4 bytes (`B imm26`) |
| Direct replacement possible? | Never (2B < 5B) | Yes, when within +/-128 MB |
| Indirect sequence | N/A (always borrows) | `ADRP + ADD + BR` = 12 bytes |
| Always borrows neighbors? | Yes | Only for indirect case |
| Branch range | +/-2 GB (`rel32`) | +/-128 MB (direct), unlimited (indirect) |

---

## 5. Branch Target Safety

Both architectures build a `HashSet<u64>` of all branch targets in the section before patching. Neither will split an instruction sequence that straddles a branch target, since doing so would corrupt the branch destination.

```rust
// Identical logic on both sides:
fn get_control_transfer_targets(...) -> HashSet<u64> {
    // Decode all instructions, collect targets of B/BL/CBZ/CBNZ/TBZ/TBNZ etc.
}
```

The x86_64 version uses `iced-x86`'s `near_branch_target()`. The ARM64 version manually extracts immediates from branch instruction bit fields.

---

## 6. Trampoline Section Layout

Both rewriters append a `.trampolineLB0` section to the ELF, placed at a page-aligned address above the highest `PT_LOAD` segment.

### x86_64 Header (16 bytes)

```
Offset  Size  Content
  0       8   "LITEBOX0" magic
  8       8   Handler address (set by loader)
 16+          Per-SYSCALL trampoline entries
```

### ARM64 Header (24 bytes + sigreturn trampoline)

```
Offset  Size  Content
  0       8   "LITEBOX0" magic
  8       8   Handler address (set by loader)
 16       8   TLS table pointer (set at runtime)
 24      64   Sigreturn trampoline (16 instructions)
 88+          Per-SVC and per-MSR trampoline entries
```

The ARM64 header is larger because:
1. **TLS table pointer (offset 16):** Points to a 256-entry `(guest_tpidr, host_tls)` lookup table in memory. Each SVC trampoline reads this to find the host TLS for the current thread.
2. **Sigreturn trampoline (offset 24):** On ARM64, glibc does not set `SA_RESTORER` when calling `sigaction()`. The kernel normally provides sigreturn via the vDSO, but since the guest runs inside the sandbox, a custom sigreturn trampoline is needed.

---

## 7. Per-Instruction Trampoline Entries

### x86_64 SVC Entry

```asm
; Displaced instructions (borrowed from before/after SYSCALL)
  <copied instructions>

; Set return address into RCX (mimics hardware SYSCALL behavior,
; which puts RIP-after-SYSCALL into RCX)
  lea rcx, [rip + disp32]       ; RCX = address after original SYSCALL

; Jump to handler (indirect through header[8])
  jmp [rip + disp32]            ; reads handler address from trampoline header
```

The `LEA RCX` is critical: the real x86_64 `SYSCALL` instruction places the return address in `RCX` (and RFLAGS in R11). By mimicking this, the trampoline makes the handler's return path identical whether the syscall was intercepted or not.

### ARM64 SVC Entry (direct case)

```asm
; Save scratch regs (no equivalent needed on x86_64 -- it uses stack-based JMP)
  sub  sp, sp, #32
  str  x16, [sp, #0]
  str  x17, [sp, #8]
  str  x30, [sp, #16]

; Load TLS table pointer from header[16]
  ldr  x16, [pc, #offset_to_header_16]

; Read current thread's TPIDR_EL0
  mrs  x17, tpidr_el0

; Linear scan: find table entry where guest_tpidr == X17
loop:
  ldr  x18, [x16, #0]           ; table[i].guest_tpidr
  cmp  x18, x17
  b.eq found
  add  x16, x16, #16            ; next entry (16 bytes each)
  b    loop

found:
  ldr  x18, [x16, #8]           ; X18 = table[i].host_tls

; Set return address via PC-relative addressing
  adrp x30, return_addr@page
  add  x30, x30, #return_addr@page_off

; Load handler address from header[8], jump to it
  ldr  x16, [pc, #offset_to_header_8]
  br   x16
```

The ARM64 trampoline is significantly larger due to the TLS table lookup. On x86_64, the host TLS is always accessible via `GS`, so no lookup is needed.

### ARM64 MSR TPIDR_EL0 Entry (no x86_64 equivalent)

```asm
; Save scratch regs
  sub  sp, sp, #32
  str  x16, [sp, #0]
  str  x17, [sp, #8]
  str  x18, [sp, #16]

; Read old TPIDR_EL0
  mrs  x16, tpidr_el0

; Get new value (from source register, handling clobbered cases)
  mov  x17, <source_reg>

; Perform the actual MSR
  msr  tpidr_el0, x17

; Load TLS table pointer, scan for old_tpidr
  ldr  x16, [pc, #offset_to_header_16]
loop:
  ldr  x18, [x16, #0]
  cmn  x18, #1                  ; check sentinel (0xFFFFFFFFFFFFFFFF)
  b.eq skip                     ; not found -- early init, skip update
  cmp  x18, x17_old
  b.eq found
  add  x16, x16, #16
  b    loop

found:
  str  new_tpidr, [x16, #0]     ; update table entry with new guest TPIDR

skip:
; Restore registers, return inline (no handler call)
  ldr  x18, [sp, #16]
  ldr  x16, [sp, #0]
  ldr  x17, [sp, #8]
  add  sp, sp, #32
  <jump back to caller>
```

This trampoline does NOT call the syscall handler. It performs the MSR directly and updates the TLS lookup table so that future SVC trampolines on this thread can still find the correct host TLS. The sentinel check (`CMN X18, #1`) handles the case where the table entry hasn't been initialized yet (early thread startup).

---

## 8. Runtime: `syscall_callback`

Both implementations live in `litebox_platform_linux_userland/src/lib.rs` behind `#[cfg(target_arch)]`.

### x86_64

```
Trampoline sets:  RCX = return address (guest PC after SYSCALL)
Entry via:        JMP [RIP+disp] to syscall_callback

syscall_callback:
  1. Clear `in_guest` flag (TLS via GS)
  2. Swap FS base: restore host fsbase, save guest fsbase
  3. Switch to host stack (from GS:host_sp)
  4. Build PtRegs struct on stack (push all GP regs + RCX as RIP)
  5. Call syscall_handler(regs)
  6. → switch_to_guest on return
```

Key: the host TLS (`GS` base) is always accessible, even while running guest code. The `FS`/`GS` swap is a `wrfsbase`/`rdfsbase` pair.

### ARM64

```
Trampoline sets:  X18 = host_tls (from TLS table lookup)
                  X30 = return address
                  X16/X17/X30 saved on stack by trampoline
Entry via:        BR X16 to syscall_callback

syscall_callback:
  1. Use X18 to locate ThreadContext (host_tls - constant offset)
  2. Save all guest registers (X0-X15, X19-X29) to PtRegs
  3. Recover X16/X17/X30 from trampoline's stack frame (rewriter mode)
     OR from current values (systrap mode -- different save convention)
  4. Restore host SP and FP from TLS
  5. Call syscall_handler(regs)
  6. → switch_to_guest on return
```

Key differences from x86_64:
- **No dual-TLS luxury.** X18 carries the host TLS from the trampoline's table lookup. Without it, the callback would have no way to find its own thread-local data.
- **Dual-mode support.** The same `syscall_callback` handles both rewriter mode (X16/X17/X30 on stack) and systrap mode (seccomp-based, different register state). A runtime flag distinguishes them.
- **More registers to save.** ARM64 has 31 GP registers vs x86_64's 16.

---

## 9. Runtime: `switch_to_guest`

### x86_64

```asm
switch_to_guest:
  1. Set in_guest = 1
  2. Check interrupt flag; if set, call interrupt_callback
  3. Restore guest fsbase (wrfsbase)
  4. Pop all GP regs from PtRegs
  5. Store guest RIP in GS:scratch
  6. JMP [GS:scratch]              ; resume guest execution
```

The return to guest code uses an indirect jump through the GS-relative scratch slot, avoiding the need for a dedicated register to hold the guest PC.

### ARM64

```asm
switch_to_guest:
  1. Set in_guest = 1
  2. Check interrupt flag; if set, call interrupt_callback
  3. Restore all guest GP registers (X0-X30 from PtRegs)
  4. Load guest PC into X18 (from PtRegs)
  5. MSR TPIDR_EL0, <guest_tpidr>  ; switch to guest TLS
  6. BR X18                        ; resume guest execution
```

X18 is used as a trampoline register here. It holds the guest PC momentarily during the transition; the guest's actual X18 value has already been restored to the PtRegs struct and will be loaded before the `BR`.

### Comparison

| Aspect | x86_64 | ARM64 |
|--------|--------|-------|
| TLS switch | `wrfsbase` (single instruction) | `MSR TPIDR_EL0` (single instruction) |
| Guest PC dispatch | `JMP [GS:scratch]` (indirect through TLS) | `BR X18` (register direct) |
| Interrupt check | Yes | Yes |
| Clobber concern | None (GS always available) | X18 clobbered temporarily, must be restored before guest resumes |

---

## 10. Dynamic Library Support

### x86_64

The loader patches the handler address into the trampoline header (offset 8) at map time. Since the trampoline uses `JMP [RIP+disp]` to read the handler address, no further fixup is needed.

### ARM64

An `LD_AUDIT` shared library (`litebox_rtld_audit_arm64/rtld_audit_arm64.c`) hooks into the dynamic linker's `la_objopen` callback. When a new shared library is loaded, the audit library:

1. Checks if the ELF has a `.trampolineLB0` section
2. Extracts the trampoline data from the end of the file
3. `mmap`s it at the recorded virtual address
4. Writes the handler address and TLS table pointer into the header

This is necessary because ARM64 Linux's dynamic linker doesn't provide the same hooking mechanisms available on x86_64. The `LD_AUDIT` approach intercepts loads at the earliest possible point.

---

## 11. Signal Handling

### Host TLS Recovery in Signal Handlers

When a signal arrives while executing guest code, the signal handler must recover the host TLS to access thread-local state.

**x86_64:** `GS` base is never clobbered, so the signal handler can immediately access `GS:host_sp`, `GS:in_guest`, etc.

**ARM64:** `TPIDR_EL0` may hold the guest's TLS value. The host TLS is recovered by exploiting the alt-stack layout:

```
Alt-stack (power-of-2 aligned):
  +---------------------------+
  | guard page                |
  +---------------------------+
  | stack space               |
  |                           |
  | ... signal handler runs ...|
  |                           |
  +---------------------------+
  | host_tls (8 bytes)        |  <- top of alt-stack
  +---------------------------+

Recovery: host_tls = *(SP | (stack_size - 8))
```

The power-of-2 alignment lets the handler compute the top of the alt-stack with a single bitwise OR, regardless of the current SP position.

### Sigreturn Trampoline

**x86_64:** Not needed. glibc sets `SA_RESTORER` to point to a sigreturn stub. The stub executes `__NR_rt_sigreturn` directly.

**ARM64:** Required. glibc on ARM64 does not set `SA_RESTORER`. The kernel's vDSO provides `__kernel_rt_sigreturn`, but the guest cannot use it directly (it's in the host address space). The rewriter generates a sigreturn trampoline at trampoline header offset 24, which:

1. Sets `X8 = 139` (`__NR_rt_sigreturn`)
2. Looks up host TLS from the TLS table
3. Jumps to the syscall handler

The shim stores this trampoline address and uses it as the restorer when delivering signals to the guest.

---

## 12. Summary

| Dimension | x86_64 | ARM64 |
|-----------|--------|-------|
| **Instruction to intercept** | `SYSCALL` (2B) | `SVC #0` (4B) + `MSR TPIDR_EL0` (4B) |
| **Replacement instruction** | `JMP rel32` (5B) | `B imm26` (4B) or `ADRP+ADD+BR` (12B) |
| **Always borrows neighbors?** | Yes (2B < 5B) | No (4B = 4B for direct case) |
| **Disassembler** | `iced-x86` (full decoder) | `yaxpeax-arm` + bit patterns |
| **Trampoline header size** | 16 bytes | 24 bytes + 64B sigreturn |
| **TLS architecture** | Dual (FS/GS) -- no conflicts | Single (TPIDR_EL0) -- requires lookup table |
| **TLS lookup in trampoline** | Not needed | Linear scan of 256-entry table |
| **MSR interception** | Not needed | Required (TPIDR_EL0 writes) |
| **Sigreturn trampoline** | Not needed | Required (glibc omits SA_RESTORER) |
| **Signal handler TLS recovery** | Via GS (always available) | Via alt-stack top (bitmask trick) |
| **Dynamic library patching** | Loader hooks | `LD_AUDIT` library |
| **Dual-mode callback** | No (rewriter only) | Yes (rewriter + systrap in same callback) |
| **Rewriter crate size** | ~660 lines | ~1480 lines |
| **Key complexity driver** | Variable-length instruction borrowing | TLS table management |

### Why ARM64 is More Complex

The ARM64 rewriter is roughly 2.2x larger. The additional complexity comes from:

1. **TLS table infrastructure** (~240 lines): the global lookup table, initialization, per-thread updates, sentinel handling, and the TLS scan loop duplicated in every SVC and sigreturn trampoline.
2. **MSR TPIDR_EL0 interception** (~140 lines): trampoline bodies for each source register variant, table update logic, and direct/indirect variants.
3. **Sigreturn trampoline** (~90 lines): an entire subsystem that x86_64 doesn't need.
4. **Custom encoder** (~210 lines): ARM64 has no equivalent to `iced-x86`'s assembler, so all instructions are encoded manually via bit manipulation.

However, ARM64's patch-site logic is simpler. The fixed instruction width eliminates the "borrowing" complexity that dominates the x86_64 rewriter. The direct `B` replacement (1:1 swap with no neighbor displacement) is the common case and requires zero surrounding-instruction analysis.
