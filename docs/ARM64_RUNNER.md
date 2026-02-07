# ARM64 Runner Support for LiteBox

## Overview

This document describes the ARM64 (aarch64) support for LiteBox, implemented in the `litebox_runner_linux_arm64_userland` crate. For detailed development history and debugging notes, see [ARM64_REWRITER_DEVELOPMENT.md](./ARM64_REWRITER_DEVELOPMENT.md).

## Current Status

| Component | Status | Notes |
|-----------|--------|-------|
| Platform layer | ✅ Complete | TLS, context switching, signal handling |
| Syscall rewriter crate | ✅ Complete | ELF rewriting, trampoline generation, ADRP+ADD for ±4GB |
| Runner integration | ✅ Complete | CLI with backend selection |
| Seccomp backend | ⚠️ Issues | Timing bug with SIGSYS outside guest mode |
| Rewriter backend | ✅ Working | Static and dynamically linked binaries work |
| Dynamic linking | ✅ Working | LD_AUDIT-based trampoline discovery via `litebox_rtld_audit_arm64` |
| Tests | ✅ Core tests passing | `test_static_exec`, `test_dynamic_lib`, `test_runner_with_ls` all pass |

### Supported Syscalls

| Syscall | ARM64 Number | Status | Notes |
|---------|-------------|--------|-------|
| `openat` | 56 | ✅ | Replaces x86 `open` |
| `newfstatat` | 79 | ✅ | With correct ARM64 128-byte `FileStat` layout |
| `statx` | 291 | ✅ | Modern glibc prefers this over `newfstatat` |
| `statfs` | 43 | ✅ | Returns tmpfs-like values |
| `faccessat` | 48 | ✅ | Replaces x86 `access` |
| `read` | 63 | ✅ | |
| `write` | 64 | ✅ | |
| `brk` | 214 | ✅ | |
| `mmap` | 222 | ✅ | |
| `mprotect` | 226 | ✅ | |
| `munmap` | 215 | ✅ | |
| `close` | 57 | ✅ | |
| `ioctl` | 29 | ✅ | |
| `writev` | 66 | ✅ | |
| `exit_group` | 94 | ✅ | |
| `faccessat2` | 439 | ❌ | Not yet implemented (only `faccessat`) |

## Quick Start

```bash
# Build
cargo build -p litebox_runner_linux_arm64_userland

# Run with rewriter backend (recommended)
./target/debug/litebox_runner_linux_arm64_userland \
    --unstable \
    --interception-backend rewriter \
    --rewrite-syscalls \
    --initial-files rootfs.tar \
    /path/to/static/binary

# Run with seccomp backend (has timing issues)
./target/debug/litebox_runner_linux_arm64_userland \
    --unstable \
    --interception-backend seccomp \
    --initial-files rootfs.tar \
    /path/to/static/binary
```

## Architecture

### Two Syscall Interception Backends

1. **Rewriter backend** (recommended):
   - Rewrites ELF binary before execution
   - Replaces SVC instructions with branches to trampolines
   - Trampolines redirect to shim for syscall emulation
   - Avoids per-syscall signal overhead

2. **Seccomp backend** (fallback):
   - Uses seccomp BPF filter to trap syscalls
   - Generates SIGSYS signal for each syscall
   - Signal handler redirects to shim
   - Has timing issues (see Known Issues)

### Key Constraints

- The x86 syscall rewriter uses `iced-x86` which doesn't support ARM
- Created new `litebox_syscall_rewriter_arm64` using `yaxpeax-arm`
- `litebox_rtld_audit` is x86_64 only - not needed with systrap/rewriter backends
- ARM64 has different syscall numbers and conventions than x86

## Implementation Details

### ARM64 vs x86 Differences

| Aspect | ARM64 | x86_64 |
|--------|-------|--------|
| Syscall instruction | `SVC #0` | `syscall` |
| Syscall number | `x8` | `rax` |
| Arguments | `x0-x5` | `rdi, rsi, rdx, r10, r8, r9` |
| Return value | `x0` | `rax` |
| TLS register | `tpidr_el0` | `fs` segment |
| Link register | `x30` | Return address on stack |

### Missing Syscalls on ARM64

These x86 syscalls don't exist on ARM64 and must use alternatives:

| x86 Syscall | ARM64 Alternative | Implemented? |
|-------------|-------------------|-------------|
| `open` | `openat` | ✅ Yes |
| `stat`, `lstat`, `fstat` | `newfstatat` / `statx` | ✅ Yes |
| `mkdir`, `rmdir` | `mkdirat`, `unlinkat` | ✅ Yes |
| `access` | `faccessat` | ✅ Yes |
| `dup2` | `dup3` | ✅ Yes |
| `pipe` | `pipe2` | ✅ Yes |
| `poll` | `ppoll` | ✅ Yes |

## Known Issues

### 1. Seccomp Backend Timing (Critical)

**Problem**: After `seccompiler::apply_filter()` returns, the seccomp filter is immediately active. Any syscall from libc/std triggers SIGSYS, but the handler expects to be in "guest mode."

**Mitigation**: Added `in_guest` check in SIGSYS handler that aborts if called outside guest mode.

**Fix needed**: Use raw syscalls with backdoor magic for all operations between filter application and guest entry.

### 2. Threading/Signal Tests

**Problem**: Tests involving threading (thread.c, thread_exit.c) and signals (signal.c) are currently skipped due to host TLS race condition in multi-threaded scenarios.

**Root Cause**: The host TLS pointer stored at `trampoline_base+16` is a single global location. In multi-threaded scenarios, one thread can overwrite another thread's TLS pointer.

**Workaround**: These tests are skipped in the test suite until per-thread TLS storage is implemented.

## Files

### New Crates

```
litebox_runner_linux_arm64_userland/
├── Cargo.toml
├── build.rs            # Build script
├── src/
│   ├── lib.rs          # Runner implementation
│   └── main.rs         # Entry point
└── tests/
    ├── run.rs          # Test runner
    ├── cache.rs        # Compilation cache
    ├── common/mod.rs   # Test utilities (rewrite caching, dependency resolution)
    ├── hello.c         # Basic test
    ├── thread.c        # Threading test
    └── ...             # More tests

litebox_syscall_rewriter_arm64/
├── Cargo.toml
├── src/
│   └── lib.rs          # Rewriter implementation (~900 lines)
└── tests/
    └── snapshot_tests.rs

litebox_rtld_audit_arm64/
├── Cargo.toml
└── src/
    └── lib.rs          # LD_AUDIT shared library for runtime trampoline loading
```

### Modified Files

| File | Changes |
|------|---------|
| `Cargo.toml` (workspace) | Added new crates to members |
| `litebox/src/fs/in_mem.rs` | Added O_DIRECT, O_NDELAY to supported OFlags |
| `litebox/src/fs/layered.rs` | Added O_DIRECT, O_NDELAY to supported OFlags |
| `litebox/src/fs/tar_ro.rs` | Added O_DIRECT, O_NDELAY to supported OFlags |
| `litebox/src/mm/tests.rs` | Added aarch64 TASK_ADDR_MAX constant |
| `litebox_common_linux/src/lib.rs` | ARM64 PtRegs, CChar, FileStat (128-byte), StatFs, Statx, StatxTimestamp structs; statx_mask constants; faccessat/statfs/statx SyscallRequest variants |
| `litebox_common_linux/src/loader.rs` | Trampoline section loading and mapping |
| `litebox_common_linux/src/mm.rs` | Memory management |
| `litebox_common_linux/src/signal/aarch64.rs` | Signal context |
| `litebox_platform_linux_userland/src/lib.rs` | ARM64 assembly: switch_to_guest, syscall_callback, signal handler, TLS |
| `litebox_shim_linux/src/lib.rs` | ARM64 syscall handling, dispatchers for faccessat/statfs/statx |
| `litebox_shim_linux/src/loader/elf.rs` | Trampoline address propagation from interpreter |
| `litebox_shim_linux/src/syscalls/file.rs` | sys_faccessat, sys_statfs, sys_statx handlers |
| `litebox_shim_linux/src/syscalls/signal/aarch64.rs` | Signal frame |
| `litebox_syscall_rewriter_arm64/src/lib.rs` | ADRP+ADD PC-relative addressing, encode_adrp() |

## Testing

Core tests pass with the rewriter backend:

```bash
# Run all runner tests
cargo test -p litebox_runner_linux_arm64_userland
# test_static_exec_with_rewriter  ... ok
# test_dynamic_lib_with_rewriter  ... ok
# test_runner_with_ls             ... ok
# test_node_with_rewriter         ... FAILED (requires node.js installed)
# test_static_exec_with_systrap   ... ignored
# test_dynamic_lib_with_systrap   ... ignored

# Run rewriter unit tests (4/4 pass)
cargo test -p litebox_syscall_rewriter_arm64

# Run core litebox tests (90/90 pass)
cargo test -p litebox

# For TUN/TAP tests, set up network first:
sudo ./litebox_platform_linux_userland/scripts/tun-setup.sh
```

## Development Dependencies

```bash
# Required for test compilation
sudo dnf install -y kernel-headers glibc-devel glibc-static

# For debugging
sudo dnf install -y gdb

# For ARM64 cross-compilation (if needed)
sudo dnf install -y gcc-aarch64-linux-gnu
```

## References

- [ARM64_REWRITER_DEVELOPMENT.md](./ARM64_REWRITER_DEVELOPMENT.md) - Detailed development log
- ARM Architecture Reference Manual (ARMv8-A)
- Linux kernel: `arch/arm64/include/uapi/asm/sigcontext.h`
