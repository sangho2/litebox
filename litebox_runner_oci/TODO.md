# LiteBox OCI Runtime - TODO

## Completed

- [x] OCI config.json parsing via oci-spec crate
- [x] Rootfs loading into LiteBox's in-memory filesystem
- [x] Symlink resolution (flattened to regular files)
- [x] Syscall rewriting for executables
- [x] rtld_audit.so inclusion for dynamic library support
- [x] PATH resolution for command lookup
- [x] Full OCI lifecycle: create, start, state, kill, delete, list
- [x] containerd compatibility flags
- [x] Successful containerd integration test
- [x] bundle.py for binary and image bundling
- [x] integration_test.sh for automated testing
- [x] Rootless operation support
- [x] Manual testing instructions (instruction.md)
- [x] Clippy warnings fixed
- [x] `--version` output with git commit hash
- [x] `exec` command for running commands in containers
- [x] Improved error messages for common failures
- [x] Unit tests for state.rs (18 tests)
- [x] Unit tests for lifecycle.rs (18 tests)
- [x] README.md with quick start guide
- [x] Document supported/unsupported OCI features (OCI_FEATURES.md)
- [x] Troubleshooting guide (TROUBLESHOOTING.md)
- [x] `--env` and `--env-file` flags
- [x] `--mount` for additional bind mounts
- [x] `--stdout` and `--stderr` for stdio redirection
- [x] Cache rewritten binaries for faster subsequent runs

## In Progress

*None - all items completed or moved to future work*

## Short-term TODO

### CLI Improvements
- [x] Add `--version` output with git commit hash
- [x] Add `exec` command for running commands in existing containers
- [x] Improve error messages for common failures

### Testing
- [x] Add unit tests for state.rs
- [x] Add unit tests for lifecycle.rs
- [ ] Test with more container images (debian, ubuntu, fedora)
- [ ] Test with multi-process workloads

### Documentation
- [x] Add README.md with quick start guide
- [x] Document supported/unsupported OCI features
- [x] Add troubleshooting guide

## Medium-term TODO

### Features
- [ ] Implement console-socket for TTY support
- [x] Add proper stdio redirection for container logs
- [x] Support `--env` and `--env-file` flags
- [x] Support `--mount` for additional bind mounts
- [ ] Implement `events` command for container metrics

### Performance
- [ ] Optimize rootfs loading for large images
- [ ] Lazy file loading (load on first access)
- [x] Cache rewritten binaries

### Compatibility
- [ ] Kubernetes/CRI-O integration testing
- [ ] Podman integration testing
- [ ] Docker integration via containerd

## Long-term TODO

### Architecture
- [ ] ARM64 support (requires rtld_audit.so port)
- [ ] Network namespace emulation
- [ ] Support for more syscalls
- [ ] seccomp backend improvements

### Security
- [ ] Audit syscall emulation for security gaps
- [ ] Add sandboxing for the runtime itself
- [ ] Support for capabilities

### Advanced Features
- [ ] Checkpoint/restore support
- [ ] Live migration
- [ ] GPU passthrough
- [ ] Nested container support

## Known Issues

1. **Large rootfs slow**: Loading python:3.11-slim (~5000 files) takes several seconds
2. **Some syscalls unsupported**: lgetxattr, listxattr show warnings but don't break execution
3. ~~**Timestamps wrong**: Files show "Jan 1 1970" due to incomplete time syscall support~~ *Fixed: Now defaults to 2024-01-01*
4. **whoami fails**: Needs proper /etc/passwd and utmp support
5. **Alpine cleanup segfault**: Sometimes segfaults during cleanup (doesn't affect execution)

## Notes

### Why not use youki's libcontainer?

youki performs `pivot_root` and Linux namespace setup BEFORE calling the Executor trait. This fundamentally conflicts with LiteBox's approach of:
1. Loading files into in-memory filesystem
2. Rewriting syscalls in executables
3. Running through syscall emulation

### Backend choice

Must use "rewriter" backend, not "seccomp". The seccomp backend tests are marked `#[ignore]` in LiteBox with a note about needing modifications.

### containerd integration

Uses existing `io.containerd.runc.v2` shim - no custom shim needed. Critical flags:
- `--log` and `--log-format` (accepted for compatibility)
- `-b` (short for --bundle)
- `--pid-file` (write PID for shim)
