# litebox-oci

OCI-compliant container runtime powered by LiteBox userspace syscall emulation.

## Overview

`litebox-oci` is an alternative container runtime that uses LiteBox's syscall interception instead of traditional Linux namespaces and cgroups. Similar to gVisor's approach, but implemented in Rust.

## Quick Start

### Build

```bash
cargo build -p litebox_runner_oci --release
sudo cp target/release/litebox_runner_oci /usr/local/bin/litebox-oci
```

### Run a Container Directly

```bash
# Create a test bundle
mkdir -p /tmp/test-bundle/rootfs
cat > /tmp/test-bundle/config.json << 'EOF'
{
  "ociVersion": "1.0.0",
  "root": { "path": "rootfs" },
  "process": {
    "args": ["/bin/echo", "Hello from LiteBox!"],
    "env": ["PATH=/usr/local/bin:/usr/bin:/bin"],
    "cwd": "/"
  }
}
EOF

# Copy a minimal rootfs (e.g., from Alpine)
# skopeo copy docker://alpine:latest oci:alpine:latest
# umoci unpack --image alpine:latest /tmp/test-bundle

# Run
litebox-oci run --bundle /tmp/test-bundle my-container
```

### Use with containerd

```bash
# Pull an image
sudo ctr image pull docker.io/library/alpine:latest

# Run with litebox-oci as the runtime
sudo ctr run --rm --runc-binary /usr/local/bin/litebox-oci \
    docker.io/library/alpine:latest test-container \
    /bin/echo "Hello via containerd"
```

### Kubernetes/CRI Integration

Configure containerd to use litebox-oci as an alternative runtime:

```toml
# /etc/containerd/config.toml
[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.litebox]
  runtime_type = "io.containerd.runc.v2"
  [plugins."io.containerd.grpc.v1.cri".containerd.runtimes.litebox.options]
    BinaryName = "/usr/local/bin/litebox-oci"
```

Then run containers with the `litebox` runtime:

```bash
# Using crictl (CRI)
sudo crictl runp --runtime=litebox pod.json
sudo crictl create <pod-id> container.json pod.json
sudo crictl start <container-id>

# Using ctr (containerd)
sudo ctr run --runc-binary /usr/local/bin/litebox-oci \
    docker.io/library/alpine:latest test /bin/echo "Hello"
```

**Tested configurations:**
- containerd 1.7+ with CRI plugin
- crictl v1.28+
- Podman 4.9+ (rootless)
- Alpine, Debian, Ubuntu, Fedora images

### Use with Podman

```bash
# Run with litebox-oci as the runtime
podman run --rm --runtime /usr/local/bin/litebox-oci \
    alpine:latest /bin/echo "Hello via Podman"

# Shell commands and pipes work
podman run --rm --runtime /usr/local/bin/litebox-oci \
    debian:bookworm-slim sh -c "ls / | grep bin"
```

Podman rootless mode is fully supported — no root or `sudo` required.

### Rootless Operation

```bash
litebox-oci --root ~/.litebox-oci run --bundle /tmp/test-bundle my-container
```

## OCI Lifecycle Commands

| Command | Description |
|---------|-------------|
| `create -b <bundle> <id>` | Create a container |
| `start <id>` | Start a created container |
| `state <id>` | Query container state |
| `kill <id> [signal]` | Send signal to container |
| `delete [--force] <id>` | Delete a container |
| `list` | List all containers |
| `run -b <bundle> <id>` | Create and run (convenience) |
| `exec <id> <command>...` | Run a command in container's rootfs |
| `events --stats <id>` | Get container resource stats (CPU, memory) |

## Additional Options

### Environment Variables

```bash
# Set environment variables
litebox-oci run -b /bundle -e FOO=bar -e BAZ=qux my-container

# Load from file
litebox-oci run -b /bundle --env-file /path/to/envfile my-container
```

### Bind Mounts

Mount host directories into the container (loaded into in-memory filesystem):

```bash
# Mount a directory
litebox-oci run -b /bundle -m source=/host/data,destination=/data my-container

# Multiple mounts with short form
litebox-oci run -b /bundle \
  -m src=/host/config,dst=/etc/app \
  -m source=/host/data,target=/data,readonly \
  my-container
```

**Note:** Since litebox uses an in-memory filesystem, writes to mounted paths don't persist back to the host.

### Stdio Redirection

Redirect container stdout/stderr to files (useful for logging):

```bash
# Redirect stdout
litebox-oci run -b /bundle --stdout /var/log/container.out my-container

# Redirect both
litebox-oci run -b /bundle --stdout /var/log/out.log --stderr /var/log/err.log my-container
```

### Exec Command

Run additional commands using an existing container's rootfs:

```bash
# Run a command in container's rootfs
litebox-oci exec my-container /bin/ls /

# With environment and mounts
litebox-oci exec -e DEBUG=1 -m src=/tools,dst=/tools my-container /tools/script.sh
```

### Container Stats (Events)

Query resource usage statistics for a running container:

```bash
# Get current resource stats
litebox-oci events --stats my-container
```

Output format matches `runc events --stats`:

```json
{
  "type": "stats",
  "id": "my-container",
  "data": {
    "cpu": {
      "usage": {
        "total": 123456789,
        "kernel": 45678901,
        "user": 77777888
      }
    },
    "memory": {
      "usage": {
        "usage": 12345678
      }
    },
    "pids": {
      "current": 1
    }
  }
}
```

Stats are read from `/proc/<pid>/statm` (memory) and `/proc/<pid>/stat` (CPU time).

### Networking

Container networking works automatically with Podman (via CNI), `ctr --cni` (containerd), or manually with a TUN device.

#### Automatic (Podman or ctr — recommended)

The container manager sets up a CNI network namespace. litebox-oci auto-detects it and configures networking:

```bash
# Podman — no flags needed
sudo podman run --rm --runtime /usr/local/bin/litebox-oci \
  docker.io/library/alpine:latest /bin/ping -c 3 10.0.0.1

# containerd (ctr) — use --cni flag
sudo ctr run --rm --cni --runc-binary /usr/local/bin/litebox-oci \
  docker.io/library/alpine:latest test /bin/ping -c 3 10.0.0.1
```

**Architecture:** litebox-oci is a single process with a three-phase lifecycle:

1. **Setup** (privileged): create TUN device, configure kernel IP forwarding + NAT
   (via `ip` and `iptables`), open all required file descriptors
2. **Lock down**: install seccomp filter
3. **Execute**: load and run the guest in the same process and address space

```
Guest app → shim → smoltcp → TUN fd → kernel NAT → veth/eth0 → CNI bridge → network
```

After lockdown, the process can only `read`/`write`/`poll`/`futex` on pre-opened file descriptors.
Even if compromised, the attacker cannot open sockets or access network interfaces directly.

#### Manual (TUN Device)

For custom setups without CNI, create a TUN device on the host:

```bash
# Set up TUN device
sudo ip tuntap add dev tun99 mode tun
sudo ip addr add 10.0.0.1/24 dev tun99
sudo ip link set tun99 up

# Enable NAT for internet access
sudo sysctl -w net.ipv4.ip_forward=1
sudo iptables -t nat -A POSTROUTING -s 10.0.0.0/24 ! -o tun99 -j MASQUERADE

# Run with Podman (overrides auto-CNI)
sudo podman run --rm --runtime /usr/local/bin/litebox-oci \
  --runtime-flag='tun-device=tun99' alpine /bin/ping -c 3 10.0.0.1
```

**Supported:**
- TCP sockets (`socket`, `connect`, `bind`, `listen`, `accept`, `send`/`recv`)
- UDP sockets (`socket`, `bind`, `sendto`/`recvfrom`)
- ICMP ping (`SOCK_RAW`/`SOCK_DGRAM` + `IPPROTO_ICMP`)

**Not yet supported:**
- DNS resolution (needs `/etc/resolv.conf` in rootfs)
- Generic raw sockets (non-ICMP)

**Note:** All containers use smoltcp-internal IP `10.0.0.2`. With auto-CNI, kernel NAT translates this to the container's real CNI-assigned IP.

## How It Works

1. **Rootfs Loading**: Container filesystem is loaded into LiteBox's in-memory filesystem (with lazy loading option)
2. **Syscall Rewriting**: ELF binaries are patched to redirect syscalls to LiteBox (parallel rewriting, xxhash-cached)
3. **Dynamic Library Support**: `LD_AUDIT` mechanism patches shared libraries at load time
4. **Syscall Emulation**: All syscalls are intercepted and emulated by LiteBox

**Performance Optimizations:**
- Lazy executable rewriting (`--lazy-rewrite`): Only rewrites critical binaries upfront
- Tar indexing: O(1) file lookups instead of O(n) linear scan
- Parallel rewriting: Uses rayon for multi-core speedup
- xxhash caching: 10x faster than default hash for cache keys
- Memory-mapped tar cache: Zero-copy access to cached tar files

## Performance

### Quick Benchmarks (Cold Cache)

| Image | Size | Files | Lazy-rewrite | vs Eager |
|-------|------|-------|--------------|----------|
| Alpine | 8.7MB | 84 | **193ms** | 32% faster |
| Debian | 82MB | 3,264 | **172ms** | 72% faster |
| Ubuntu | 84MB | 2,587 | **164ms** | 70% faster |
| Python | 131MB | 4,944 | **252ms** | 66% faster |
| Node.js | 205MB | 5,667 | **980ms** | 31% faster |

*`--lazy-rewrite` is the fastest mode for all workloads. See [BENCHMARKS.md](BENCHMARKS.md) for details.*

### Binary Caching

Rewritten executables are cached in `~/.cache/litebox-oci/rewritten/`. Subsequent runs of the same container image are faster as cached binaries are loaded directly.

```bash
# Clear cache if needed
rm -rf ~/.cache/litebox-oci/
```

### Lazy Loading Modes

For large container images, lazy loading can significantly reduce startup time and memory usage:

```bash
# Lazy-rewrite mode: fastest - only critical executables rewritten upfront (RECOMMENDED)
litebox-oci run -b /bundle --lazy-rewrite my-container

# Lazy-tar mode: only loads executables, reads other files on-demand
litebox-oci run -b /bundle --lazy-tar my-container

# Default: eager mode (loads all files upfront)
litebox-oci run -b /bundle my-container

# Squashfs mode: uses loop-mounted squashfs (requires root)
litebox-oci run -b /bundle --lazy my-container
```

**Mode Comparison:**

| Mode | Cold Cache | Warm Cache | Best For |
|------|------------|------------|----------|
| `--lazy-rewrite` | **Fastest** | **Fastest** | All workloads (recommended) |
| `--lazy-tar` | Fast | Fast | Memory-constrained |
| Eager (default) | Slow | Moderate | Debugging, simple images |

**Why lazy-rewrite is fastest:**
- Only rewrites critical executables upfront (ld-linux, main binary)
- Other executables lazily rewritten when first accessed
- Most workloads only use a fraction of available executables
- Parallel rewriting with rayon for multi-core speedup
- xxhash for fast cache key computation

**Lazy-tar limitations:**
- All executables rewritten upfront (slower startup)
- File writes to read-only layer fail (use `PYTHONDONTWRITEBYTECODE=1` for Python)

### Advanced: ublk + Squashfs

For lowest overhead with repeated access to the same image, use kernel ublk block device:

```bash
# One-time setup (requires linux-modules-extra package)
sudo apt install linux-modules-extra-$(uname -r)
sudo modprobe ublk_drv
cargo install rublk

# Create and mount squashfs-backed block device
mksquashfs /path/to/rootfs /tmp/rootfs.squashfs
rublk add loop -f /tmp/rootfs.squashfs
sudo mount -t squashfs /dev/ublkb0 /mnt/rootfs

# Point bundle to mounted rootfs
litebox-oci run -b /bundle my-container
```

See [TODO.md](TODO.md) for detailed performance analysis and virtual block device options.

## Supported Features

- ✅ OCI runtime lifecycle (create/start/state/kill/delete)
- ✅ containerd integration via runc shim
- ✅ Rootless operation
- ✅ Basic process isolation via syscall emulation
- ✅ Environment variable injection (`--env`, `--env-file`)
- ✅ Bind mounts (`--mount`)
- ✅ Exec command for running commands in container rootfs
- ✅ Stdio redirection (`--stdout`, `--stderr`)
- ✅ Binary caching for faster subsequent runs
- ✅ Lazy file loading (`--lazy-tar`, `--lazy`)
- ✅ TUN-based networking (`--tun-device`)
- ✅ Virtual /proc filesystem (cpuinfo, meminfo, mounts, etc.)
- ✅ `chdir()` syscall and `process.cwd` from OCI spec
- ✅ Fork-free shell (litebox-sh) for container entrypoint scripts
- ✅ Automatic shell rewriting for fork-free compatibility (`--no-rewrite-shell` to disable)
- ✅ Pipeline orchestration (`echo hello | cat` works via sequential re-exec)
- ✅ Rootfs-aware symlink resolution (merged `/usr` layouts work — Debian, Ubuntu, Fedora)
- ✅ TTY support via console-socket (PTY master sent via SCM_RIGHTS per OCI spec)
- ✅ Podman integration (`--systemd-cgroup`, rootless state dir)

### Virtual /proc Filesystem

LiteBox emulates essential `/proc` files for container compatibility:

```bash
# CPU information
cat /proc/cpuinfo

# Memory statistics
cat /proc/meminfo

# Mount points (enables `df` command)
cat /proc/mounts
df -h

# Process information
cat /proc/self/status
cat /proc/self/cmdline

# System info
cat /proc/version
cat /proc/uptime
cat /proc/loadavg
```

**Supported /proc files:**
- `/proc/cpuinfo` - CPU information
- `/proc/meminfo` - Memory statistics
- `/proc/mounts` - Mount points
- `/proc/stat` - System statistics
- `/proc/version` - Kernel version string
- `/proc/uptime` - System uptime
- `/proc/loadavg` - Load averages
- `/proc/filesystems` - Supported filesystems
- `/proc/self/stat` - Process status
- `/proc/self/status` - Human-readable status
- `/proc/self/cmdline` - Command line
- `/proc/self/environ` - Environment variables
- `/proc/self/exe` - Executable path (symlink)
- `/proc/self/cwd` - Working directory (symlink)
- `/proc/self/maps` - Memory mappings
- `/proc/<pid>/*` - Same as /proc/self/*

## Process Model

LiteBox supports **pthreads**, **execve**, and **chdir**, but not **fork()**:

- ✅ Multi-threaded programs work (Go, Rust, Java, Python with threads)
- ✅ Direct command execution works (`/bin/ls`, `/usr/bin/python`)
- ✅ Shell builtins work (`echo`, `cd`, `pwd`, `export`, `test`, etc.)
- ✅ Shell `exec` works (`sh -c "export FOO=bar && exec /app/server"`)
- ✅ `process.cwd` from OCI spec sets initial working directory
- ✅ Pipes work via pipeline orchestration (`echo hello | cat`, `ls / | grep bin`)
- ❌ Background jobs (`&`), subshells (`$(...)`) not supported

**Shell support:** Alpine's `/bin/sh` (ash) works for builtin-only commands and
single external commands (the last command is exec'd). For more complex scripts,
use **litebox-sh**, a fork-free shell included in `litebox_runner_oci/tools/litebox-sh/`:

```bash
# Builtins + final exec (most common OCI entrypoint pattern)
litebox-sh -c 'export PATH=/app/bin:$PATH && cd /app && exec ./server'
```

See [litebox-sh README](tools/litebox-sh/README.md) for full feature list.

### Automatic Shell Rewriting

By default, LiteBox automatically rewrites shell entrypoints for fork-free compatibility:

1. **Shell replacement**: `sh -c "..."` → `litebox-sh -c "..."` (litebox-sh is injected into `/bin/litebox-sh`)
2. **Exec insertion**: `exec` is added before the final external command if not already present
3. **Script file args**: `sh /entrypoint.sh` → `litebox-sh /entrypoint.sh`
4. **Shebang rewriting**: `#!/bin/sh` → `#!/bin/litebox-sh` in script files during rootfs loading
5. **Pipeline orchestration**: `echo hello | cat` runs each pipe stage as a separate LiteBox process

```bash
# Original entrypoint:        sh -c "export FOO=bar && /app/server"
# After rewriting:   litebox-sh -c "export FOO=bar && exec /app/server"

# Script shebangs are also rewritten:
# #!/bin/sh → #!/bin/litebox-sh
# #!/bin/bash → #!/bin/litebox-sh
# #!/usr/bin/env sh → #!/bin/litebox-sh
```

This is safe and conservative — commands are never reordered or removed. Disable with `--no-rewrite-shell`:

```bash
litebox-oci run -b /bundle --no-rewrite-shell my-container
```

## Limitations

- ❌ x86_64 only (ARM64 not yet supported)
- ❌ No fork() syscall (pthreads, execve, and chdir work; automatic shell rewriting handles most entrypoint patterns)
- ❌ No per-container network isolation (all containers share same TUN IP)
- ❌ No cgroup resource limits
- ❌ Symlinks flattened to regular files (but directory symlinks and symlink chains are resolved correctly)
- ❌ Some syscalls unsupported (lgetxattr, listxattr)
- ❌ Mounts are read-only snapshots (writes don't persist to host)
- ⚠️ Background jobs (`&`) and subshells (`$(...)`) not supported
- ⚠️ Interactive shells hang (stdin polling not yet supported; `bash` prints prompt but `pselect` on stdin doesn't wake)

## Documentation

- [instruction.md](instruction.md) - Manual testing guide
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) - Technical architecture
- [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) - Development guide
- [docs/OCI_FEATURES.md](docs/OCI_FEATURES.md) - OCI feature support matrix
- [docs/TROUBLESHOOTING.md](docs/TROUBLESHOOTING.md) - Common issues and solutions
- [tools/litebox-sh/README.md](tools/litebox-sh/README.md) - Fork-free shell for LiteBox

## License

MIT License - see [LICENSE](../LICENSE) for details.
