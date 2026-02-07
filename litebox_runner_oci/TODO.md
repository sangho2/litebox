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
- [x] `events --stats` command for container resource stats
- [x] Unix socket path length fix for Kubernetes integration
- [x] Directory symlink handling for glibc-based distros (Debian, Ubuntu, Fedora)
- [x] `chdir()` / `fchdir()` syscall support and `process.cwd` from OCI spec
- [x] litebox-sh: fork-free minimal shell for container entrypoints
- [x] Automatic shell rewriting: `sh -c` → `litebox-sh -c` with exec insertion
- [x] Shell script file rewriting: `sh /script.sh` → `litebox-sh /script.sh`
- [x] Shebang rewriting in rootfs: `#!/bin/sh` → `#!/bin/litebox-sh`
- [x] Shebang entrypoint detection: `./script.sh` → `litebox-sh ./script.sh`
- [x] litebox-sh rewritten in Rust with musl static linking (435KB)
- [x] Multi-distro container testing (Alpine, BusyBox, Debian, Ubuntu — 111/112 99% pass)
- [x] Pipeline orchestration: `echo hello | cat`, `ls / | grep bin` work via sequential re-exec
- [x] Rootfs-aware symlink resolution for merged `/usr` layouts (Debian, Ubuntu, Fedora)

## TODO

### Short-term
- [x] Test with more container images (debian ✓, ubuntu ✓, fedora ✓)
- [x] Test with multi-process workloads (pthreads ✓, execve ✓, fork ✗)

### Medium-term
- [ ] Implement console-socket for TTY support
- [x] Implement `events` command for container metrics (basic stats output)
- [x] Optimize rootfs loading for large images (lazy loading implemented)
- [x] Lazy file loading (`--lazy-tar`, `--lazy` flags)
- [x] Kubernetes/CRI-O integration testing (crictl ✓, containerd ✓)
- [ ] Podman integration testing
- [x] Tar indexing for O(1) lookups (56% faster for complex images)
- [x] Lazy executable rewriting (`--lazy-rewrite` flag)
  - Cold cache: 31-72% faster than Eager
  - Warm cache: 59-79% faster than Eager
  - Best results: Debian 72%, Ubuntu 70%, Python 66%
- [x] Performance optimizations (rayon parallel rewriting, xxhash, mmap tar cache)

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
7. **fork() not supported**: Standard shells can't run external commands. Use litebox-sh (see below) or direct exec instead. **Automatic shell rewriting** mitigates this for most container entrypoints. **Pipeline orchestration** handles pipes.
8. **Background jobs / subshells**: `&` and `$(...)` are not supported (require fork)

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
| chdir | ✅ Supported | Change working directory |
| fork | ❌ Not supported | Returns ENOSYS |

**Implications:**
- Multi-threaded applications (Go, Rust, Java, Python threads) work
- Direct command execution works (`/bin/ls`, `/usr/bin/python`)
- Standard shells (bash, dash, ash) can't run external commands (they need fork)
- Traditional fork-then-exec patterns don't work

**Workaround:** **Automatic shell rewriting** (enabled by default) handles this transparently:
- `sh -c "..."` → `litebox-sh -c "..."` (Layer 1)
- `exec` inserted before final external command (Layer 2)
- `sh /script.sh` → `litebox-sh /script.sh` (Layer 3)
- `#!/bin/sh` → `#!/bin/litebox-sh` in script files (Layer 3)
- Direct `./script.sh` execution detected and routed through litebox-sh
- Pipes: `echo hello | cat` run as separate LiteBox processes (Layer 4)

Disable with `--no-rewrite-shell`. Or manually use **litebox-sh**, a fork-free minimal shell included in `litebox_runner_oci/tools/litebox-sh/`:
```json
// litebox-sh supports builtins + final exec:
["litebox-sh", "-c", "export FOO=bar && echo $FOO && exec /app/server"]
```

litebox-sh supports:
- Builtins: `echo`, `cd`, `pwd`, `export`, `unset`, `exit`, `test`/`[`, `true`, `false`, `set`, `exec`, `source`/`.`, `read`, `:`
- Operators: `&&`, `||`, `;`
- Variable expansion: `$VAR`, `${VAR}`, `$?`, `$0`-`$9`
- Quoting: single quotes, double quotes, backslash escaping
- I/O redirection: `>`, `>>`, `<`, `2>`, `2>&1`
- External commands via `execve` (replaces shell — no pipes or background jobs)

**Future:** Implementing fork would require forking the LiteBox process itself while sharing the emulated address space across instances. This technique was used in kernel-mode LiteBox but hasn't been ported to userspace yet.

### Lazy File Loading

**Three lazy loading modes available:**

#### 1. Squashfs Mode (`--lazy`)

Uses squashfs + loop mount for on-demand file access from kernel:

```bash
litebox_runner_oci run --bundle /path/to/bundle --lazy container_id
```

Still walks all files and rewrites executables upfront - minimal benefit over eager mode.

#### 2. True Lazy Mode (`--lazy-tar`)

Uses tar + layered filesystem for real on-demand loading:

```bash
litebox_runner_oci run --bundle /path/to/bundle --lazy-tar container_id
```

**How it works:**
1. Creates tar archive from rootfs (cached at `~/.cache/litebox-oci/tar/`)
2. Uses `tar_ro::FileSystem` as read-only lower layer
3. Uses `in_mem::FileSystem` as writable upper layer (for rewritten executables)
4. Only executables and symlinks to executables are loaded upfront
5. Non-executable files are read on-demand from tar

#### 3. ublk + Squashfs (External Setup)

Uses kernel ublk block device for lowest-overhead on-demand access:

```bash
# Setup (one-time per image)
sudo modprobe ublk_drv
rublk add loop -f /path/to/image.squashfs
sudo mount -t squashfs /dev/ublkb0 /mnt/rootfs

# Run container with mounted rootfs
litebox_runner_oci run --bundle /path/to/bundle container_id
```

**Performance Comparison by Image Size (with tar indexing):**

| Image | Size | Files | Eager (cached) | Lazy-tar (indexed) | Improvement |
|-------|------|-------|----------------|-------------------|-------------|
| Alpine | 8.7MB | 84 | 0.27s | **0.22s** | 19% faster |
| Debian bookworm-slim | 82MB | 3,264 | 0.32s | **0.13s** | **59% faster** |
| Ubuntu 24.04 | 84MB | 2,587 | 0.30s | **0.13s** | **57% faster** |
| Python 3.11-slim | 131MB | 4,944 | 0.40s | **0.18s** | **55% faster** |
| Node.js 20-slim | 205MB | 5,667 | 0.65s | **0.41s** | **37% faster** |

**Key Insights:**
- **Lazy-tar with indexing now wins for ALL image types**
- Tar indexing provides O(1) file lookups instead of O(n) linear scan
- Index is built once on tar load (one-time O(n) cost, ~5-35ms)
- Improvement scales with file count: 19% (84 files) → 59% (3264 files)
- Glibc-based distros (Debian, Ubuntu) see the largest improvement

**Mode Comparison (Python 3.11-slim):**

| Mode | First Run | Cached | Notes |
|------|-----------|--------|-------|
| Eager | 0.52s | 0.40s | Copies all files, rewriter cache helps |
| **Lazy-tar (indexed)** | 0.34s | **0.18s** | Best for all workloads now |
| ublk+squashfs | 0.48s | 0.30s | Fast after mount, kernel cache |
| Squashfuse (FUSE) | 0.58s | 0.58s | FUSE overhead (~200ms) |
| Loop+squashfs | 0.45s | 0.45s | Kernel mount overhead |

**Recommendations:**
- **All containers:** Use `--lazy-tar` (fastest with indexing)
- **Repeated access to same image:** Still consider ublk+squashfs (kernel cache)
- **Memory-constrained:** Use `--lazy-tar` (only loads accessed files)

**Limitations of `--lazy-tar`:**
- File writes to lower layer fail (e.g., Python .pyc files)
  - Workaround: `PYTHONDONTWRITEBYTECODE=1`
- Symlinks to executables flattened (tar_ro doesn't support symlinks)

### Virtual Block Device Options

#### ublk (Userspace Block Device)

ublk provides kernel block device semantics with userspace implementation via io_uring.

**Benefits over FUSE:**
- ~10-50x lower latency (no context switches per I/O)
- True block device with kernel page cache integration
- Supports any filesystem (squashfs, ext4, etc.)

**Availability on Ubuntu 24.04:**

```bash
# Install the module (not loaded by default on Azure)
sudo apt install linux-modules-extra-$(uname -r)
sudo modprobe ublk_drv

# Verify
ls /dev/ublk-control  # Should exist

# Install Rust CLI tool
cargo install rublk
```

**Usage:**

```bash
# Create ublk device backed by squashfs
rublk add loop -f /path/to/rootfs.squashfs
# Output: dev id 0: ... ublkb: 259:1 ...

# Mount as squashfs
sudo mount -t squashfs /dev/ublkb0 /mnt/rootfs

# Use with litebox (point bundle to mounted rootfs)
litebox_runner_oci run --bundle /path/to/bundle container_id

# Cleanup
sudo umount /mnt/rootfs
rublk del -n 0
```

**Kernel Support Matrix:**

| Environment | ublk Module | Notes |
|-------------|-------------|-------|
| Ubuntu 24.04 (generic) | ✅ | In linux-modules-extra |
| Ubuntu 24.04 (Azure) | ✅ | In linux-modules-extra-*-azure |
| Ubuntu 22.04 | ❌ | Kernel too old (needs 5.19+) |
| Custom kernel | ✅ | Enable CONFIG_BLK_DEV_UBLK |

**Rust crates for ublk:**
- `libublk` (0.4.5) - Library for building ublk devices
- `rublk` (0.2.13) - CLI tool with loop, null, qcow2 targets

#### FUSE Alternatives (Not Recommended)

FUSE adds ~150-200ms overhead per container invocation, making it unsuitable for short-lived containers.

| Tool | Overhead | Use Case |
|------|----------|----------|
| squashfuse | ~200ms | Mount squashfs without kernel module |
| ratarmount | ~200ms + index | Indexed tar access, good for large archives |
| archivemount | Higher | Legacy, avoid |

**When FUSE might help:**
- Long-running daemon containers (amortize mount overhead)
- Environments without ublk support
- Development/debugging (easier to inspect)

#### Performance Deep Dive

**Why lazy-tar beats ublk for one-shot:**
1. No device creation overhead (~26ms for ublk add)
2. No mount syscall overhead (~22ms)
3. tar_ro reads directly from cached tar file
4. LiteBox's layered FS handles copy-on-write natively

**Why ublk wins for repeated access:**
1. Kernel page cache persists across containers
2. Block device semantics enable readahead
3. Squashfs decompression cached at block level
4. No userspace involvement after mount

**Future improvements:**
- Add `--ublk` flag that handles device setup automatically
- Index tar file for O(1) lookups (like ratarmount)
- Lazy executable rewriting at exec() time
- Support for multi-layer OCI images
- Daemon mode to keep images mounted

### Benchmark Results

See [BENCHMARKS.md](BENCHMARKS.md) for comprehensive performance data including:
- Container startup by image type (Alpine, Python, large images)
- Loading mode comparison (eager, lazy-tar, FUSE, ublk)
- Memory usage estimates
- Recommendations by use case

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
