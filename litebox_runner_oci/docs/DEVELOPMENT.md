# LiteBox OCI Runtime - Development Guide

## Overview

`litebox_runner_oci` is an OCI-compliant container runtime that uses LiteBox's userspace syscall emulation instead of traditional Linux namespaces and cgroups. This approach is similar to gVisor's architecture but implemented in Rust.

## Architecture

### How It Differs from Traditional Runtimes

Traditional OCI runtimes (runc, crun, youki):
- Use Linux namespaces for isolation
- Use cgroups for resource limits
- Call `pivot_root` to change the container's root filesystem
- Execute the container process natively

LiteBox OCI runtime:
- Uses userspace syscall emulation for isolation
- Loads rootfs into an in-memory filesystem
- Rewrites syscall instructions in ELF binaries
- Intercepts and emulates syscalls at runtime

### Why Not Use youki's libcontainer?

We initially attempted to integrate with youki's `Executor` trait, but discovered that youki performs `pivot_root` and Linux namespace setup *before* calling the executor. This fundamentally conflicts with LiteBox's approach:

1. youki creates real Linux namespaces and mounts
2. youki calls `pivot_root` to change the root filesystem
3. Only then does youki call `Executor::exec()`

LiteBox needs to:
1. Load files into its in-memory filesystem
2. Rewrite syscalls in executables
3. Run the process through syscall emulation

These approaches are incompatible, so we built a custom OCI layer using only the `oci-spec` crate for config parsing.

### Key Components

```
litebox_runner_oci/
├── src/
│   ├── main.rs       # CLI entry point
│   ├── lib.rs        # Public API
│   ├── runner.rs     # Core container execution
│   ├── state.rs      # Container state persistence
│   └── lifecycle.rs  # OCI lifecycle (create/start/kill/delete)
├── scripts/
│   ├── bundle.py     # Bundle creation utility
│   └── integration_test.sh
└── docs/
    ├── DEVELOPMENT.md    # This file
    └── ARCHITECTURE.md   # Technical details
```

## Building

```bash
# Debug build
cargo build -p litebox_runner_oci

# Release build
cargo build -p litebox_runner_oci --release

# Install
sudo cp target/release/litebox_runner_oci /usr/local/bin/litebox-oci
```

## Technical Details

### Syscall Rewriting

LiteBox uses a "rewriter" backend that:
1. Scans ELF binaries for syscall instructions
2. Rewrites them to jump to a trampoline
3. Uses `LD_AUDIT=/lib/litebox_rtld_audit.so` to patch dynamic libraries at load time

The seccomp backend exists but is currently marked as ignored in tests due to compatibility issues.

### Rootfs Loading

The `runner.rs` module:
1. Walks the OCI rootfs directory
2. Loads each file into LiteBox's in-memory filesystem
3. Rewrites syscalls in executable files
4. Flattens symlinks (LiteBox doesn't support symlinks)

### OCI Lifecycle

The lifecycle follows the OCI runtime specification:

```
(none) --create--> created --start--> running --exit--> stopped
                                \--kill--> stopped
```

Implementation uses fork + Unix socket synchronization:
- `create`: Fork child, child waits on Unix socket, parent returns PID
- `start`: Connect to child's socket, send signal to exec
- `state`: Read state.json from disk
- `kill`: Send signal to PID
- `delete`: Remove state directory

### State Storage

Container state is stored in JSON format:

```
/run/litebox-oci/containers/<container-id>/
├── state.json     # Container state
└── sync.pipe      # Unix socket for create/start sync
```

## Known Limitations

1. **x86_64 only**: The rtld_audit.so library is architecture-specific
2. **No symlink support**: Symlinks are flattened to regular files
3. **Large rootfs slow**: Loading thousands of files takes time
4. **Some syscalls unsupported**: lgetxattr, listxattr, etc. (warnings only)
5. **No TTY support**: console-socket flag is accepted but not implemented

## Testing

### Manual Testing

See `instruction.md` for step-by-step manual testing guide.

### Automated Testing

```bash
# Run integration tests (builds and installs first)
./scripts/integration_test.sh --build

# Run tests only (assumes already installed)
./scripts/integration_test.sh
```

### containerd Integration

```bash
# Run via containerd
sudo ctr run --rm --runc-binary /usr/local/bin/litebox-oci \
    docker.io/library/alpine:latest test-container \
    /bin/echo "Hello"
```

## Recent Additions

- **exec command**: Run commands in a container's rootfs
- **--env / --env-file**: Inject environment variables at runtime
- **--mount**: Bind mount host directories into container (snapshot)
- **--stdout / --stderr**: Redirect container output to files
- **Binary caching**: Rewritten ELF binaries are cached for ~3-4x faster subsequent runs
- **--tun-device**: Enable TUN-based networking (TCP/UDP supported)

## Networking

TUN-based networking connects containers to the host network:

```bash
# Set up TUN device on host (requires root)
sudo litebox_platform_linux_userland/scripts/tun-setup.sh -t tun99 -i 10.0.0.1

# Run container with networking
litebox-oci run -b /bundle --tun-device tun99 my-container
```

- Container IP: `10.0.0.2/24`
- Gateway: `10.0.0.1`
- Supported: TCP and UDP sockets
- Not supported: Raw sockets (ping won't work)

## Performance

Rewritten binaries are cached in `~/.cache/litebox-oci/rewritten/` using content hashing. This significantly speeds up repeated container runs:

| Scenario | Time |
|----------|------|
| First run (cold cache) | ~50ms |
| Subsequent runs (cached) | ~14ms |

The cache is automatically invalidated when binary content changes.

## Future Work

- [ ] Implement console-socket for TTY support
- [ ] Kubernetes/CRI-O integration testing
- [ ] Lazy file loading (load on first access)
- [ ] Raw socket support (ICMP/ping)
- [ ] ARM64 support for rtld_audit.so
- [ ] Per-container network isolation

## References

- [OCI Runtime Specification](https://github.com/opencontainers/runtime-spec)
- [gVisor Architecture](https://gvisor.dev/docs/architecture_guide/)
- [youki](https://github.com/containers/youki)
- [containerd](https://containerd.io/)
