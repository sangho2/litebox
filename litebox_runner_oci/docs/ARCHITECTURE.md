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
│  │  kill       │  │             │  │  shell rewriting        │  │
│  │  delete     │  │             │  │  program execution      │  │
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
2. Shell Rewriting (if enabled)
        │
        ├──▶ Layer 1: sh/bash/dash → litebox-sh
        ├──▶ Layer 2: Add exec before final external command
        └──▶ Layer 3: sh /script.sh → litebox-sh /script.sh
        │
        ▼
3. Initialize LiteBox platform
        │
        ▼
4. Create in-memory filesystem
        │
        ▼
5. Walk rootfs directory
        │
        ├──▶ Directory: mkdir() in sandbox
        │
        ├──▶ Executable: rewrite syscalls, load into sandbox
        │
        ├──▶ Script file: rewrite shebang (#!/bin/sh → #!/bin/litebox-sh)
        │
        └──▶ Symlink: resolve and copy target
                │
                ▼
6. Inject litebox-sh into /bin/litebox-sh
        │
        ▼
7. Detect script entrypoints (shebang → prepend litebox-sh)
        │
        ▼
8. Add rtld_audit.so for dynamic libraries
        │
        ▼
9. Build LiteBox shim, resolve program path
        │
        ▼
10. Load and run program through LiteBox
```

## Shell Rewriting

LiteBox automatically rewrites shell entrypoints for fork-free compatibility.
This is a 3-layer system applied during container setup:

```
                    OCI config.json args
                           │
                           ▼
              ┌────────────────────────┐
              │  Layer 1: Shell Name   │
              │  sh/bash/dash/ash      │──▶ /bin/litebox-sh
              └────────────────────────┘
                           │
                           ▼
              ┌────────────────────────┐
              │  Layer 2: Exec Insert  │
              │  -c "... && cmd"       │──▶ -c "... && exec cmd"
              └────────────────────────┘
                           │
                           ▼
              ┌────────────────────────┐
              │  Layer 3: Script File  │
              │  sh /script.sh         │──▶ litebox-sh /script.sh
              └────────────────────────┘

  During rootfs loading:
  ┌───────────────────────────────┐
  │  Shebang Rewriting            │
  │  #!/bin/sh    → #!/bin/litebox-sh  │
  │  #!/bin/bash  → #!/bin/litebox-sh  │
  └───────────────────────────────┘

  At program load:
  ┌───────────────────────────────┐
  │  Entrypoint Detection         │
  │  ./script.sh (has #!/bin/sh)  │──▶ litebox-sh ./script.sh
  └───────────────────────────────┘
```

litebox-sh is a Rust binary statically linked with musl (~435KB), embedded
via `include_bytes!` and injected into `/bin/litebox-sh` in the rootfs.
Disable all rewriting with `--no-rewrite-shell`.

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
| Network | Netstack | smoltcp (TUN-based) |
| Shell support | Native (has fork) | litebox-sh + auto-rewriting |
| Platform | Sentry kernel | LiteBox shim |
| OCI compliance | Full | Basic lifecycle |

## Limitations

1. **No network namespace**: Containers share host network (TUN-based networking available)
2. **No cgroup limits**: Resource limits not enforced
3. **No seccomp filters**: Uses rewriter instead
4. **No user namespaces**: Runs as invoking user
5. **x86_64 only**: rtld_audit.so is architecture-specific
6. **No symlinks**: Flattened during rootfs loading
7. **No fork()**: Mitigated by automatic shell rewriting (litebox-sh)
