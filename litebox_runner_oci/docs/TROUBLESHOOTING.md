# Troubleshooting Guide

Common issues and solutions when using litebox-oci.

## Shell Scripts and fork() Limitation

### "can't fork: Function not implemented"

**Problem:** Shell scripts that run external commands fail with fork error.

**Cause:** LiteBox doesn't support `fork()` syscall. Standard shells need fork to run external commands.

**Automatic mitigation:** Shell rewriting (enabled by default) handles most patterns:
- `sh -c "..."` → `litebox-sh -c "..."` (no fork needed)
- `sh /script.sh` → `litebox-sh /script.sh`
- `#!/bin/sh` shebangs rewritten to `#!/bin/litebox-sh`
- `./script.sh` detected and routed through litebox-sh

If you still see fork errors, the pattern may be beyond litebox-sh's capabilities (e.g., pipes, subshells).

**Disable rewriting:** `--no-rewrite-shell` to use the original shell.

**What works (with or without rewriting):**
```bash
# Direct execution (no shell)
args: ["/bin/ls", "/"]

# Shell built-ins only
args: ["/bin/sh", "-c", "echo hello; pwd; echo $((1+2))"]

# Use exec for ONE external command (replaces shell)
args: ["/bin/sh", "-c", "VAR=setup; exec /bin/echo $VAR"]
```

**What works (with rewriting — automatic):**
```bash
# Shell -c with external commands
args: ["sh", "-c", "ls /"]

# Chains with builtins and externals
args: ["sh", "-c", "export FOO=bar && echo $FOO"]

# Script file execution
args: ["sh", "/entrypoint.sh"]

# Direct script execution (shebang-based)
args: ["/entrypoint.sh"]
```

**What still fails (even with rewriting):**
```bash
# Multiple external commands
args: ["/bin/sh", "-c", "ls /; cat /etc/passwd"]  # FAILS

# Pipes
args: ["/bin/sh", "-c", "ls | grep bin"]  # FAILS

# Command substitution
args: ["/bin/sh", "-c", "echo $(date)"]  # FAILS
```

**Workarounds (if rewriting is insufficient):**

1. **Automatic shell rewriting** (default) — handles most patterns. If something still fails, check if the pattern involves pipes or subshells.

2. **Direct execution** - Run commands directly without shell wrapper:
   ```json
   // Instead of: ["sh", "-c", "ls -la /"]
   // Use:
   {"args": ["/bin/ls", "-la", "/"]}
   ```

2. **Use shell built-ins** - `echo`, `pwd`, `cd`, `export`, `[`, `test`, arithmetic `$((...))`:
   ```json
   {"args": ["/bin/sh", "-c", "echo Hello; pwd; [ -f /etc/passwd ] && echo exists"]}
   ```

3. **exec for final command** - Use `exec` to run one external command:
   ```json
   {"args": ["/bin/sh", "-c", "export VAR=value; exec /bin/myapp"]}
   ```

4. **Multiple container runs** - Run commands separately:
   ```bash
   litebox-oci run -b /bundle c1 -- /bin/ls /
   litebox-oci run -b /bundle c2 -- /bin/cat /etc/passwd
   ```

## Container Creation Issues

### "container already exists"

**Problem:** A container with the same ID already exists.

**Solution:**
```bash
litebox-oci delete --force <container-id>
```

### "config.json not found in bundle"

**Problem:** The bundle directory doesn't contain a valid OCI config.

**Solution:**
1. Verify the bundle path is correct
2. Ensure `config.json` exists in the bundle directory
3. Check it's valid JSON: `cat /path/to/bundle/config.json | jq .`

### "rootfs not found"

**Problem:** The rootfs directory specified in config.json doesn't exist.

**Solution:**
1. Check the `root.path` in config.json (defaults to `rootfs`)
2. Ensure the rootfs directory exists and contains files
3. For container images, use `umoci unpack` to extract the rootfs

## Container Start Issues

### "failed to connect to container sync socket"

**Problem:** The container process exited before `start` was called.

**Causes:**
- The forked process crashed during initialization
- Container was killed between `create` and `start`

**Solution:**
```bash
# Delete the stale container
litebox-oci delete --force <container-id>

# Recreate it
litebox-oci create -b /path/to/bundle <container-id>
litebox-oci start <container-id>
```

### "cannot start container: status is running"

**Problem:** Container is already running.

**Solution:** The container is already started. Use `state` to check:
```bash
litebox-oci state <container-id>
```

### "cannot start container: status is stopped"

**Problem:** Container has already run and exited.

**Solution:** Delete and recreate:
```bash
litebox-oci delete <container-id>
litebox-oci create -b /path/to/bundle <container-id>
litebox-oci start <container-id>
```

## Runtime Errors

### "failed to load program"

**Problem:** The executable couldn't be loaded into the sandbox.

**Causes:**
- Binary doesn't exist in rootfs
- Binary is not a valid x86_64 ELF
- Binary is for a different architecture (e.g., ARM)
- Script file without shell rewriting (shebang scripts need rewriting enabled)

**Solution:**
1. Verify the binary exists: `ls /path/to/bundle/rootfs/bin/`
2. Check it's x86_64: `file /path/to/bundle/rootfs/bin/program`
3. Ensure it's a Linux ELF, not a script
4. For script files, ensure shell rewriting is enabled (don't use `--no-rewrite-shell`)

### Syscall warnings (lgetxattr, listxattr)

**Problem:** Warnings about unsupported syscalls appear.

**Impact:** Usually harmless. These syscalls are used for extended attributes which aren't supported.

**Solution:** Warnings can be ignored if the program runs correctly. Set `RUST_LOG=error` to hide warnings.

### Program exits immediately with no output

**Possible causes:**
1. Program crashed during startup
2. stdout/stderr not being captured correctly

**Debug steps:**
```bash
# Enable debug logging
RUST_LOG=debug litebox-oci run -b /path/to/bundle test-container
```

## containerd Integration Issues

### Permission denied

**Problem:** Operations fail with permission errors.

**Solution:** containerd typically runs as root. Use `sudo`:
```bash
sudo ctr run --rm --runc-binary /usr/local/bin/litebox-oci ...
```

Or use rootless containerd with appropriate `--root` flag.

### "runtime not found"

**Problem:** containerd can't find the litebox-oci binary.

**Solution:**
1. Ensure the binary is installed: `which litebox-oci`
2. Use full path: `--runc-binary /usr/local/bin/litebox-oci`

### Container exits immediately via containerd

**Problem:** Container runs fine with `litebox-oci run` but exits immediately via containerd.

**Possible causes:**
- stdout/stderr redirection issues
- containerd expects certain signals/responses

**Debug steps:**
1. Check containerd logs: `journalctl -u containerd`
2. Try with a simple command: `/bin/echo hello`

## Performance Issues

### Slow rootfs loading

**Problem:** Large images (e.g., python:3.11-slim) take several seconds to load.

**Cause:** All files are loaded into memory and executables are rewritten.

**Workarounds:**
- Use smaller base images (Alpine instead of Debian/Ubuntu)
- Only include necessary files in rootfs
- **Binary caching**: Subsequent runs are ~3-4x faster (rewritten binaries are cached)
- Future: Lazy loading (not yet implemented)

### Clearing the binary cache

If you need to force re-rewriting of binaries:

```bash
rm -rf ~/.cache/litebox-oci/rewritten/
```

The cache is stored at `$XDG_CACHE_HOME/litebox-oci/rewritten/` (defaults to `~/.cache/litebox-oci/rewritten/`).

### High memory usage

**Problem:** Container uses more memory than expected.

**Cause:** Entire rootfs is loaded into memory.

**Workarounds:**
- Minimize rootfs size
- Remove unnecessary files from the image

## Debug Mode

Enable verbose logging to diagnose issues:

```bash
# Maximum verbosity
RUST_LOG=trace litebox-oci run -b /path/to/bundle test

# Show only errors
RUST_LOG=error litebox-oci run -b /path/to/bundle test

# Specific module logging
RUST_LOG=litebox_runner_oci=debug litebox-oci run -b /path/to/bundle test
```

## Getting Help

1. Check container state: `litebox-oci state <container-id>`
2. Check state directory: `ls -la /run/litebox-oci/containers/<container-id>/`
3. Enable debug logging: `RUST_LOG=debug`
4. Review the [OCI_FEATURES.md](OCI_FEATURES.md) for supported features
