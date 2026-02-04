# LiteBox OCI Runtime - Architecture

## System Overview

```
┌─────────────────────────────────────────────────────────────────┐
│                         User Request                            │
│                    (docker run, ctr run, etc.)                  │
└─────────────────────────────────────────────────────────────────┘
                                │
                                ▼
┌─────────────────────────────────────────────────────────────────┐
│                        containerd                               │
│                   (container manager)                           │
└─────────────────────────────────────────────────────────────────┘
                                │
                                ▼
┌─────────────────────────────────────────────────────────────────┐
│                   containerd-shim-runc-v2                       │
│                      (process manager)                          │
└─────────────────────────────────────────────────────────────────┘
                                │
                                ▼
┌─────────────────────────────────────────────────────────────────┐
│                       litebox-oci                               │
│                    (OCI runtime CLI)                            │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────────┐  │
│  │  lifecycle  │  │    state    │  │         runner          │  │
│  │  create     │  │  load/save  │  │  rootfs loading         │  │
│  │  start      │  │  refresh    │  │  syscall rewriting      │  │
│  │  kill       │  │             │  │  program execution      │  │
│  │  delete     │  │             │  │                         │  │
│  └─────────────┘  └─────────────┘  └─────────────────────────┘  │
└─────────────────────────────────────────────────────────────────┘
                                │
                                ▼
┌─────────────────────────────────────────────────────────────────┐
│                         LiteBox                                 │
│                  (userspace sandbox)                            │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │                  In-Memory Filesystem                    │    │
│  │    /bin/echo  /lib/libc.so  /etc/passwd  ...            │    │
│  └─────────────────────────────────────────────────────────┘    │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │                 Syscall Emulation                        │    │
│  │    open() read() write() stat() mmap() ...              │    │
│  └─────────────────────────────────────────────────────────┘    │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │                Platform Abstraction                      │    │
│  │    Linux userland / LVBS / OP-TEE / ...                 │    │
│  └─────────────────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────────────────┘
```

## Component Details

### 1. CLI Layer (`main.rs`)

Handles OCI runtime commands and containerd compatibility flags:

```rust
// Global flags for containerd compatibility
--root <path>       // State directory
--log <path>        // Log file (accepted, logs to stderr)
--log-format <fmt>  // Log format (accepted, ignored)

// OCI lifecycle commands
create -b <bundle> [--pid-file <path>] <container-id>
start <container-id>
state <container-id>
kill <container-id> [signal]
delete [--force] <container-id>
list

// Convenience commands
run -b <bundle> <container-id>
info
```

### 2. Lifecycle Manager (`lifecycle.rs`)

Implements OCI container lifecycle using fork + Unix socket synchronization:

```
create:
┌─────────────────┐     fork      ┌─────────────────┐
│     Parent      │──────────────▶│     Child       │
│                 │               │                 │
│  1. Fork child  │               │  1. Create      │
│  2. Wait for    │◀──"R"────────│     UnixListener│
│     ready       │               │  2. Signal ready│
│  3. Save state  │               │  3. Wait for    │
│  4. Return PID  │               │     connection  │
└─────────────────┘               └────────┬────────┘
                                           │ (blocked)
start:                                     │
┌─────────────────┐    connect    ┌────────▼────────┐
│  start command  │──────────────▶│     Child       │
│                 │               │                 │
│  1. Connect to  │───"S"────────▶│  4. Receive "S" │
│     socket      │               │  5. exec() into │
│  2. Send "S"    │               │     container   │
│  3. Update state│               │                 │
└─────────────────┘               └─────────────────┘
```

### 3. State Manager (`state.rs`)

Persists container state to disk:

```json
{
  "ociVersion": "1.0.0",
  "id": "my-container",
  "status": "running",
  "pid": 12345,
  "bundle": "/path/to/bundle"
}
```

State directory structure:
```
/run/litebox-oci/
└── containers/
    └── <container-id>/
        ├── state.json
        └── sync.pipe
```

### 4. Container Runner (`runner.rs`)

Core execution logic:

```
1. Parse config.json (OCI spec)
        │
        ▼
2. Initialize LiteBox platform
        │
        ▼
3. Create in-memory filesystem
        │
        ▼
4. Walk rootfs directory
        │
        ├──▶ Directory: mkdir() in sandbox
        │
        ├──▶ Regular file:
        │       │
        │       ├── If executable: rewrite syscalls
        │       │
        │       └── Load into sandbox fs
        │
        └──▶ Symlink: resolve and copy target
                │
                ▼
5. Add rtld_audit.so for dynamic libraries
        │
        ▼
6. Build LiteBox shim
        │
        ▼
7. Resolve program path (search PATH)
        │
        ▼
8. Load and run program through LiteBox
```

## Syscall Interception

### Rewriter Backend

```
Original binary:                  Rewritten binary:
┌─────────────────┐              ┌─────────────────┐
│ ...             │              │ ...             │
│ mov rax, 1      │              │ mov rax, 1      │
│ syscall    ◀────┼──rewrite────▶│ jmp trampoline  │
│ ...             │              │ ...             │
└─────────────────┘              └─────────────────┘
                                          │
                                          ▼
                                 ┌─────────────────┐
                                 │   trampoline    │
                                 │                 │
                                 │ → LiteBox shim  │
                                 │ → emulate       │
                                 │ → return        │
                                 └─────────────────┘
```

### Dynamic Library Support

For dynamically-linked binaries, `LD_AUDIT` is used:

```
1. Binary loads
        │
        ▼
2. ld.so sees LD_AUDIT=/lib/litebox_rtld_audit.so
        │
        ▼
3. rtld_audit.so patches each loaded library
        │
        ▼
4. All syscalls redirected to LiteBox
```

## containerd Integration

LiteBox OCI works with containerd's existing runc shim:

```
containerd
    │
    ▼
io.containerd.runc.v2 shim
    │
    │  (expects runc-compatible CLI)
    │
    ▼
litebox-oci --root /run/... create -b /bundle --pid-file /pid <id>
litebox-oci --root /run/... start <id>
litebox-oci --root /run/... kill <id> SIGTERM
litebox-oci --root /run/... delete <id>
```

Key compatibility requirements:
- Accept `--log` and `--log-format` flags
- Accept `-b` as short form for `--bundle`
- Accept `--pid-file` and write PID to it
- Accept `--no-pivot` and `--no-new-keyring` (ignored)
- Output state as JSON to stdout for `state` command

## Comparison with gVisor

| Aspect | gVisor (runsc) | LiteBox OCI |
|--------|----------------|-------------|
| Language | Go | Rust |
| Syscall interception | ptrace/KVM | Syscall rewriting |
| Filesystem | Gofer (9P) | In-memory |
| Network | Netstack | Not implemented |
| Platform | Sentry kernel | LiteBox shim |
| OCI compliance | Full | Basic lifecycle |

## Limitations

1. **No network namespace**: Containers share host network
2. **No cgroup limits**: Resource limits not enforced
3. **No seccomp filters**: Uses rewriter instead
4. **No user namespaces**: Runs as invoking user
5. **x86_64 only**: rtld_audit.so is architecture-specific
6. **No symlinks**: Flattened during rootfs loading
