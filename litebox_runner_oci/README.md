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
- Alpine, Debian, Ubuntu, Fedora images

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

### Networking (TUN Device)

Enable container networking using a TUN device:

```bash
# First, set up a TUN device on the host (requires root)
sudo litebox_platform_linux_userland/scripts/tun-setup.sh -t tun99 -i 10.0.0.1

# Run container with networking enabled
litebox-oci run -b /bundle --tun-device tun99 my-container

# Container will have IP 10.0.0.2/24, gateway 10.0.0.1
```

**Requirements:**
- TUN device must be pre-created on the host
- Container gets IP `10.0.0.2/24` (hardcoded in LiteBox)
- Gateway is `10.0.0.1` (host side of TUN)

**Supported:**
- TCP sockets (`socket`, `connect`, `bind`, `listen`, `accept`, `send`/`recv`)
- UDP sockets (`socket`, `bind`, `sendto`/`recvfrom`)

**Not supported:**
- Raw sockets (ICMP ping)
- `/proc/net/*` files
- Some edge cases in socket state transitions

**Setup host NAT for internet access:**

```bash
# Enable IP forwarding
sudo sysctl -w net.ipv4.ip_forward=1

# NAT container traffic
sudo iptables -t nat -A POSTROUTING -s 10.0.0.0/24 -o eth0 -j MASQUERADE
sudo iptables -A FORWARD -i tun99 -o eth0 -j ACCEPT
sudo iptables -A FORWARD -i eth0 -o tun99 -m state --state RELATED,ESTABLISHED -j ACCEPT
```

**Note:** All containers using the same TUN device share the IP `10.0.0.2`. There's no per-container network isolation.

## How It Works

1. **Rootfs Loading**: Container filesystem is loaded into LiteBox's in-memory filesystem
2. **Syscall Rewriting**: ELF binaries are patched to redirect syscalls to LiteBox (cached for performance)
3. **Dynamic Library Support**: `LD_AUDIT` mechanism patches shared libraries at load time
4. **Syscall Emulation**: All syscalls are intercepted and emulated by LiteBox

## Performance

### Quick Benchmarks

| Image | Size | Startup Time | Notes |
|-------|------|--------------|-------|
| Alpine (echo) | 12MB | 0.23-0.27s | Lazy-tar fastest |
| Python hello | 74MB | 0.13s | Eager fastest |
| Python 3.11-slim | 130MB | 0.38s | Debian-based |
| Large data | 574MB | 0.37s | Memory-efficient |

*Benchmarks on Ubuntu 24.04 Azure VM. See [BENCHMARKS.md](BENCHMARKS.md) for comprehensive results.*

### Binary Caching

Rewritten executables are cached in `~/.cache/litebox-oci/rewritten/`. Subsequent runs of the same container image are faster as cached binaries are loaded directly.

```bash
# Clear cache if needed
rm -rf ~/.cache/litebox-oci/rewritten/
```

### Lazy Loading Modes

For large container images, lazy loading can significantly reduce startup time and memory usage:

```bash
# Default: eager mode (loads all files upfront)
litebox-oci run -b /bundle my-container

# Lazy-tar mode: only loads executables, reads other files on-demand
litebox-oci run -b /bundle --lazy-tar my-container

# Squashfs mode: uses loop-mounted squashfs (requires root)
litebox-oci run -b /bundle --lazy my-container
```

**Performance by image type:**

| Image | Size | Eager | Lazy-tar | Best Mode |
|-------|------|-------|----------|-----------|
| Alpine (simple) | 12MB | 0.27s | **0.23s** | Lazy-tar |
| Python (complex) | 74MB | **0.13s** | 0.54s | Eager |
| Large data | 174MB | **0.18s** | 0.60s | Eager |

**When to use each mode:**
- **`--lazy-tar`**: Simple containers (busybox, Alpine), memory-constrained environments
- **Default (eager)**: Complex containers with many libraries (Python, Node, Go)
- **ublk+squashfs**: Repeated access to same image, lowest overhead after setup

**Lazy-tar benefits:**
- Faster startup for simple images (fewer file lookups)
- Lower memory usage (only accessed files loaded)
- No external dependencies

**Lazy-tar limitations:**
- Slower for complex images due to tar O(n) parsing overhead
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

LiteBox supports **pthreads** and **execve**, but not **fork()**:

- ✅ Multi-threaded programs work (Go, Rust, Java, Python with threads)
- ✅ Direct command execution works (`/bin/ls`, `/usr/bin/python`)
- ❌ Shell scripts calling external commands fail (`sh -c "ls"` needs fork)
- ❌ Traditional fork-then-exec patterns don't work

**Workaround:** Run commands directly instead of through shell.

## Limitations

- ❌ x86_64 only (ARM64 not yet supported)
- ❌ No fork() syscall (pthreads and execve work)
- ❌ No per-container network isolation (all containers share same TUN IP)
- ❌ No cgroup resource limits
- ❌ Symlinks flattened to regular files
- ❌ Some syscalls unsupported (lgetxattr, listxattr)
- ❌ Mounts are read-only snapshots (writes don't persist to host)

## Documentation

- [instruction.md](instruction.md) - Manual testing guide
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) - Technical architecture
- [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) - Development guide
- [docs/OCI_FEATURES.md](docs/OCI_FEATURES.md) - OCI feature support matrix
- [docs/TROUBLESHOOTING.md](docs/TROUBLESHOOTING.md) - Common issues and solutions

## License

MIT License - see [LICENSE](../LICENSE) for details.
