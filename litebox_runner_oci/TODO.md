# LiteBox OCI Runtime - TODO

## Completed

- [x] OCI config.json parsing via oci-spec crate
- [x] Rootfs loading into LiteBox's in-memory filesystem
- [x] Symlink resolution (flattened to regular files)
- [x] Syscall rewriting for executables
- [x] rtld_audit.so inclusion for dynamic library support
- [x] PATH resolution for command lookup
- [x] Full OCI lifecycle: create, start, state, kill, delete, list
- [x] containerd compatibility flags
- [x] Successful containerd integration test
- [x] bundle.py for binary and image bundling
- [x] integration_test.sh for automated testing
- [x] Rootless operation support
- [x] Manual testing instructions (instruction.md)
- [x] Clippy warnings fixed
- [x] `--version` output with git commit hash
- [x] `exec` command for running commands in containers
- [x] Improved error messages for common failures
- [x] Unit tests for state.rs (18 tests)
- [x] Unit tests for lifecycle.rs (18 tests)
- [x] README.md with quick start guide
- [x] Document supported/unsupported OCI features (OCI_FEATURES.md)
- [x] Troubleshooting guide (TROUBLESHOOTING.md)
- [x] `--env` and `--env-file` flags
- [x] `--mount` for additional bind mounts
- [x] `--stdout` and `--stderr` for stdio redirection
- [x] Cache rewritten binaries for faster subsequent runs
- [x] TUN-based networking via `--tun-device` flag (TCP/UDP supported, raw sockets not yet)
- [x] statx syscall implementation
- [x] statfs/fstatfs syscall implementation
- [x] Virtual /proc filesystem emulation (cpuinfo, meminfo, mounts, stat, status, etc.)

## TODO

### Short-term
- [ ] Test with more container images (debian, ubuntu, fedora)
- [x] Test with multi-process workloads (pthreads ✓, execve ✓, fork ✗)

### Medium-term
- [ ] Implement console-socket for TTY support
- [ ] Implement `events` command for container metrics
- [ ] Optimize rootfs loading for large images
- [ ] Lazy file loading (load on first access)
- [ ] Kubernetes/CRI-O integration testing
- [ ] Podman integration testing

### Long-term
- [ ] ARM64 support (requires rtld_audit.so port)
- [ ] Per-container network isolation (multiple TUN devices or veth pairs)
- [ ] Raw socket support (SOCK_RAW for ping/ICMP)
- [ ] fork() support (see notes below)
- [ ] seccomp backend improvements
- [ ] Audit syscall emulation for security gaps
- [ ] Support for capabilities
- [ ] Checkpoint/restore support

## Known Issues

1. **Large rootfs slow**: Loading python:3.11-slim (~5000 files) takes several seconds
2. **Some syscalls unsupported**: lgetxattr, listxattr show warnings but don't break execution
3. **whoami fails**: Needs proper /etc/passwd and utmp support
4. **Alpine cleanup segfault**: Sometimes segfaults during cleanup (doesn't affect execution)
5. **Raw sockets not supported**: ping and other ICMP tools fail (SOCK_RAW not implemented in litebox)
6. **Some TCP edge cases**: Certain socket state transitions cause panics in smoltcp stack
7. **fork() not supported**: Shell scripts can't run external commands (use direct exec instead)

## Notes

### Why not use youki's libcontainer?

youki performs `pivot_root` and Linux namespace setup BEFORE calling the Executor trait. This fundamentally conflicts with LiteBox's approach of:
1. Loading files into in-memory filesystem
2. Rewriting syscalls in executables
3. Running through syscall emulation

### Backend choice

Must use "rewriter" backend, not "seccomp". The seccomp backend tests are marked `#[ignore]` in LiteBox with a note about needing modifications.

### containerd integration

Uses existing `io.containerd.runc.v2` shim - no custom shim needed. Critical flags:
- `--log` and `--log-format` (accepted for compatibility)
- `-b` (short for --bundle)
- `--pid-file` (write PID for shim)

### Networking

TUN-based networking uses LiteBox's smoltcp TCP/IP stack:
- Container IP: `10.0.0.2/24` (hardcoded)
- Gateway: `10.0.0.1` (host TUN interface)
- Supports: TCP, UDP sockets
- Not supported: Raw sockets (ICMP/ping)

### Multi-threading and Process Model

LiteBox supports **pthreads** (multi-threading) and **execve**, but not **fork()**:

| Feature | Status | Notes |
|---------|--------|-------|
| pthreads | ✅ Supported | `clone()` with `CLONE_VM\|CLONE_THREAD` |
| execve | ✅ Supported | Replaces current process image |
| fork | ❌ Not supported | Returns ENOSYS |

**Implications:**
- Multi-threaded applications (Go, Rust, Java, Python threads) work
- Direct command execution works (`/bin/ls`, `/usr/bin/python`)
- Shell scripts calling external commands fail (sh needs fork)
- Traditional fork-then-exec patterns don't work

**Workaround:** Run commands directly instead of through shell:
```json
// Instead of: ["sh", "-c", "ls /"]
// Use:        ["/bin/ls", "/"]
```

**Future:** Implementing fork would require forking the LiteBox process itself while sharing the emulated address space across instances. This technique was used in kernel-mode LiteBox but hasn't been ported to userspace yet.

### Virtual /proc Filesystem

LiteBox emulates a subset of /proc for container compatibility:
- `/proc/cpuinfo` - CPU information
- `/proc/meminfo` - Memory statistics
- `/proc/mounts` - Mount points (for `df` command)
- `/proc/stat` - System statistics
- `/proc/version` - Kernel version string
- `/proc/loadavg` - Load averages
- `/proc/uptime` - System uptime
- `/proc/filesystems` - Supported filesystems
- `/proc/self/stat` - Process status
- `/proc/self/status` - Human-readable process status
- `/proc/self/cmdline` - Command line arguments
- `/proc/self/environ` - Environment variables
- `/proc/self/exe` - Executable path (symlink)
- `/proc/self/cwd` - Current directory (symlink)
- `/proc/self/maps` - Memory mappings
