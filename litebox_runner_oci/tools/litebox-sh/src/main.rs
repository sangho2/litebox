// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! litebox-sh: A minimal, fork-free shell for LiteBox sandboxes.
//!
//! This shell runs inside LiteBox where fork() is not available.
//! It supports:
//!   - Builtins: echo, cd, pwd, export, unset, exit, test/[, true, false,
//!               exec, set, read, source/.
//!   - Operators: && (and-then), || (or-else), ; (sequence)
//!   - Variable expansion: $VAR, ${VAR}, $?, $0-$9
//!   - Quoting: single quotes, double quotes, backslash escaping
//!   - I/O redirection: >, >>, <, 2>, 2>&1
//!   - External commands via execve (replaces shell process)
//!
//! Limitations:
//!   - No pipes (| requires fork)
//!   - No background jobs (& requires fork)
//!   - External commands replace the shell (remaining commands are skipped)

use std::collections::HashMap;
use std::env;
use std::ffi::CString;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::process;

// ── Shell state ──

struct ShellState {
    /// Shell-local variables (not yet exported to environment).
    vars: HashMap<String, ShellVar>,
    /// Last command exit status ($?).
    last_status: i32,
    /// set -e: exit on error.
    errexit: bool,
    /// Positional parameters ($0-$9).
    positional: Vec<String>,
}

struct ShellVar {
    value: String,
    exported: bool,
}

impl ShellState {
    fn new(args: &[String]) -> Self {
        Self {
            vars: HashMap::new(),
            last_status: 0,
            errexit: false,
            positional: args.iter().take(10).cloned().collect(),
        }
    }

    fn get_var(&self, name: &str) -> Option<String> {
        if let Some(v) = self.vars.get(name) {
            return Some(v.value.clone());
        }
        env::var(name).ok()
    }

    fn set_var(&mut self, name: &str, value: &str, do_export: bool) {
        if let Some(v) = self.vars.get_mut(name) {
            v.value = value.to_string();
            if do_export {
                v.exported = true;
            }
            if v.exported {
                // Safety: litebox-sh is single-threaded
                unsafe { env::set_var(name, value) };
            }
            return;
        }
        self.vars.insert(
            name.to_string(),
            ShellVar {
                value: value.to_string(),
                exported: do_export,
            },
        );
        if do_export {
            // Safety: litebox-sh is single-threaded
            unsafe { env::set_var(name, value) };
        }
    }

    fn unset_var(&mut self, name: &str) {
        self.vars.remove(name);
        // Safety: litebox-sh is single-threaded
        unsafe { env::remove_var(name) };
    }
}

// ── Variable expansion ──

fn expand_vars(input: &str, state: &ShellState) -> String {
    let mut result = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'$' {
            i += 1;
            if i >= bytes.len() {
                result.push('$');
                break;
            }
            match bytes[i] {
                b'?' => {
                    result.push_str(&state.last_status.to_string());
                    i += 1;
                }
                b'0'..=b'9' => {
                    let idx = (bytes[i] - b'0') as usize;
                    if let Some(val) = state.positional.get(idx) {
                        result.push_str(val);
                    }
                    i += 1;
                }
                b'{' => {
                    i += 1;
                    let start = i;
                    while i < bytes.len() && bytes[i] != b'}' {
                        i += 1;
                    }
                    let name = &input[start..i];
                    if let Some(val) = state.get_var(name) {
                        result.push_str(&val);
                    }
                    if i < bytes.len() {
                        i += 1; // skip '}'
                    }
                }
                b'A'..=b'Z' | b'a'..=b'z' | b'_' => {
                    let start = i;
                    while i < bytes.len()
                        && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_')
                    {
                        i += 1;
                    }
                    let name = &input[start..i];
                    if let Some(val) = state.get_var(name) {
                        result.push_str(&val);
                    }
                }
                _ => {
                    result.push('$');
                    result.push(bytes[i] as char);
                    i += 1;
                }
            }
        } else {
            result.push(bytes[i] as char);
            i += 1;
        }
    }
    result
}

// ── Tokenizer ──

#[derive(Debug, Clone, PartialEq)]
enum TokenType {
    Word(String),
    And,       // &&
    Or,        // ||
    Semi,      // ;
    RedirOut,  // >
    RedirAppend, // >>
    RedirIn,   // <
    RedirErr,  // 2>
    RedirErrOut, // 2>&1
}

fn tokenize(input: &str) -> Vec<TokenType> {
    let mut tokens = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        // Skip whitespace
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }

        // Operators
        if bytes[i] == b'&' && i + 1 < bytes.len() && bytes[i + 1] == b'&' {
            tokens.push(TokenType::And);
            i += 2;
            continue;
        }
        if bytes[i] == b'|' && i + 1 < bytes.len() && bytes[i + 1] == b'|' {
            tokens.push(TokenType::Or);
            i += 2;
            continue;
        }
        if bytes[i] == b';' {
            tokens.push(TokenType::Semi);
            i += 1;
            continue;
        }
        // 2>&1
        if bytes[i] == b'2'
            && i + 3 < bytes.len()
            && bytes[i + 1] == b'>'
            && bytes[i + 2] == b'&'
            && bytes[i + 3] == b'1'
        {
            tokens.push(TokenType::RedirErrOut);
            i += 4;
            continue;
        }
        // 2>
        if bytes[i] == b'2' && i + 1 < bytes.len() && bytes[i + 1] == b'>' {
            tokens.push(TokenType::RedirErr);
            i += 2;
            continue;
        }
        // >>
        if bytes[i] == b'>' && i + 1 < bytes.len() && bytes[i + 1] == b'>' {
            tokens.push(TokenType::RedirAppend);
            i += 2;
            continue;
        }
        // >
        if bytes[i] == b'>' {
            tokens.push(TokenType::RedirOut);
            i += 1;
            continue;
        }
        // <
        if bytes[i] == b'<' {
            tokens.push(TokenType::RedirIn);
            i += 1;
            continue;
        }
        // # comment
        if bytes[i] == b'#' {
            break;
        }

        // Word (with quoting)
        let mut word = String::new();
        while i < bytes.len() {
            match bytes[i] {
                b' ' | b'\t' | b';' | b'#' | b'>' | b'<' => break,
                b'&' if i + 1 < bytes.len() && bytes[i + 1] == b'&' => break,
                b'|' if i + 1 < bytes.len() && bytes[i + 1] == b'|' => break,
                b'\\' if i + 1 < bytes.len() => {
                    i += 1;
                    word.push(bytes[i] as char);
                    i += 1;
                }
                b'\'' => {
                    // Single quotes — no expansion
                    i += 1;
                    while i < bytes.len() && bytes[i] != b'\'' {
                        word.push(bytes[i] as char);
                        i += 1;
                    }
                    if i < bytes.len() {
                        i += 1;
                    }
                }
                b'"' => {
                    // Double quotes — allow variable expansion (handled later)
                    i += 1;
                    while i < bytes.len() && bytes[i] != b'"' {
                        if bytes[i] == b'\\' && i + 1 < bytes.len() {
                            i += 1;
                            word.push(bytes[i] as char);
                            i += 1;
                        } else {
                            word.push(bytes[i] as char);
                            i += 1;
                        }
                    }
                    if i < bytes.len() {
                        i += 1;
                    }
                }
                _ => {
                    word.push(bytes[i] as char);
                    i += 1;
                }
            }
        }
        tokens.push(TokenType::Word(word));
    }
    tokens
}

// ── Command representation ──

struct Command {
    argv: Vec<String>,
    redir_in: Option<String>,
    redir_out: Option<String>,
    redir_append: bool,
    redir_err: Option<String>,
    redir_err_to_out: bool,
}

impl Command {
    fn new() -> Self {
        Self {
            argv: Vec::new(),
            redir_in: None,
            redir_out: None,
            redir_append: false,
            redir_err: None,
            redir_err_to_out: false,
        }
    }
}

// ── Redirections ──

fn setup_redirections(cmd: &Command) -> Result<(), String> {
    if let Some(path) = &cmd.redir_in {
        let fd = nix_open(path, libc::O_RDONLY, 0)?;
        unsafe { libc::dup2(fd, libc::STDIN_FILENO) };
        unsafe { libc::close(fd) };
    }
    if let Some(path) = &cmd.redir_out {
        let flags = libc::O_WRONLY
            | libc::O_CREAT
            | if cmd.redir_append {
                libc::O_APPEND
            } else {
                libc::O_TRUNC
            };
        let fd = nix_open(path, flags, 0o644)?;
        unsafe { libc::dup2(fd, libc::STDOUT_FILENO) };
        unsafe { libc::close(fd) };
    }
    if let Some(path) = &cmd.redir_err {
        let fd = nix_open(path, libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC, 0o644)?;
        unsafe { libc::dup2(fd, libc::STDERR_FILENO) };
        unsafe { libc::close(fd) };
    }
    if cmd.redir_err_to_out {
        unsafe { libc::dup2(libc::STDOUT_FILENO, libc::STDERR_FILENO) };
    }
    Ok(())
}

fn nix_open(path: &str, flags: i32, mode: i32) -> Result<i32, String> {
    let c_path = CString::new(path).map_err(|_| format!("invalid path: {path}"))?;
    let fd = unsafe { libc::open(c_path.as_ptr(), flags, mode) };
    if fd < 0 {
        Err(format!(
            "litebox-sh: {path}: {}",
            io::Error::last_os_error()
        ))
    } else {
        Ok(fd)
    }
}

// ── Builtins ──

const BUILTINS: &[&str] = &[
    "echo", "cd", "pwd", "export", "unset", "exit", "test", "[", "true", "false", "set", "exec",
    "source", ".", "read", ":",
];

fn is_builtin(name: &str) -> bool {
    BUILTINS.contains(&name)
}

fn run_builtin(cmd: &Command, state: &mut ShellState) -> i32 {
    let argv = &cmd.argv;
    let argc = argv.len();
    let name = argv[0].as_str();

    match name {
        "true" | ":" => 0,
        "false" => 1,
        "echo" => {
            let mut newline = true;
            let mut start = 1;
            if argc > 1 && argv[1] == "-n" {
                newline = false;
                start = 2;
            }
            let text: Vec<&str> = argv[start..].iter().map(|s| s.as_str()).collect();
            print!("{}", text.join(" "));
            if newline {
                println!();
            }
            let _ = io::stdout().flush();
            0
        }
        "cd" => {
            let dir = if argc > 1 {
                argv[1].as_str()
            } else {
                match state.get_var("HOME") {
                    Some(ref h) if !h.is_empty() => {
                        // Leak to get &str with sufficient lifetime.
                        // This is fine — shell runs once and exits.
                        Box::leak(h.clone().into_boxed_str()) as &str
                    }
                    _ => {
                        eprintln!("litebox-sh: cd: HOME not set");
                        return 1;
                    }
                }
            };
            if env::set_current_dir(dir).is_err() {
                eprintln!(
                    "litebox-sh: cd: {dir}: {}",
                    io::Error::last_os_error()
                );
                return 1;
            }
            if let Ok(cwd) = env::current_dir() {
                // Safety: litebox-sh is single-threaded
                unsafe { env::set_var("PWD", cwd) };
            }
            0
        }
        "pwd" => {
            match env::current_dir() {
                Ok(cwd) => {
                    println!("{}", cwd.display());
                    0
                }
                Err(e) => {
                    eprintln!("litebox-sh: pwd: {e}");
                    1
                }
            }
        }
        "export" => {
            for arg in &argv[1..] {
                if let Some((key, val)) = arg.split_once('=') {
                    state.set_var(key, val, true);
                } else {
                    // Export existing variable
                    if let Some(val) = state.get_var(arg) {
                        // Safety: litebox-sh is single-threaded
                        unsafe { env::set_var(arg, &val) };
                    }
                }
            }
            0
        }
        "unset" => {
            for arg in &argv[1..] {
                state.unset_var(arg);
            }
            0
        }
        "exit" => {
            let code = if argc > 1 {
                argv[1].parse::<i32>().unwrap_or(2)
            } else {
                state.last_status
            };
            process::exit(code);
        }
        "test" | "[" => builtin_test(argv),
        "set" => {
            for arg in &argv[1..] {
                match arg.as_str() {
                    "-e" => state.errexit = true,
                    "+e" => state.errexit = false,
                    _ => {}
                }
            }
            0
        }
        "exec" => {
            if argc < 2 {
                return 0;
            }
            let _ = setup_redirections(cmd);
            exec_external(&argv[1..]);
        }
        "source" | "." => {
            if argc < 2 {
                eprintln!("litebox-sh: source: filename argument required");
                return 2;
            }
            builtin_source(&argv[1], state)
        }
        "read" => {
            if argc < 2 {
                eprintln!("litebox-sh: read: variable name required");
                return 1;
            }
            let mut line = String::new();
            match io::stdin().lock().read_line(&mut line) {
                Ok(0) => 1,
                Ok(_) => {
                    if line.ends_with('\n') {
                        line.pop();
                    }
                    state.set_var(&argv[1], &line, false);
                    0
                }
                Err(_) => 1,
            }
        }
        _ => 127,
    }
}

// ── test / [ builtin ──

fn builtin_test(argv: &[String]) -> i32 {
    let is_bracket = argv[0] == "[";
    let args: &[String] = if is_bracket {
        if argv.len() < 2 || argv.last().map(|s| s.as_str()) != Some("]") {
            eprintln!("litebox-sh: [: missing ]");
            return 2;
        }
        &argv[1..argv.len() - 1]
    } else {
        &argv[1..]
    };

    match args.len() {
        0 => 1, // no args = false
        1 => {
            if args[0].is_empty() { 1 } else { 0 }
        }
        2 => {
            let op = args[0].as_str();
            let val = &args[1];
            match op {
                "!" => if val.is_empty() { 0 } else { 1 },
                "-n" => if !val.is_empty() { 0 } else { 1 },
                "-z" => if val.is_empty() { 0 } else { 1 },
                "-f" => if Path::new(val).is_file() { 0 } else { 1 },
                "-d" => if Path::new(val).is_dir() { 0 } else { 1 },
                "-e" => if Path::new(val).exists() { 0 } else { 1 },
                "-x" => test_access(val, libc::X_OK),
                "-r" => test_access(val, libc::R_OK),
                "-w" => test_access(val, libc::W_OK),
                "-s" => {
                    fs::metadata(val)
                        .map(|m| if m.len() > 0 { 0 } else { 1 })
                        .unwrap_or(1)
                }
                _ => {
                    eprintln!("litebox-sh: test: unrecognized operator: {op}");
                    2
                }
            }
        }
        3 => {
            let (a, op, b) = (args[0].as_str(), args[1].as_str(), args[2].as_str());
            match op {
                "=" => if a == b { 0 } else { 1 },
                "!=" => if a != b { 0 } else { 1 },
                "-eq" => int_cmp(a, b, |x, y| x == y),
                "-ne" => int_cmp(a, b, |x, y| x != y),
                "-lt" => int_cmp(a, b, |x, y| x < y),
                "-gt" => int_cmp(a, b, |x, y| x > y),
                "-le" => int_cmp(a, b, |x, y| x <= y),
                "-ge" => int_cmp(a, b, |x, y| x >= y),
                _ => {
                    eprintln!("litebox-sh: test: unrecognized operator: {op}");
                    2
                }
            }
        }
        _ => {
            eprintln!("litebox-sh: test: too many arguments");
            2
        }
    }
}

fn test_access(path: &str, mode: i32) -> i32 {
    let c_path = match CString::new(path) {
        Ok(p) => p,
        Err(_) => return 1,
    };
    if unsafe { libc::access(c_path.as_ptr(), mode) } == 0 { 0 } else { 1 }
}

fn int_cmp(a: &str, b: &str, f: impl FnOnce(i64, i64) -> bool) -> i32 {
    let ai = a.parse::<i64>().unwrap_or(0);
    let bi = b.parse::<i64>().unwrap_or(0);
    if f(ai, bi) { 0 } else { 1 }
}

// ── source builtin ──

fn builtin_source(filename: &str, state: &mut ShellState) -> i32 {
    let content = match fs::read_to_string(filename) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("litebox-sh: source: {filename}: {e}");
            return 1;
        }
    };
    let mut result = 0;
    for line in content.lines() {
        result = execute_line(line, state);
        if state.errexit && result != 0 {
            break;
        }
    }
    result
}

// ── PATH resolution and exec ──

fn find_in_path(name: &str) -> Option<String> {
    if name.contains('/') {
        let c_path = CString::new(name).ok()?;
        if unsafe { libc::access(c_path.as_ptr(), libc::X_OK) } == 0 {
            return Some(name.to_string());
        }
        return None;
    }
    let path_var = env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string());
    for dir in path_var.split(':') {
        let candidate = format!("{dir}/{name}");
        let c_path = match CString::new(candidate.as_str()) {
            Ok(p) => p,
            Err(_) => continue,
        };
        if unsafe { libc::access(c_path.as_ptr(), libc::X_OK) } == 0 {
            return Some(candidate);
        }
    }
    None
}

/// Execute an external command via execv. This replaces the shell process.
fn exec_external(argv: &[String]) -> ! {
    let name = &argv[0];
    let resolved = match find_in_path(name) {
        Some(r) => r,
        None => {
            eprintln!("litebox-sh: {name}: command not found");
            process::exit(127);
        }
    };

    let c_prog = CString::new(resolved.as_str()).unwrap_or_default();
    let c_argv: Vec<CString> = argv
        .iter()
        .map(|a| CString::new(a.as_str()).unwrap_or_default())
        .collect();
    let c_ptrs: Vec<*const libc::c_char> = c_argv
        .iter()
        .map(|a| a.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();

    unsafe { libc::execv(c_prog.as_ptr(), c_ptrs.as_ptr()) };
    eprintln!(
        "litebox-sh: {name}: {}",
        io::Error::last_os_error()
    );
    process::exit(126);
}

// ── Command execution ──

fn execute_command(cmd: &Command, state: &mut ShellState) -> i32 {
    if cmd.argv.is_empty() {
        return 0;
    }

    // Handle VAR=value assignment (no command)
    if cmd.argv.len() == 1 && cmd.argv[0].contains('=') && !cmd.argv[0].starts_with('=') {
        if let Some((key, val)) = cmd.argv[0].split_once('=') {
            state.set_var(key, val, false);
            return 0;
        }
    }

    let name = &cmd.argv[0];
    let has_redir = cmd.redir_in.is_some()
        || cmd.redir_out.is_some()
        || cmd.redir_err.is_some()
        || cmd.redir_err_to_out;

    if is_builtin(name) {
        // Save/restore fds for builtins with redirections
        let saved = if has_redir {
            let saved_in = if cmd.redir_in.is_some() {
                Some(unsafe { libc::dup(libc::STDIN_FILENO) })
            } else {
                None
            };
            let saved_out = if cmd.redir_out.is_some() {
                Some(unsafe { libc::dup(libc::STDOUT_FILENO) })
            } else {
                None
            };
            let saved_err = if cmd.redir_err.is_some() || cmd.redir_err_to_out {
                Some(unsafe { libc::dup(libc::STDERR_FILENO) })
            } else {
                None
            };
            if let Err(e) = setup_redirections(cmd) {
                eprintln!("{e}");
                return 1;
            }
            Some((saved_in, saved_out, saved_err))
        } else {
            None
        };

        let result = run_builtin(cmd, state);

        // Restore saved fds
        if let Some((saved_in, saved_out, saved_err)) = saved {
            if let Some(fd) = saved_in {
                unsafe {
                    libc::dup2(fd, libc::STDIN_FILENO);
                    libc::close(fd);
                }
            }
            if let Some(fd) = saved_out {
                unsafe {
                    libc::dup2(fd, libc::STDOUT_FILENO);
                    libc::close(fd);
                }
            }
            if let Some(fd) = saved_err {
                unsafe {
                    libc::dup2(fd, libc::STDERR_FILENO);
                    libc::close(fd);
                }
            }
        }
        result
    } else {
        // External command — replaces the shell process
        if has_redir {
            let _ = setup_redirections(cmd);
        }
        exec_external(&cmd.argv);
    }
}

// ── Line execution ──

fn execute_line(line: &str, state: &mut ShellState) -> i32 {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return 0;
    }

    let tokens = tokenize(line);
    if tokens.is_empty() {
        return 0;
    }

    let mut ti = 0;
    let mut result = 0;
    let mut pending_op = TokenType::Semi; // as if preceded by ;

    while ti <= tokens.len() {
        // Build command from consecutive words and redirections
        let mut cmd = Command::new();

        while ti < tokens.len() {
            match &tokens[ti] {
                TokenType::And | TokenType::Or | TokenType::Semi => break,
                TokenType::RedirIn => {
                    ti += 1;
                    if let Some(TokenType::Word(w)) = tokens.get(ti) {
                        cmd.redir_in = Some(w.clone());
                    }
                }
                TokenType::RedirOut => {
                    ti += 1;
                    if let Some(TokenType::Word(w)) = tokens.get(ti) {
                        cmd.redir_out = Some(w.clone());
                        cmd.redir_append = false;
                    }
                }
                TokenType::RedirAppend => {
                    ti += 1;
                    if let Some(TokenType::Word(w)) = tokens.get(ti) {
                        cmd.redir_out = Some(w.clone());
                        cmd.redir_append = true;
                    }
                }
                TokenType::RedirErr => {
                    ti += 1;
                    if let Some(TokenType::Word(w)) = tokens.get(ti) {
                        cmd.redir_err = Some(w.clone());
                    }
                }
                TokenType::RedirErrOut => {
                    cmd.redir_err_to_out = true;
                }
                TokenType::Word(w) => {
                    cmd.argv.push(w.clone());
                }
            }
            ti += 1;
        }

        // Decide whether to execute based on pending operator
        let should_run = match pending_op {
            TokenType::Semi => true,
            TokenType::And => result == 0,
            TokenType::Or => result != 0,
            _ => true,
        };

        if should_run && !cmd.argv.is_empty() {
            // Expand variables at execution time
            cmd.argv = cmd
                .argv
                .iter()
                .map(|a| expand_vars(a, state))
                .collect();
            cmd.redir_in = cmd.redir_in.map(|r| expand_vars(&r, state));
            cmd.redir_out = cmd.redir_out.map(|r| expand_vars(&r, state));
            cmd.redir_err = cmd.redir_err.map(|r| expand_vars(&r, state));

            result = execute_command(&cmd, state);
            state.last_status = result;

            if state.errexit && result != 0 && pending_op != TokenType::Or {
                process::exit(result);
            }
        }

        // Get next operator
        if ti < tokens.len() {
            pending_op = tokens[ti].clone();
            ti += 1;
        } else {
            break;
        }
    }

    result
}

// ── Interactive mode ──

fn interactive_loop(state: &mut ShellState) {
    let stdin = io::stdin();
    loop {
        let ps1 = state
            .get_var("PS1")
            .unwrap_or_else(|| "$ ".to_string());
        eprint!("{ps1}");
        let _ = io::stderr().flush();

        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) | Err(_) => break,
            _ => {}
        }
        if line.ends_with('\n') {
            line.pop();
        }
        execute_line(&line, state);
    }
}

// ── Main ──

fn main() {
    let args: Vec<String> = env::args().collect();
    let mut state = ShellState::new(&args);

    let mut argi = 1;
    while argi < args.len() {
        match args[argi].as_str() {
            "-c" => {
                if argi + 1 >= args.len() {
                    eprintln!("litebox-sh: -c: option requires an argument");
                    process::exit(2);
                }
                let result = execute_line(&args[argi + 1], &mut state);
                process::exit(result);
            }
            "-e" => {
                state.errexit = true;
                argi += 1;
            }
            _ => {
                // Script file
                let result = builtin_source(&args[argi], &mut state);
                process::exit(result);
            }
        }
    }

    // Interactive mode
    interactive_loop(&mut state);
    process::exit(state.last_status);
}
