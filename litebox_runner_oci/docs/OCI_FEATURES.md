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
| `rlimits` | ⚠️ Partial | NOFILE and STACK tracked; others return unlimited |
| `noNewPrivileges` | ⚠️ Ignored | Always no new privileges |
| `terminal` | ✅ Supported | PTY via console-socket |
| `consoleSize` | ⚠️ Ignored | Hardcoded 20×20 default |

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
| Bind mounts (config.json) | ⚠️ Partial | File bind mounts loaded (e.g., `/etc/resolv.conf`) |
| Bind mounts (CLI) | ✅ Supported | Via `--mount` flag, snapshot only |
| tmpfs | ❌ Not supported | |

**OCI spec bind mounts:** When the OCI spec includes `type: "bind"` mounts pointing to
host files (e.g., `/etc/resolv.conf`, `/etc/hosts`, `/etc/hostname`), litebox-oci reads
them and loads their contents into the in-memory filesystem. This enables DNS resolution
and hostname configuration in containers launched by Podman or containerd.

### Hooks (`hooks`)

| Field | Status | Notes |
|-------|--------|-------|
| `prestart` | ✅ Supported | Deprecated; runs before start signal. Receives state JSON on stdin |
| `createRuntime` | ✅ Supported | Runs after create, before start (runtime namespace) |
| `createContainer` | ✅ Supported | Runs after create (same as createRuntime — no separate container ns) |
| `startContainer` | ✅ Supported | Runs before start signal (same context as prestart) |
| `poststart` | ✅ Supported | Runs after container started. Best-effort (errors logged, not fatal) |
| `poststop` | ✅ Supported | Runs during delete, after container stopped. Best-effort |

All hooks receive the OCI container state as JSON on stdin. Hooks are executed sequentially;
if a hook fails (non-zero exit), subsequent hooks in the same phase are skipped. Hooks support
`path`, `args`, `env`, and `timeout` fields per OCI spec.

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
- `ioctl` - Terminal ioctls (TCGETS, TCSETS/W/F, TIOCGWINSZ, TIOCSWINSZ, TIOCGPGRP, TIOCSPGRP)
- `fcntl` - Basic operations only
- `clone` - Threads only (`CLONE_VM|CLONE_THREAD`), not full processes
- `pselect6` - Works for sockets/pipes; does not wake on raw stdio fd (PTY slave)
- `prlimit64` - NOFILE and STACK tracked; others return unlimited

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

### Architecture

LiteBox is a single-process runtime with a three-phase lifecycle:

1. **Setup** (privileged): create TUN device, configure kernel IP forwarding + NAT
   (via `ip` and `iptables` commands), open all required file descriptors, prepare rootfs
2. **Lock down**: install seccomp filter — after this point, only `read`/`write`/`poll`/`futex`
   are permitted on pre-opened file descriptors
3. **Execute**: load and run the guest binary in the same process and address space

All network syscalls from the guest are intercepted by the shim and processed through a userspace
TCP/IP stack (`smoltcp`), which sends/receives raw IP packets through a TUN device. The kernel
handles IP forwarding and NAT from the TUN to the CNI network — no userspace packet bridge needed.

```
┌─────────────────────────────────────────────────┐
│  litebox-oci (single process)                   │
│                                                 │
│  1. Setup: open TUN, configure NAT, open fds    │
│  2. Lock down: install seccomp filter           │
│  3. Execute: run guest in same address space    │
│     Guest app → shim → smoltcp → TUN fd         │
│     (only read/write/poll/futex on pre-opened   │
│      file descriptors)                          │
└──────────┬──────────────────────────────────────┘
           │ TUN fd (litebox0)
┌──────────▼──────────────────────────────────────┐
│  Kernel                                         │
│  IP forwarding + iptables MASQUERADE            │
│  TUN (10.0.0.1) ←→ veth/eth0 (CNI network)     │
└─────────────────────────────────────────────────┘
```

**Security**: Even if the sandbox is compromised, the attacker can only `read`/`write`/`poll`/`futex`
on pre-opened file descriptors. No `socket()`, `ioctl()`, or `open()` calls are available — the
attacker cannot access any network interface directly.

### Automatic CNI Networking (Podman and ctr)

When launched via Podman or `ctr --cni`, litebox-oci automatically detects the CNI-configured
network namespace and sets up networking with zero configuration:

```bash
# Podman — just works, no flags needed
sudo podman run --rm --runtime /usr/local/bin/litebox-oci \
  docker.io/library/alpine:latest /bin/ping -c 3 10.0.0.1

# DNS resolution
sudo podman run --rm --runtime /usr/local/bin/litebox-oci \
  docker.io/library/alpine:latest /usr/bin/nslookup dns.google

# containerd (ctr) — use --cni flag
sudo ctr run --rm --cni --runc-binary /usr/local/bin/litebox-oci \
  docker.io/library/alpine:latest test /bin/ping -c 3 10.0.0.1
```

**How it works:**
1. The container manager creates a network namespace with a veth pair (via CNI plugins)
   - Podman: creates the netns before calling the runtime, passes the path in the OCI spec
   - containerd (`ctr --cni`): the runtime creates a new netns via `unshare(CLONE_NEWNET)`,
     then `ctr` applies CNI plugins to `/proc/<pid>/ns/net`
2. litebox-oci detects the netns — either from the OCI spec path (Podman) or by detecting
   a non-loopback interface in the current namespace (`ctr --cni`)
3. Reads the veth config (IP, gateway, MTU)
4. Creates a TUN device (`litebox0`) inside the netns
5. Configures kernel IP forwarding + NAT (MASQUERADE) so TUN traffic routes through the veth
6. smoltcp uses `10.0.0.2/24` (internal), kernel NATs to the container's CNI IP

### Manual TUN Device

For environments without CNI, use the `--tun-device` flag:

```bash
# Set up TUN on host
sudo ip tuntap add dev tun99 mode tun
sudo ip addr add 10.0.0.1/24 dev tun99
sudo ip link set tun99 up
sudo sysctl -w net.ipv4.ip_forward=1
sudo iptables -t nat -A POSTROUTING -s 10.0.0.0/24 ! -o tun99 -j MASQUERADE

# Run with Podman (overrides auto-CNI)
sudo podman run --rm --runtime /usr/local/bin/litebox-oci \
  --runtime-flag='tun-device=tun99' alpine /bin/ping -c 3 10.0.0.1
```

### Supported Socket Operations

- **TCP**: `socket`, `connect`, `bind`, `listen`, `accept`, `send`/`recv`, `close`
- **UDP**: `socket`, `bind`, `sendto`/`recvfrom`, `connect`+`write`/`read`, `close`
- **ICMP**: `socket(SOCK_RAW/SOCK_DGRAM, IPPROTO_ICMP)`, `sendto`/`recvfrom` (ping works)
- **DNS**: UDP-based resolution via `/etc/resolv.conf` (loaded from OCI spec bind mounts)
- **Timer**: `setitimer(ITIMER_REAL)` for SIGALRM delivery (required by ping)

### Not Yet Supported

- Generic raw sockets (`SOCK_RAW` with non-ICMP protocols)
- `/proc/net/*` files

### Limitations

- No per-container IP isolation (all containers use smoltcp IP `10.0.0.2`)
- No port mapping (requires host-side iptables)

## Architectural Limitations

### x86_64 Only
The `rtld_audit.so` library for dynamic library syscall interception is x86_64-specific. ARM64 support requires porting this library.

### No Symlinks
Symlinks in the rootfs are resolved and flattened to regular files during loading. This is because LiteBox's in-memory filesystem doesn't support symlinks.

### No Network Isolation
All containers use the smoltcp-internal IP address `10.0.0.2`. With auto-CNI, the kernel NATs this to the container's real CNI-assigned IP, but there is no per-container isolation within smoltcp itself.

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
| `--console-socket` | ✅ Supported | PTY master sent via SCM_RIGHTS |

## Podman Compatibility

litebox-oci works with Podman (rootless and rootful) via the `--runtime` flag:

```bash
podman run --rm --runtime /usr/local/bin/litebox-oci alpine:latest /bin/echo "Hello"
```

| Flag | Status | Notes |
|------|--------|-------|
| `--systemd-cgroup` | ✅ Accepted | Ignored (no cgroup integration) |
| `--root` | ✅ Supported | Auto-detects rootless path via `XDG_RUNTIME_DIR` |
| `-b` / `--bundle` | ✅ Supported | Absolute `root.path` from overlay storage works |
| `--pid-file` | ✅ Supported | Writes container PID |

**Rootless support:** When `XDG_RUNTIME_DIR` is set (default in user sessions and
Podman user namespaces), state is stored in `$XDG_RUNTIME_DIR/litebox-oci/` instead
of `/run/litebox-oci/`. No root or `sudo` required.

**Tested:** Podman 4.9 with Alpine, Debian, Ubuntu — 21/21 tests pass.

## TTY / Console Socket

litebox-oci implements the OCI console-socket protocol for TTY support:

1. Runtime creates a PTY master/slave pair via `posix_openpt`
2. Master fd is sent to the orchestrator via `SCM_RIGHTS` over the console-socket
3. Slave is dup2'd onto container stdio (fd 0/1/2) with `setsid` + `TIOCSCTTY`

```bash
# containerd
sudo ctr run --tty --runc-binary /usr/local/bin/litebox-oci \
    docker.io/library/alpine:latest test /bin/sh -c "echo hello"

# Podman
podman run --rm -t --runtime /usr/local/bin/litebox-oci \
    alpine:latest /bin/echo "hello"
```

| Feature | Status | Notes |
|---------|--------|-------|
| Console-socket (SCM_RIGHTS) | ✅ Works | All 4 distros tested |
| TTY output (echo, ls, uname) | ✅ Works | Output flows through PTY |
| TCGETS/TCSETS/TCSETSW/TCSETSF | ✅ Stubbed | Returns default termios |
| TIOCGWINSZ/TIOCSWINSZ | ✅ Stubbed | Hardcoded 20×20, set ignored |
| TIOCGPGRP/TIOCSPGRP | ✅ Stubbed | Returns pgrp=1, set ignored |
| Interactive shell | ❌ Hangs | stdin polling (pselect) doesn't wake on PTY data |

**Limitation:** Interactive shells (`/bin/sh`, `/bin/bash` without `-c`) show a prompt
(bash shows `root@litebox:/#`) but hang waiting for input. The sandbox's `pselect`/`select`
implementation cannot poll the host's raw stdio fd for incoming data.
