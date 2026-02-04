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
| `cwd` | ⚠️ Partial | Parsed but not enforced |
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
- File operations: `open`, `read`, `write`, `close`, `stat`, `fstat`, `lstat`
- Directory operations: `mkdir`, `rmdir`, `getdents`
- Process operations: `fork`, `execve`, `exit`, `wait`
- Memory operations: `mmap`, `munmap`, `brk`
- Misc: `getcwd`, `chdir`, `uname`

### Partially Supported
- `ioctl` - Limited terminal ioctls
- `fcntl` - Basic operations only

### Not Supported (warnings only)
- `lgetxattr`, `listxattr` - Extended attributes
- `inotify_*` - File watching
- Network syscalls - No network emulation

## Architectural Limitations

### x86_64 Only
The `rtld_audit.so` library for dynamic library syscall interception is x86_64-specific. ARM64 support requires porting this library.

### No Symlinks
Symlinks in the rootfs are resolved and flattened to regular files during loading. This is because LiteBox's in-memory filesystem doesn't support symlinks.

### No Network Isolation
Containers share the host's network stack. Network namespace emulation is not implemented.

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
