# OCI Feature Support

This document details which OCI runtime specification features are supported by litebox-oci.

## OCI Runtime Commands

| Command | Status | Notes |
|---------|--------|-------|
| `create` | ✅ Supported | Creates container, waits for start signal |
| `start` | ✅ Supported | Triggers container execution |
| `state` | ✅ Supported | Returns OCI-compliant JSON state |
| `kill` | ✅ Supported | Sends signals to container process |
| `delete` | ✅ Supported | Removes container state |
| `exec` | ✅ Supported | Run command in container's rootfs (simplified) |

## CLI Extensions

These flags extend OCI functionality for the `run` and `exec` commands:

| Flag | Status | Notes |
|------|--------|-------|
| `--env KEY=VALUE` | ✅ Supported | Set environment variables |
| `--env-file FILE` | ✅ Supported | Load env vars from file |
| `--mount src=..,dst=..` | ✅ Supported | Bind mount (snapshot, writes don't persist) |

## config.json Fields

### Root Configuration (`root`)

| Field | Status | Notes |
|-------|--------|-------|
| `path` | ✅ Supported | Path to rootfs directory |
| `readonly` | ⚠️ Ignored | All files loaded into in-memory fs |

### Process Configuration (`process`)

| Field | Status | Notes |
|-------|--------|-------|
| `args` | ✅ Supported | Command and arguments |
| `env` | ✅ Supported | Environment variables |
| `cwd` | ✅ Supported | Working directory via `chdir()` syscall |
| `user.uid` | ⚠️ Ignored | Runs as invoking user |
| `user.gid` | ⚠️ Ignored | Runs as invoking user |
| `capabilities` | ❌ Not supported | No capability management |
| `rlimits` | ❌ Not supported | No resource limits |
| `noNewPrivileges` | ⚠️ Ignored | Always no new privileges |
| `terminal` | ❌ Not supported | No TTY support |
| `consoleSize` | ❌ Not supported | No TTY support |

### Linux-specific (`linux`)

| Field | Status | Notes |
|-------|--------|-------|
| `namespaces` | ❌ Not supported | Uses syscall emulation instead |
| `uidMappings` | ❌ Not supported | No user namespace |
| `gidMappings` | ❌ Not supported | No user namespace |
| `devices` | ❌ Not supported | No device passthrough |
| `cgroupsPath` | ❌ Not supported | No cgroup integration |
| `resources` | ❌ Not supported | No resource limits |
| `seccomp` | ❌ Not supported | Uses syscall rewriting instead |
| `rootfsPropagation` | ❌ Not supported | In-memory fs only |
| `maskedPaths` | ❌ Not supported | |
| `readonlyPaths` | ❌ Not supported | |

### Mounts (`mounts`)

| Field | Status | Notes |
|-------|--------|-------|
| Standard mounts | ❌ Not supported | Rootfs loaded into in-memory fs |
| Bind mounts (config.json) | ❌ Not supported | Use `--mount` CLI flag instead |
| Bind mounts (CLI) | ✅ Supported | Via `--mount` flag, snapshot only |
| tmpfs | ❌ Not supported | |

### Hooks (`hooks`)

| Field | Status | Notes |
|-------|--------|-------|
| `prestart` | ❌ Not supported | |
| `createRuntime` | ❌ Not supported | |
| `createContainer` | ❌ Not supported | |
| `startContainer` | ❌ Not supported | |
| `poststart` | ❌ Not supported | |
| `poststop` | ❌ Not supported | |

### Annotations (`annotations`)

| Field | Status | Notes |
|-------|--------|-------|
| Custom annotations | ✅ Stored | Stored in state, not processed |

## Syscall Support

LiteBox emulates syscalls in userspace. Most common syscalls are supported:

### Fully Supported
- File operations: `open`, `read`, `write`, `close`, `stat`, `fstat`, `lstat`, `statx`, `statfs`
- Directory operations: `mkdir`, `rmdir`, `getdents`
- Process operations: `execve`, `exit`, `wait`, `clone` (threads only)
- Memory operations: `mmap`, `munmap`, `brk`
- Misc: `getcwd`, `chdir`, `uname`

### Partially Supported
- `ioctl` - Limited terminal ioctls
- `fcntl` - Basic operations only
- `clone` - Threads only (`CLONE_VM|CLONE_THREAD`), not full processes

### Not Supported
- `fork`, `vfork` - Returns ENOSYS (use execve directly)
- `lgetxattr`, `listxattr` - Extended attributes (warnings only)
- `inotify_*` - File watching

## Process Model

LiteBox supports multi-threading but not multi-processing:

| Syscall | Status | Notes |
|---------|--------|-------|
| `clone` (threads) | ✅ Supported | With `CLONE_VM\|CLONE_THREAD` flags |
| `execve` | ✅ Supported | Replaces current process |
| `fork` | ❌ Not supported | Returns ENOSYS |
| `vfork` | ❌ Not supported | Returns ENOSYS |

**What works:**
- Multi-threaded applications (pthreads, Go goroutines, Rust threads)
- Direct command execution (`/bin/ls`, `/usr/bin/python script.py`)
- Programs using execve to run other programs

**What doesn't work:**
- Subshells (`$(...)`) — requires `fork()`
- Background jobs (`&`) — requires `fork()`
- Daemon-style process spawning

**What works (via pipeline orchestration):**
- Pipes (`|`) — `echo hello | cat`, `ls / | grep bin`, `echo a | cat | cat`
- Each pipe stage runs as a separate LiteBox process via re-exec

**Automatic shell rewriting** (enabled by default) handles most entrypoint patterns transparently:
- `sh -c "..."` → `litebox-sh -c "..."` (litebox-sh uses execve, not fork)
- `exec` inserted before final external command
- `sh /script.sh` → `litebox-sh /script.sh`
- `#!/bin/sh` shebangs rewritten to `#!/bin/litebox-sh` in rootfs
- Direct `./script.sh` execution detected and routed through litebox-sh
- Pipes: `echo hello | cat` orchestrated as sequential LiteBox processes
- Disable with `--no-rewrite-shell`

**Container compatibility (with rewriting):** Alpine 100%, BusyBox 96%, Debian 100%, Ubuntu 100% (111/112 tests pass)

## Virtual /proc Filesystem

LiteBox emulates a subset of `/proc` to enable common container tools:

### Supported /proc Files

| Path | Description | Used by |
|------|-------------|---------|
| `/proc/cpuinfo` | CPU information | Various tools |
| `/proc/meminfo` | Memory statistics | `free`, monitoring tools |
| `/proc/mounts` | Mount points | `df`, `mount` |
| `/proc/stat` | System statistics | Performance tools |
| `/proc/version` | Kernel version | `uname`, scripts |
| `/proc/uptime` | System uptime | `uptime` |
| `/proc/loadavg` | Load averages | `uptime`, `top` |
| `/proc/filesystems` | Supported filesystems | `mount` |
| `/proc/self/stat` | Process status (raw) | Process tools |
| `/proc/self/status` | Process status (readable) | `ps`, scripts |
| `/proc/self/cmdline` | Command line | `ps`, process info |
| `/proc/self/environ` | Environment variables | Debug tools |
| `/proc/self/exe` | Executable path (symlink) | Self-discovery |
| `/proc/self/cwd` | Working directory (symlink) | Self-discovery |
| `/proc/self/maps` | Memory mappings | Debug tools |
| `/proc/self/fd/<n>` | File descriptor symlinks | Debug tools |
| `/proc/<pid>/*` | Same as `/proc/self/*` | |

### Not Supported /proc Files
- `/proc/net/*` - Network statistics
- `/proc/sys/*` (except hostname, osrelease) - Kernel parameters
- `/proc/interrupts`, `/proc/ioports` - Hardware info

## Networking

### TUN-based Networking

Container networking is supported via TUN devices using the `--tun-device` flag:

```bash
# Set up TUN on host
sudo litebox_platform_linux_userland/scripts/tun-setup.sh -t tun99 -i 10.0.0.1

# Run with networking
litebox-oci run -b /bundle --tun-device tun99 my-container
```

**How it works:**
- LiteBox implements a TCP/IP stack using `smoltcp`
- Socket syscalls (`socket`, `connect`, `bind`, `listen`, `accept`, etc.) are intercepted
- IP packets are sent/received through the TUN device
- Container IP: `10.0.0.2/24` (hardcoded)
- Gateway: `10.0.0.1` (host TUN interface)

**Supported socket operations:**
- TCP: `socket`, `connect`, `bind`, `listen`, `accept`, `send`/`recv`, `close`
- UDP: `socket`, `bind`, `sendto`/`recvfrom`, `close`

**Not yet supported:**
- Raw sockets (SOCK_RAW) - ping won't work
- Some socket state edge cases
- `/proc/net/*` files

**Limitations:**
- No per-container IP isolation
- No port mapping (requires host-side iptables)
- No DNS resolution (needs `/etc/resolv.conf` in rootfs)

## Architectural Limitations

### x86_64 Only
The `rtld_audit.so` library for dynamic library syscall interception is x86_64-specific. ARM64 support requires porting this library.

### No Symlinks
Symlinks in the rootfs are resolved and flattened to regular files during loading. This is because LiteBox's in-memory filesystem doesn't support symlinks.

### No Network Isolation
All containers using the same TUN device share the IP address `10.0.0.2`. There is no per-container network namespace isolation.

### No Resource Limits
cgroups are not used, so CPU, memory, and I/O limits are not enforced.

## containerd Compatibility

litebox-oci is compatible with containerd's `io.containerd.runc.v2` shim:

| Flag | Status | Notes |
|------|--------|-------|
| `--root` | ✅ Supported | State directory |
| `--log` | ✅ Accepted | Logs to stderr |
| `--log-format` | ✅ Accepted | Ignored (always text) |
| `-b` / `--bundle` | ✅ Supported | Bundle path |
| `--pid-file` | ✅ Supported | Writes container PID |
| `--no-pivot` | ✅ Accepted | Ignored (never pivots) |
| `--no-new-keyring` | ✅ Accepted | Ignored |
| `--console-socket` | ⚠️ Accepted | Not implemented |
