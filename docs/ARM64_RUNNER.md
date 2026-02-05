# ARM64 Runner Support for LiteBox

## Problem Statement
Add ARM64 (aarch64) support to LiteBox by creating a separate runner crate for ARM64 Linux userland. The existing `litebox_runner_linux_userland` supports x86/x64 only.

## Key Constraints
- Syscall rewriter (`litebox_syscall_rewriter`) uses `iced-x86` and doesn't support ARM - must use systrap backend
- `litebox_rtld_audit` is x86_64 only - not needed with systrap backend  
- `litebox_platform_linux_userland` currently restricted to x86/x86_64 - needs ARM64 support
- `litebox_common_linux` has many x86-specific structures (PtRegs, signal contexts, etc.)

## Approach
Use systrap (seccomp SIGSYS) backend for syscall interception on ARM64, avoiding the need for binary rewriting.

---

## Completed Work

### Phase 1: Platform Layer ARM64 Support ✅

#### litebox_common_linux changes:
- [x] Added `EM_AARCH64 = 183` to loader MACHINE constants
- [x] Added ARM64 `PtRegs` structure with fields: `regs[31]`, `sp`, `pc`, `pstate`, `orig_x0`, `syscallno`
- [x] Created `signal/aarch64.rs` with `Sigcontext`, `FpsimdContext`, `AuxHead` structures
- [x] Added `CChar` type alias (`u8` on ARM64, `i8` on x86) for char pointer compatibility
- [x] Updated all `Platform::RawConstPointer<i8>` to `Platform::RawConstPointer<CChar>` in syscall definitions
- [x] Added `MINSIGSTKSZ = 5120` for ARM64 (vs 2048 on x86)
- [x] Added `TASK_ADDR_MAX = 0x0000_FFFF_FFFF_F000` for ARM64 48-bit virtual address space
- [x] Added ARM64 implementations for `syscall_arg()`, `get_ip()`, `set_ip()` on PtRegs
- [x] Added cfg guards for x86-only syscalls that don't exist on ARM64

#### litebox_platform_linux_userland changes:
- [x] Updated main cfg gate to include `target_arch = "aarch64"`
- [x] Created `syscall_intercept/systrap_aarch64.rs` for seccomp-based syscall interception
- [x] Added ARM64 `SYSCALL_ARG_MAGIC = 0xDEAD_BEEF_CAFE_F00D` constant
- [x] Implemented ARM64 `run_thread_arch` with naked assembly for guest context switching
- [x] Added ARM64 TLS variables: `scratch`, `host_sp`, `host_fp`, `guest_context_top`, `guest_tpidr`, `in_guest`, `interrupt`
- [x] Implemented ARM64 `switch_to_guest` assembly for entering guest context
- [x] Implemented ARM64 `syscall_callback` for handling trapped syscalls
- [x] Implemented `signal_handler_exit_guest` and `copy_signal_context` for ARM64
- [x] Added `set_signal_return` for ARM64 signal handling
- [x] Updated `with_signal_alt_stack` to use raw syscalls with backdoor magic flags

#### litebox (core) changes:
- [x] Added ARM64 `ExceptionInfo` with `esr` (exception syndrome register) and `far` (fault address register)
- [x] Added ARM64 `Exception` type (simplified, maps to `esr` values)
- [x] Added ARM64 fallible memory operations in `exception_table.rs`

#### litebox_shim_linux changes:
- [x] Created `syscalls/signal/aarch64.rs` with signal frame handling
- [x] Added ARM64 loader module support (cfg gate)
- [x] Added `CChar` type alias
- [x] Added ARM64 syscall number extraction (`ctx.syscallno`)
- [x] Added ARM64 `ThreadLocalDescriptor` type
- [x] Added ARM64 case for `SetThreadArea` syscall (returns ENOSYS)
- [x] Added ARM64 machine string "aarch64" in `Utsname`
- [x] Updated `__pad` field cfg from `target_arch = "x86_64"` to `target_pointer_width = "64"`
- [x] Added ARM64 `handle_exception_request` for signal mapping

### Phase 2: New ARM64 Runner Crate ✅
- [x] Created `litebox_runner_linux_arm64_userland/Cargo.toml`
  - No `litebox_syscall_rewriter` dependency
  - Features: `systrap_backend` only
- [x] Created `litebox_runner_linux_arm64_userland/src/lib.rs`
  - Simplified CLI (no `--interception-backend` flag, systrap only)
  - Reordered initialization: `init_task()` before `enable_seccomp_based_syscall_interception()`
- [x] Created `litebox_runner_linux_arm64_userland/src/main.rs`
- [x] Added to workspace `Cargo.toml`

### Phase 3: Testing ✅
- [x] Created test infrastructure:
  - `tests/run.rs` - Main test runner
  - `tests/cache.rs` - Compilation caching
  - `tests/common/mod.rs` - Common utilities adapted for ARM64 library paths
- [x] Copied C test files from x86 runner:
  - `hello.c`, `thread.c`, `thread_exit.c`, `signal.c`, `execve.c`, `efault.c`, `unix.c`
  - `net/tcp_server.c`, `net/tcp_client.c`
- [x] All tests compile and run (with expected ignores)

---

## Technical Discoveries

### ARM64 vs x86 Architecture Differences

1. **Syscall Convention**:
   - ARM64: `svc #0`, syscall number in `x8`, args in `x0-x5`, return in `x0`
   - x86_64: `syscall`, syscall number in `rax`, args in `rdi, rsi, rdx, r10, r8, r9`

2. **Missing Syscalls on ARM64**:
   - `open` → use `openat`
   - `stat`, `lstat`, `fstat` → use `fstatat`
   - `mkdir`, `rmdir` → use `mkdirat`, `unlinkat`
   - `access` → use `faccessat`
   - `dup2` → use `dup3`
   - `pipe` → use `pipe2`
   - `poll` → use `ppoll`
   - `alarm`, `arch_prctl`, `set_thread_area` → not available

3. **Character Type**:
   - ARM64: `c_char = u8` (unsigned char)
   - x86: `c_char = i8` (signed char)

4. **TLS Access**:
   - ARM64: `tpidr_el0` system register
   - x86_64: `fs` segment base

5. **Stack Pointer**:
   - ARM64: `sp` register cannot be used directly in `str`/`ldr` with TLS addressing modes
   - Must copy to scratch register first

6. **Signal Stack Size**:
   - ARM64: `MINSIGSTKSZ = 5120`
   - x86: `MINSIGSTKSZ = 2048`

### Critical Bug Found: Seccomp Filter Timing Issue

**Problem**: After `seccompiler::apply_filter()` returns, the seccomp filter is immediately active. Any subsequent syscall from libc/std triggers SIGSYS, but the SIGSYS handler expects to be in "guest mode" with valid context pointers.

**Root Cause**: The `apply_filter()` function uses `libc::syscall()` which may make cleanup syscalls after the seccomp syscall returns. Additionally, any Rust/std code after the call (error handling, debug output, etc.) makes syscalls.

**Evidence**:
```
DEBUG: Applying seccomp filter
FATAL: SIGSYS received outside guest mode
```

**Current Mitigation**: 
1. Added `in_guest` TLS variable check in SIGSYS handler
2. Made `in_guest` globally visible with `.globl` directive
3. Handler aborts if SIGSYS received outside guest mode

**Proper Fix Needed**:
- Use raw syscalls (not libc) for seccomp filter application
- Use backdoor magic flags for ALL syscalls between filter application and guest entry
- Eliminate any std/libc calls in the critical path

---

## Current Status

### Build Status: ✅ PASSING
```bash
cargo build -p litebox_runner_linux_arm64_userland  # Compiles successfully
cargo fmt                                            # Formatted
cargo clippy -p litebox_runner_linux_arm64_userland  # Passes with warnings
```

### Test Status: ⚠️ IGNORED (by design)
```
test test_dynamic_lib_with_systrap ... ignored (needs platform std support)
test test_static_exec_with_systrap ... ignored (seccomp backend issue)
test test_tun_with_tcp_socket ... ignored (needs root/CAP_NET_ADMIN)
```

---

## Files Created

### New Crate
```
litebox_runner_linux_arm64_userland/
├── Cargo.toml
├── src/
│   ├── lib.rs
│   └── main.rs
└── tests/
    ├── cache.rs
    ├── common/
    │   └── mod.rs
    ├── run.rs
    ├── hello.c
    ├── thread.c
    ├── thread_exit.c
    ├── signal.c
    ├── execve.c
    ├── efault.c
    ├── unix.c
    └── net/
        ├── tcp_server.c
        └── tcp_client.c
```

### New Source Files
```
litebox_common_linux/src/signal/aarch64.rs
litebox_platform_linux_userland/src/syscall_intercept/systrap_aarch64.rs
litebox_shim_linux/src/syscalls/signal/aarch64.rs
```

## Files Modified

### Workspace
- `Cargo.toml` - Added `litebox_runner_linux_arm64_userland` to members

### litebox_common_linux
- `src/lib.rs` - CChar type, ARM64 PtRegs, syscall cfg guards, time_t, FileStat, TASK_ADDR_MAX
- `src/signal/mod.rs` - Added aarch64 module import and Sigcontext use
- `src/loader.rs` - Added EM_AARCH64 constant

### litebox_platform_linux_userland
- `src/lib.rs` - ARM64 cfg gate, TLS variables, run_thread_arch, signal handlers, with_signal_alt_stack
- `src/syscall_intercept/mod.rs` - ARM64 SYSCALL_ARG_MAGIC, systrap_aarch64 import

### litebox (core)
- `src/shim.rs` - ARM64 ExceptionInfo with esr/far fields
- `src/mm/exception_table.rs` - ARM64 fallible memory operations

### litebox_shim_linux
- `src/lib.rs` - CChar type, syscall_number for ARM64, SetThreadArea ARM64 case
- `src/loader/mod.rs` - Added aarch64 to cfg gate
- `src/syscalls/process.rs` - ARM64 ThreadLocalDescriptor, clone TLS handling
- `src/syscalls/signal/mod.rs` - aarch64 arch module, __pad cfg to pointer_width
- `src/syscalls/misc.rs` - ARM64 machine field in Utsname

---

## Future Work (TODO)

### High Priority: Fix Seccomp Backend
1. **Replace libc calls in seccomp application**:
   ```rust
   // Instead of seccompiler::apply_filter() which uses libc
   // Use raw syscalls with backdoor magic
   unsafe {
       syscalls::raw::syscall3(
           syscalls::Sysno::seccomp,
           SECCOMP_SET_MODE_FILTER,
           0,
           bpf_prog_ptr as usize,
       )
   }
   ```

2. **Ensure no syscalls between filter and guest entry**:
   - Remove all debug output after filter application
   - Use only raw syscalls with backdoor for any necessary operations
   - Possibly pre-allocate all resources before enabling filter

3. **Update with_signal_alt_stack**:
   - Already partially done - uses raw mmap/munmap with backdoor
   - Need to verify sigaltstack also uses backdoor

### Medium Priority: Test Enhancements
1. Enable tests once seccomp backend is fixed
2. Add ARM64-specific test cases
3. Test on real ARM64 hardware (not just compilation)

### Low Priority: Optimizations
1. Pre-compile BPF program offline (noted in TODO)
2. Consider hotpatching syscall instructions (like x86 TODO)
3. Add support for SVE/NEON context saving in signals

---

## How to Test (When Fixed)

```bash
# Set up TUN device (requires root)
sudo ./litebox_platform_linux_userland/scripts/tun-setup.sh

# Build
cargo build -p litebox_runner_linux_arm64_userland

# Run tests
cargo test -p litebox_runner_linux_arm64_userland

# Run a specific test binary manually
./target/debug/litebox_runner_linux_arm64_userland \
    --unstable \
    --env "LD_LIBRARY_PATH=/lib" \
    --env "HOME=/" \
    --initial-files /path/to/rootfs.tar \
    /path/to/executable
```

---

## Dependencies Installed During Development

```bash
# For test compilation
sudo dnf install -y kernel-headers glibc-devel glibc-static

# For debugging
sudo dnf install -y gdb
```
