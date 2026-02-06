# litebox_runner_linux_userland

Run Linux programs in a sandboxed environment using LiteBox on unmodified Linux systems.

## Overview

This runner executes Linux binaries within LiteBox's sandboxed filesystem and syscall interception layer, without requiring kernel modifications or privileged containers.

## Building

```bash
cargo build --release -p litebox_runner_linux_userland
cargo build --release -p litebox_syscall_rewriter  # Required for rewriter backend
```

## Usage

### Direct Execution

```bash
# Run with seccomp-based syscall interception
./target/release/litebox_runner_linux_userland -Z \
    --interception-backend seccomp \
    --initial-files rootfs.tar \
    /path/to/binary [args...]

# Run with syscall rewriting (requires pre-rewritten binaries)
./target/release/litebox_runner_linux_userland -Z \
    --interception-backend rewriter \
    --initial-files libs.tar \
    /path/to/rewritten-binary [args...]
```

### Using bundle.py (Recommended)

The `bundle.py` script simplifies the process of preparing binaries for execution:

#### Bundle a Host Binary

```bash
# Prepare echo command
./bundle.py --binary /usr/bin/echo --output-dir /tmp/litebox-echo

# Run it
/tmp/litebox-echo/run.sh "Hello from LiteBox!"
```

#### Bundle a Container Image

```bash
# Prepare Alpine Linux
./bundle.py --image alpine:latest --output-dir /tmp/litebox-alpine

# Run commands from the container
/tmp/litebox-alpine/run.sh /bin/echo "Hello from Alpine!"
/tmp/litebox-alpine/run.sh /bin/cat /etc/os-release
/tmp/litebox-alpine/run.sh /bin/ls /
```

### bundle.py Options

```
usage: bundle.py [-h] (--binary BINARY | --image IMAGE) --output-dir OUTPUT_DIR
                 [--rewriter-path REWRITER_PATH]

Create bundles for litebox_runner_linux_userland

options:
  --binary BINARY       Local binary path
  --image IMAGE         Container image (e.g., alpine:latest)
  --output-dir, -o      Output directory (required)
  --rewriter-path       Path to litebox_syscall_rewriter (auto-detected)
```

### Requirements for bundle.py

- Python 3.6+
- `litebox_syscall_rewriter` binary (built from this repo)
- For container images: `skopeo` and `umoci`

```bash
# Install container tools (Ubuntu/Debian)
sudo apt install skopeo umoci
```

## Bundle Contents

### Host Binary Bundle

```
/tmp/litebox-echo/
├── echo.hooked     # Rewritten binary
├── libs.tar        # Libraries at original paths
└── run.sh          # Execution script
```

### Container Image Bundle

```
/tmp/litebox-alpine/
├── rootfs.tar      # Complete rewritten filesystem
└── run.sh          # Execution script
```

## CLI Options

| Option | Description |
|--------|-------------|
| `-Z, --unstable` | Enable unstable options |
| `--interception-backend` | `seccomp` or `rewriter` |
| `--initial-files PATH` | Tar file with initial filesystem contents |
| `--env KEY=VALUE` | Set environment variable (repeatable) |
| `--forward-env` | Forward host environment variables |
| `--tun-device-name` | TUN device for networking |

## Technical Notes

### Tar File Format

The `--initial-files` tar must:
- Use GNU or USTAR format (not PAX)
- Include directory entries before files
- Place libraries at their original absolute paths (e.g., `lib64/ld-linux-x86-64.so.2`)

### Interception Backends

- **seccomp**: Uses seccomp-bpf to intercept syscalls at runtime. Works with unmodified binaries.
- **rewriter**: Rewrites syscall instructions in ELF binaries. Lower overhead but requires preprocessing.

### Busybox Compatibility

When bundling busybox-based images (Alpine, etc.), the runner preserves the binary name in `argv[0]` so applet detection works correctly.

## Testing

```bash
cargo test -p litebox_runner_linux_userland
```

## See Also

- [litebox_runner_oci](../litebox_runner_oci/) - OCI-compatible container runtime
- [litebox_syscall_rewriter](../litebox_syscall_rewriter/) - ELF syscall rewriting tool
