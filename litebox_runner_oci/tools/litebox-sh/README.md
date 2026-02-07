# litebox-sh

A minimal, fork-free shell designed for [LiteBox](../../README.md) sandboxes where `fork()` is not available.

## Overview

Standard shells (`bash`, `dash`, `ash`) require `fork()` to run external commands. LiteBox only supports `clone()` with `CLONE_VM|CLONE_THREAD` (threads), so traditional shells cannot work. **litebox-sh** provides a POSIX-like shell experience using only `execve()` for external commands (which replaces the shell process).

## Usage

```sh
# Run with -c (primary use case for OCI containers)
./litebox-sh -c 'export FOO=bar && echo $FOO && exec /app/server'

# Run a script file
./litebox-sh script.sh

# Interactive mode
./litebox-sh
```

> **Note:** In the OCI runner, litebox-sh is automatically injected into containers and
> shell entrypoints are rewritten to use it. You don't need to build or install it manually.

## Supported Features

### Builtins

| Command | Description |
|---------|-------------|
| `echo [-n] args...` | Print arguments to stdout |
| `cd [dir]` | Change working directory |
| `pwd` | Print working directory |
| `export KEY=VALUE` | Set and export environment variable |
| `unset VAR` | Remove variable |
| `exit [code]` | Exit with status code |
| `test` / `[` | Conditional expressions (see below) |
| `true` / `:` | Return success (exit 0) |
| `false` | Return failure (exit 1) |
| `set -e` / `set +e` | Enable/disable errexit |
| `exec cmd args...` | Replace shell with command |
| `source file` / `. file` | Execute commands from file |
| `read VAR` | Read line from stdin into variable |

### Operators

| Operator | Description |
|----------|-------------|
| `&&` | Execute next command only if previous succeeded |
| `||` | Execute next command only if previous failed |
| `;` | Execute next command unconditionally |

### Variable Expansion

| Syntax | Description |
|--------|-------------|
| `$VAR` | Expand variable |
| `${VAR}` | Expand variable (braced form) |
| `$?` | Last command's exit status |
| `$0` - `$9` | Positional parameters |

### Quoting

| Syntax | Description |
|--------|-------------|
| `'...'` | Single quotes — no expansion |
| `"..."` | Double quotes — variable expansion, backslash escaping |
| `\c` | Backslash — escape next character |

### I/O Redirection

| Syntax | Description |
|--------|-------------|
| `> file` | Redirect stdout to file (truncate) |
| `>> file` | Redirect stdout to file (append) |
| `< file` | Redirect stdin from file |
| `2> file` | Redirect stderr to file |
| `2>&1` | Redirect stderr to stdout |

### Test Expressions

```sh
test -f FILE    # true if FILE exists and is a regular file
test -d FILE    # true if FILE exists and is a directory
test -e FILE    # true if FILE exists
test -x FILE    # true if FILE exists and is executable
test -r FILE    # true if FILE exists and is readable
test -w FILE    # true if FILE exists and is writable
test -s FILE    # true if FILE exists and has size > 0
test -n STR     # true if string is non-empty
test -z STR     # true if string is empty
test S1 = S2    # string equality
test S1 != S2   # string inequality
test N1 -eq N2  # integer equality
test N1 -ne N2  # integer inequality
test N1 -lt N2  # integer less than
test N1 -gt N2  # integer greater than
test N1 -le N2  # integer less or equal
test N1 -ge N2  # integer greater or equal
```

## Limitations

- **No pipes** (`|`) — requires `fork()` to create concurrent processes
- **No background jobs** (`&`) — requires `fork()`
- **No subshells** (`$(...)`, `` `...` ``) — requires `fork()`
- **External commands replace the shell** — after `execve()`, the shell process is gone.
  This means `ls && cat` will only run `ls` (the shell is replaced by `ls`).
  **Use `exec` explicitly** for the final command: `echo setup && exec /app/server`
- **No job control** (`fg`, `bg`, `jobs`, `wait`)
- **No `if`/`while`/`for`** control structures — use `&&`/`||` chains instead

## Common OCI Patterns

```sh
# Environment setup + exec (most common)
litebox-sh -c 'export PATH=/app/bin:$PATH && export DB_HOST=localhost && exec /app/server'

# Conditional startup
litebox-sh -c 'test -f /app/config.yml && exec /app/server --config /app/config.yml || exec /app/server'

# Variable assignment chain
litebox-sh -c 'export PORT=8080 && export HOST=0.0.0.0 && echo "Starting on $HOST:$PORT" && exec /app/server'
```

## Building

```sh
# Build with musl (statically linked, ~435KB)
cargo build --release --target x86_64-unknown-linux-musl

# Binary at: target/x86_64-unknown-linux-musl/release/litebox-sh
```

Requires the `x86_64-unknown-linux-musl` target: `rustup target add x86_64-unknown-linux-musl`

The binary is automatically built and embedded into the OCI runner during `cargo build`.
