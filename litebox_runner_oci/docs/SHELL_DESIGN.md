# litebox-sh: Shell Design Analysis

## Current State

litebox-sh is a minimal, fork-free shell (~435KB static musl binary). When it encounters
an external command, it calls `execv()` which **replaces the shell process entirely**.
Only builtins can chain; the first external command terminates the shell.

```
echo hello && ls && cat file
  ↓ builtin    ↓ execv()    ↓ never reached
```

The OCI runner works around this by rewriting `sh -c "..."` entrypoints to add `exec`
before the final external command.

## Why Not Use an Existing Shell?

### The fork() Problem

Existing shells (dash, bash, busybox ash, nushell) rely on `fork()` for:
- Running external commands (fork + execve)
- Pipes (fork both sides, connect via pipe fd)
- Subshells `$(...)` and backticks
- Background jobs `cmd &`

LiteBox does not support `fork()` because it requires creating an independent copy
of the address space. LiteBox processes share memory (`CLONE_VM`).

### Can We Emulate fork() with vfork()?

No. Existing shells do "same-binary fork" — the child continues running shell code
before calling `execve()`:

```c
pid = fork();
if (pid == 0) {
    // Child: still running shell code at same addresses
    setup_redirections();  // modifies shell state
    close_fds();           // modifies shell state
    execve(program);       // only now replaces process
}
```

With `vfork()`, the child shares memory with the parent. Any state modifications
in the child (redirections, fd cleanup) corrupt the parent. The `vfork()` contract
requires the child to call only `execve()` or `_exit()` immediately.

### Can We Do COW via mprotect()?

In theory, we could mark pages read-only after `vfork()` and trap writes to
implement copy-on-write. In practice, the overhead of a page fault on every
write makes this impractical for a shell doing constant state manipulation.

### Can We Support Separate Address Spaces?

This would require `clone()` without `CLONE_VM` — a true process fork with
independent memory. This is a fundamental architectural change to LiteBox and
is not currently planned.

## Future: vfork() + execve() in LiteBox

If LiteBox adds `vfork()` + `execve()` support, litebox-sh can be extended
significantly. The key insight: litebox-sh **knows** it can't modify shared state
in the child, so it can be designed around this constraint.

### What vfork() + execve() Enables

**Sequential external commands:**
```
echo hello && ls && cat file
  ↓ builtin    ↓ vfork+execve  ↓ vfork+execve
  runs         child runs ls,   child runs cat,
  in-process   parent blocks,   parent blocks,
               then resumes     then resumes
```

Instead of replacing the shell, each external command runs in a `vfork()`'d child.
The parent blocks, then resumes and continues to the next command.

### What vfork() + pthreads Enables

LiteBox supports pthreads. Combined with `vfork()` + `execve()` + `pipe()`:

**Pipes:**
```
ls | grep foo
```
- Thread 1: `vfork()` + `execve("ls")`, stdout → pipe
- Thread 2: `vfork()` + `execve("grep")`, stdin ← pipe
- Both threads run concurrently

**Background jobs:**
```
server &
echo "started"
```
- Spawn pthread that does `vfork()` + `execve("server")`
- Shell thread continues immediately

**Subshells:**
```
result=$(ls /tmp)
```
- Spawn pthread: `vfork()` + `execve("ls")`, capture stdout via pipe
- Shell reads pipe, assigns to variable

### Feature Roadmap

| Feature | LiteBox Prereq | litebox-sh Work |
|---|---|---|
| Sequential external commands | `vfork()` + `execve()` | Replace `execv()` with vfork+execve loop |
| Pipes `\|` | + `pipe()` + pthreads | Pipeline orchestration, fd plumbing |
| Background jobs `&` | + pthreads | Job table, waitpid in background thread |
| Subshells `$(...)` | + `pipe()` + pthreads | Capture stdout, parse substitution |
| Control flow `if`/`for`/`while` | None | Parser/interpreter logic only |

### Impact on OCI Runner

With `vfork()` support, the OCI runner's shell rewriting workarounds can be removed:
- `rewrite_shell_args()` — no longer needed
- `add_exec_to_final_command()` — no longer needed
- `--no-rewrite-shell` flag — becomes the default behavior

## Conclusion

Extending litebox-sh is the right path. Existing shells cannot work without real
`fork()`, and emulating `fork()` transparently is impractical. litebox-sh is
designed for LiteBox's constraints and can grow into a capable shell as LiteBox
adds `vfork()` + `execve()` support.
