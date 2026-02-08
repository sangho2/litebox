# ARM64 Runner Support for LiteBox

## Overview

This document describes the ARM64 (aarch64) support for LiteBox, implemented in the `litebox_runner_linux_arm64_userland` crate. For detailed development history and debugging notes, see [ARM64_REWRITER_DEVELOPMENT.md](./ARM64_REWRITER_DEVELOPMENT.md).

## Current Status

| Component | Status | Notes |
|-----------|--------|-------|
| Platform layer | ✅ Complete | TLS, context switching, signal handling, per-thread host TLS table |
| Syscall rewriter crate | ✅ Complete | ELF rewriting, SVC/MSR trampoline generation, ADRP+ADD for ±4GB, sigreturn trampoline |
| Runner integration | ✅ Complete | CLI with backend selection |
| Seccomp backend | ⚠️ Issues | Timing bug with SIGSYS outside guest mode |
| Rewriter backend | ✅ Working | Static and dynamically linked binaries work |
| Dynamic linking | ✅ Working | LD_AUDIT-based trampoline discovery via `litebox_rtld_audit_arm64` |
| Signal delivery | ✅ Working | Handler fires, `uc_mcontext.pc` modification restored correctly by `sys_rt_sigreturn` |
| Multi-threading | ✅ Working | Per-thread host TLS table, MSR TPIDR_EL0 interception |
| Tests | ✅ All passing | All 7 C tests (static + dynamic), ls, node, python, iperf3, 3 loader tests |

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

## Files

### New Crates

```
litebox_runner_linux_arm64_userland/
├── Cargo.toml
├── build.rs            # Build script (compiles rtld_audit .so)
├── src/
│   ├── lib.rs          # Runner implementation
│   └── main.rs         # Entry point
└── tests/
    ├── run.rs          # Runner CLI test suite (7 C tests × 2 modes + ls/node/python/iperf3)
    ├── loader.rs       # Platform/shim API loader tests (3 tests with fork isolation)
    ├── cache.rs        # Compilation cache
    ├── common/mod.rs   # Test utilities (compile, rewrite caching, dependency resolution)
    ├── efault.c        # EFAULT handling test
    ├── execve.c        # execve + CLOEXEC + threads test
    ├── hello.c         # Basic hello world, argv/envp test
    ├── signal.c        # SIGSEGV handler with PC modification test
    ├── thread.c        # 50 threads created and joined test
    ├── thread_exit.c   # Thread exit race condition test
    └── unix.c          # Unix socket pair test

litebox_syscall_rewriter_arm64/
├── Cargo.toml
├── src/
│   └── lib.rs          # Rewriter implementation (~1926 lines)
└── tests/
    └── snapshot_tests.rs

litebox_rtld_audit_arm64/
├── Cargo.toml
├── rtld_audit_arm64.c  # LD_AUDIT shared library (~540 lines, per-thread TLS table lookup)
└── litebox_rtld_audit_arm64.so
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

### C Test Results (Rewriter Backend)

| Test | Static | Dynamic | Notes |
|------|--------|---------|-------|
| `efault.c` | PASS | PASS | EFAULT handling |
| `execve.c` | PASS | PASS | execve + CLOEXEC + 20 spin threads |
| `hello.c` | PASS | PASS | argv/envp |
| `signal.c` | PASS | PASS | SIGSEGV handler fires, modifies uc_mcontext.pc, resumes correctly |
| `thread.c` | PASS | PASS | 50 threads created and joined |
| `thread_exit.c` | PASS | PASS | 20 threads + exit(0) from non-primary thread |
| `unix.c` | PASS | PASS | Socketpair works |

### Other Tests

| Test | Status | Notes |
|------|--------|-------|
| `test_runner_with_ls` | PASS | `ls` runs successfully (dynamically linked binary) |
| `test_node_with_rewriter` | PASS | Node.js (12+ shared libraries) |
| `test_runner_with_python` | PASS | Python 3.12.9 |
| `test_tun_and_runner_with_iperf3` | PASS | iperf3 server via TUN device (requires root) |

### Loader Tests (Platform/Shim API)

| Test | Status | Notes |
|------|--------|-------|
| `test_load_exec_dynamic` | PASS | Dynamically linked hello.c with rewritten deps + rtld_audit |
| `test_load_exec_static` | PASS | Statically linked hello.c |
| `test_syscall_rewriter` | PASS | Inline-asm nolibc binary with raw SVC #0 |

```bash
# Run all runner tests
cargo test -p litebox_runner_linux_arm64_userland --test run -- --nocapture

# Run all loader tests
cargo test -p litebox_runner_linux_arm64_userland --test loader -- --nocapture

# Run specific test functions
cargo test -p litebox_runner_linux_arm64_userland test_runner_with_ls -- --nocapture
cargo test -p litebox_runner_linux_arm64_userland test_node_with_rewriter -- --nocapture
cargo test -p litebox_runner_linux_arm64_userland test_runner_with_python -- --nocapture
sudo -E env "PATH=$PATH" cargo test -p litebox_runner_linux_arm64_userland test_tun_and_runner_with_iperf3 -- --nocapture

# Run rewriter unit tests
cargo test -p litebox_syscall_rewriter_arm64

# Run core litebox tests
cargo test -p litebox

# Run clippy (zero warnings required)
cargo clippy -p litebox -p litebox_common_linux -p litebox_shim_linux \
  -p litebox_syscall_rewriter_arm64 -p litebox_runner_linux_arm64_userland \
  -p litebox_platform_linux_userland

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
