# ARM64 Runner Support for LiteBox

## Overview

This document describes the ARM64 (aarch64) support for LiteBox, implemented in the `litebox_runner_linux_arm64_userland` crate. For detailed development history and debugging notes, see [ARM64_REWRITER_DEVELOPMENT.md](./ARM64_REWRITER_DEVELOPMENT.md).

## Current Status

| Component | Status | Notes |
|-----------|--------|-------|
| Platform layer | ✅ Complete | TLS, context switching, signal handling |
| Syscall rewriter crate | ✅ Complete | ELF rewriting, trampoline generation |
| Runner integration | ✅ Complete | CLI with backend selection |
| Seccomp backend | ⚠️ Issues | Timing bug with SIGSYS outside guest mode |
| Rewriter backend | 🔄 In Progress | Crash after syscall resumption |
| Tests | ⏸️ Blocked | Waiting for backend fixes |

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

| x86 Syscall | ARM64 Alternative |
|-------------|-------------------|
| `open` | `openat` |
| `stat`, `lstat`, `fstat` | `fstatat` |
| `mkdir`, `rmdir` | `mkdirat`, `unlinkat` |
| `access` | `faccessat` |
| `dup2` | `dup3` |
| `pipe` | `pipe2` |
| `poll` | `ppoll` |

## Known Issues

### 1. Seccomp Backend Timing (Critical)

**Problem**: After `seccompiler::apply_filter()` returns, the seccomp filter is immediately active. Any syscall from libc/std triggers SIGSYS, but the handler expects to be in "guest mode."

**Mitigation**: Added `in_guest` check in SIGSYS handler that aborts if called outside guest mode.

**Fix needed**: Use raw syscalls with backdoor magic for all operations between filter application and guest entry.

### 2. Rewriter Backend Crash (In Progress)

**Problem**: Guest crashes at PC=0 after resuming from syscall.

**Status**: Under investigation. See [ARM64_REWRITER_DEVELOPMENT.md](./ARM64_REWRITER_DEVELOPMENT.md) for detailed debugging notes.

## Files

### New Crates

```
litebox_runner_linux_arm64_userland/
├── Cargo.toml
├── src/
│   ├── lib.rs          # Runner implementation
│   └── main.rs         # Entry point
└── tests/
    ├── run.rs          # Test runner
    ├── cache.rs        # Compilation cache
    ├── common/mod.rs   # Test utilities
    ├── hello.c         # Basic test
    ├── thread.c        # Threading test
    └── ...             # More tests

litebox_syscall_rewriter_arm64/
├── Cargo.toml
├── src/
│   └── lib.rs          # Rewriter implementation (~900 lines)
└── tests/
    └── snapshot_tests.rs
```

### Modified Files

| File | Changes |
|------|---------|
| `Cargo.toml` (workspace) | Added new crates to members |
| `litebox_common_linux/src/lib.rs` | ARM64 PtRegs, CChar type |
| `litebox_common_linux/src/signal/aarch64.rs` | New: signal context |
| `litebox_platform_linux_userland/src/lib.rs` | ARM64 assembly routines |
| `litebox_shim_linux/src/lib.rs` | ARM64 syscall handling |
| `litebox_shim_linux/src/syscalls/signal/aarch64.rs` | New: signal frame |

## Testing

Tests are currently ignored pending backend fixes:

```bash
# Run tests (will show ignored)
cargo test -p litebox_runner_linux_arm64_userland

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
