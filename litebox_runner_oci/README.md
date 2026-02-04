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

**Binary Caching**: Rewritten executables are cached in `~/.cache/litebox-oci/rewritten/`. Subsequent runs of the same container image are ~3-4x faster as cached binaries are loaded directly.

```bash
# First run (no cache): ~50ms
# Second run (cached):  ~14ms

# Clear cache if needed
rm -rf ~/.cache/litebox-oci/rewritten/
```

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
- ✅ TUN-based networking (`--tun-device`)

## Limitations

- ❌ x86_64 only (ARM64 not yet supported)
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
