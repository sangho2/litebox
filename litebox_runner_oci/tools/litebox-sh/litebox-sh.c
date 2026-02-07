// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// litebox-sh: A minimal, fork-free shell for LiteBox sandboxes.
//
// This shell runs inside LiteBox where fork() is not available.
// It supports:
//   - Builtins: echo, cd, pwd, export, unset, exit, test/[, true, false,
//               exec, set, read, source/.
//   - Operators: && (and-then), || (or-else), ; (sequence)
//   - Variable expansion: $VAR, ${VAR}, $?, $0-$9
//   - Quoting: single quotes, double quotes, backslash escaping
//   - I/O redirection: >, >>, <, 2>, 2>&1
//   - External commands via execve (replaces shell process)
//
// Limitations:
//   - No pipes (| requires fork)
//   - No background jobs (& requires fork)
//   - External commands replace the shell (remaining commands are skipped)

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#define MAX_ARGS 256
#define MAX_LINE 8192
#define MAX_VARS 256
#define MAX_PATH_LEN 4096

// Shell state
static int last_exit_status = 0;
static int opt_errexit = 0; // set -e
static char *shell_argv[10]; // $0-$9
static int shell_argc = 0;

// Shell variables (beyond environment)
struct shell_var {
    char *name;
    char *value;
    int exported;
};
static struct shell_var shell_vars[MAX_VARS];
static int num_vars = 0;

// ----- Variable management -----

static const char *get_var(const char *name) {
    // Check shell variables first
    for (int i = 0; i < num_vars; i++) {
        if (strcmp(shell_vars[i].name, name) == 0)
            return shell_vars[i].value;
    }
    // Fall back to environment
    return getenv(name);
}

static void set_var(const char *name, const char *value, int do_export) {
    // Check if variable already exists
    for (int i = 0; i < num_vars; i++) {
        if (strcmp(shell_vars[i].name, name) == 0) {
            free(shell_vars[i].value);
            shell_vars[i].value = strdup(value);
            if (do_export)
                shell_vars[i].exported = 1;
            if (shell_vars[i].exported)
                setenv(name, value, 1);
            return;
        }
    }
    // New variable
    if (num_vars < MAX_VARS) {
        shell_vars[num_vars].name = strdup(name);
        shell_vars[num_vars].value = strdup(value);
        shell_vars[num_vars].exported = do_export;
        if (do_export)
            setenv(name, value, 1);
        num_vars++;
    }
}

static void unset_var(const char *name) {
    for (int i = 0; i < num_vars; i++) {
        if (strcmp(shell_vars[i].name, name) == 0) {
            free(shell_vars[i].name);
            free(shell_vars[i].value);
            shell_vars[num_vars - 1].name = NULL;
            // Move last element to fill gap
            if (i < num_vars - 1)
                shell_vars[i] = shell_vars[num_vars - 1];
            num_vars--;
            break;
        }
    }
    unsetenv(name);
}

// ----- Variable expansion -----

// Expand variables in a string. Returns malloc'd result.
static char *expand_vars(const char *input) {
    char *result = malloc(MAX_LINE);
    if (!result)
        return NULL;
    int ri = 0;

    for (int i = 0; input[i] && ri < MAX_LINE - 1; i++) {
        if (input[i] == '$') {
            i++;
            if (input[i] == '?') {
                // $? - last exit status
                ri += snprintf(result + ri, MAX_LINE - ri, "%d", last_exit_status);
            } else if (input[i] >= '0' && input[i] <= '9') {
                // $0-$9 - positional parameters
                int idx = input[i] - '0';
                if (idx < shell_argc && shell_argv[idx])
                    ri += snprintf(result + ri, MAX_LINE - ri, "%s",
                                   shell_argv[idx]);
            } else if (input[i] == '{') {
                // ${VAR}
                i++;
                char varname[256];
                int vi = 0;
                while (input[i] && input[i] != '}' && vi < 255) {
                    varname[vi++] = input[i++];
                }
                varname[vi] = '\0';
                const char *val = get_var(varname);
                if (val)
                    ri += snprintf(result + ri, MAX_LINE - ri, "%s", val);
            } else if ((input[i] >= 'A' && input[i] <= 'Z') ||
                       (input[i] >= 'a' && input[i] <= 'z') ||
                       input[i] == '_') {
                // $VAR
                char varname[256];
                int vi = 0;
                while (input[i] &&
                       ((input[i] >= 'A' && input[i] <= 'Z') ||
                        (input[i] >= 'a' && input[i] <= 'z') ||
                        (input[i] >= '0' && input[i] <= '9') ||
                        input[i] == '_') &&
                       vi < 255) {
                    varname[vi++] = input[i++];
                }
                varname[vi] = '\0';
                i--; // back up one since the for loop will advance
                const char *val = get_var(varname);
                if (val)
                    ri += snprintf(result + ri, MAX_LINE - ri, "%s", val);
            } else {
                // Lone $ - keep it
                result[ri++] = '$';
                if (input[i])
                    result[ri++] = input[i];
            }
        } else {
            result[ri++] = input[i];
        }
    }
    result[ri] = '\0';
    return result;
}

// ----- Tokenizer -----

// Token types for operators
enum token_type {
    TOK_WORD,
    TOK_AND,  // &&
    TOK_OR,   // ||
    TOK_SEMI, // ;
    TOK_REDIR_OUT,    // >
    TOK_REDIR_APPEND, // >>
    TOK_REDIR_IN,     // <
    TOK_REDIR_ERR,    // 2>
    TOK_REDIR_ERR_OUT, // 2>&1
    TOK_END,
};

struct token {
    enum token_type type;
    char *value; // For TOK_WORD
};

// Parse a single command (list of words + redirections) from tokens.
struct command {
    char *argv[MAX_ARGS];
    int argc;
    char *redir_in;
    char *redir_out;
    int redir_append;
    char *redir_err;
    int redir_err_to_out; // 2>&1
};

// Tokenize input into words and operators, handling quoting.
// Returns number of tokens.
static int tokenize(const char *input, struct token *tokens, int max_tokens) {
    int nt = 0;
    int i = 0;
    int len = strlen(input);

    while (i < len && nt < max_tokens - 1) {
        // Skip whitespace
        while (i < len && (input[i] == ' ' || input[i] == '\t'))
            i++;
        if (i >= len)
            break;

        // Check for operators
        if (input[i] == '&' && i + 1 < len && input[i + 1] == '&') {
            tokens[nt].type = TOK_AND;
            tokens[nt].value = NULL;
            nt++;
            i += 2;
            continue;
        }
        if (input[i] == '|' && i + 1 < len && input[i + 1] == '|') {
            tokens[nt].type = TOK_OR;
            tokens[nt].value = NULL;
            nt++;
            i += 2;
            continue;
        }
        if (input[i] == ';') {
            tokens[nt].type = TOK_SEMI;
            tokens[nt].value = NULL;
            nt++;
            i++;
            continue;
        }
        // 2>&1
        if (input[i] == '2' && i + 3 < len && input[i + 1] == '>' &&
            input[i + 2] == '&' && input[i + 3] == '1') {
            tokens[nt].type = TOK_REDIR_ERR_OUT;
            tokens[nt].value = NULL;
            nt++;
            i += 4;
            continue;
        }
        // 2>
        if (input[i] == '2' && i + 1 < len && input[i + 1] == '>') {
            tokens[nt].type = TOK_REDIR_ERR;
            tokens[nt].value = NULL;
            nt++;
            i += 2;
            continue;
        }
        // >>
        if (input[i] == '>' && i + 1 < len && input[i + 1] == '>') {
            tokens[nt].type = TOK_REDIR_APPEND;
            tokens[nt].value = NULL;
            nt++;
            i += 2;
            continue;
        }
        // >
        if (input[i] == '>') {
            tokens[nt].type = TOK_REDIR_OUT;
            tokens[nt].value = NULL;
            nt++;
            i++;
            continue;
        }
        // <
        if (input[i] == '<') {
            tokens[nt].type = TOK_REDIR_IN;
            tokens[nt].value = NULL;
            nt++;
            i++;
            continue;
        }
        // # comment - skip rest of line
        if (input[i] == '#')
            break;

        // Word (possibly quoted)
        char word[MAX_LINE];
        int wi = 0;
        while (i < len && wi < MAX_LINE - 1) {
            if (input[i] == ' ' || input[i] == '\t')
                break;
            if (input[i] == ';' || input[i] == '#')
                break;
            if (input[i] == '>' || input[i] == '<')
                break;
            if (input[i] == '&' && i + 1 < len && input[i + 1] == '&')
                break;
            if (input[i] == '|' && i + 1 < len && input[i + 1] == '|')
                break;

            if (input[i] == '\\' && i + 1 < len) {
                // Backslash escape
                i++;
                word[wi++] = input[i++];
            } else if (input[i] == '\'') {
                // Single quotes - no expansion
                i++;
                while (i < len && input[i] != '\'' && wi < MAX_LINE - 1)
                    word[wi++] = input[i++];
                if (i < len)
                    i++; // skip closing quote
            } else if (input[i] == '"') {
                // Double quotes - allow variable expansion (handled later)
                i++;
                while (i < len && input[i] != '"' && wi < MAX_LINE - 1) {
                    if (input[i] == '\\' && i + 1 < len) {
                        i++;
                        word[wi++] = input[i++];
                    } else {
                        word[wi++] = input[i++];
                    }
                }
                if (i < len)
                    i++; // skip closing quote
            } else {
                word[wi++] = input[i++];
            }
        }
        word[wi] = '\0';

        // Store raw word (expansion deferred to execution time)
        tokens[nt].type = TOK_WORD;
        tokens[nt].value = strdup(word);
        nt++;
    }

    tokens[nt].type = TOK_END;
    tokens[nt].value = NULL;
    return nt;
}

static void free_tokens(struct token *tokens, int count) {
    for (int i = 0; i < count; i++) {
        free(tokens[i].value);
    }
}

// ----- Redirections -----

// Set up redirections. Returns 0 on success, -1 on error.
static int setup_redirections(struct command *cmd) {
    if (cmd->redir_in) {
        int fd = open(cmd->redir_in, O_RDONLY);
        if (fd < 0) {
            fprintf(stderr, "litebox-sh: %s: %s\n", cmd->redir_in,
                    strerror(errno));
            return -1;
        }
        dup2(fd, STDIN_FILENO);
        close(fd);
    }
    if (cmd->redir_out) {
        int flags = O_WRONLY | O_CREAT;
        flags |= cmd->redir_append ? O_APPEND : O_TRUNC;
        int fd = open(cmd->redir_out, flags, 0644);
        if (fd < 0) {
            fprintf(stderr, "litebox-sh: %s: %s\n", cmd->redir_out,
                    strerror(errno));
            return -1;
        }
        dup2(fd, STDOUT_FILENO);
        close(fd);
    }
    if (cmd->redir_err) {
        int fd = open(cmd->redir_err, O_WRONLY | O_CREAT | O_TRUNC, 0644);
        if (fd < 0) {
            fprintf(stderr, "litebox-sh: %s: %s\n", cmd->redir_err,
                    strerror(errno));
            return -1;
        }
        dup2(fd, STDERR_FILENO);
        close(fd);
    }
    if (cmd->redir_err_to_out) {
        dup2(STDOUT_FILENO, STDERR_FILENO);
    }
    return 0;
}

// ----- Builtins -----

static int is_builtin(const char *name);
static int run_builtin(struct command *cmd);

// test / [ builtin
static int builtin_test(int argc, char **argv) {
    // Handle [ ... ] syntax: strip trailing ]
    int is_bracket = (strcmp(argv[0], "[") == 0);
    if (is_bracket) {
        if (argc < 2 || strcmp(argv[argc - 1], "]") != 0) {
            fprintf(stderr, "litebox-sh: [: missing ]\n");
            return 2;
        }
        argc--;
    }

    if (argc == 1)
        return 1; // no args = false
    if (argc == 2) {
        // test STRING - true if non-empty
        return strlen(argv[1]) == 0 ? 1 : 0;
    }
    if (argc == 3) {
        // Unary operators
        if (strcmp(argv[1], "!") == 0) {
            // Negate single arg test
            return strlen(argv[2]) == 0 ? 0 : 1;
        }
        if (strcmp(argv[1], "-n") == 0)
            return strlen(argv[2]) > 0 ? 0 : 1;
        if (strcmp(argv[1], "-z") == 0)
            return strlen(argv[2]) == 0 ? 0 : 1;
        if (strcmp(argv[1], "-f") == 0) {
            struct stat st;
            return (stat(argv[2], &st) == 0 && S_ISREG(st.st_mode)) ? 0 : 1;
        }
        if (strcmp(argv[1], "-d") == 0) {
            struct stat st;
            return (stat(argv[2], &st) == 0 && S_ISDIR(st.st_mode)) ? 0 : 1;
        }
        if (strcmp(argv[1], "-e") == 0) {
            struct stat st;
            return stat(argv[2], &st) == 0 ? 0 : 1;
        }
        if (strcmp(argv[1], "-x") == 0) {
            return access(argv[2], X_OK) == 0 ? 0 : 1;
        }
        if (strcmp(argv[1], "-r") == 0) {
            return access(argv[2], R_OK) == 0 ? 0 : 1;
        }
        if (strcmp(argv[1], "-w") == 0) {
            return access(argv[2], W_OK) == 0 ? 0 : 1;
        }
        if (strcmp(argv[1], "-s") == 0) {
            struct stat st;
            return (stat(argv[2], &st) == 0 && st.st_size > 0) ? 0 : 1;
        }
    }
    if (argc == 4) {
        // Binary operators
        if (strcmp(argv[2], "=") == 0)
            return strcmp(argv[1], argv[3]) == 0 ? 0 : 1;
        if (strcmp(argv[2], "!=") == 0)
            return strcmp(argv[1], argv[3]) != 0 ? 0 : 1;
        if (strcmp(argv[2], "-eq") == 0)
            return atoi(argv[1]) == atoi(argv[3]) ? 0 : 1;
        if (strcmp(argv[2], "-ne") == 0)
            return atoi(argv[1]) != atoi(argv[3]) ? 0 : 1;
        if (strcmp(argv[2], "-lt") == 0)
            return atoi(argv[1]) < atoi(argv[3]) ? 0 : 1;
        if (strcmp(argv[2], "-gt") == 0)
            return atoi(argv[1]) > atoi(argv[3]) ? 0 : 1;
        if (strcmp(argv[2], "-le") == 0)
            return atoi(argv[1]) <= atoi(argv[3]) ? 0 : 1;
        if (strcmp(argv[2], "-ge") == 0)
            return atoi(argv[1]) >= atoi(argv[3]) ? 0 : 1;
    }

    fprintf(stderr, "litebox-sh: test: unrecognized expression\n");
    return 2;
}

static int is_builtin(const char *name) {
    static const char *builtins[] = {"echo",  "cd",    "pwd",    "export",
                                     "unset", "exit",  "test",   "[",
                                     "true",  "false", "set",    "exec",
                                     "source", ".",    "read",   ":"};
    for (int i = 0; i < (int)(sizeof(builtins) / sizeof(builtins[0])); i++) {
        if (strcmp(name, builtins[i]) == 0)
            return 1;
    }
    return 0;
}

// Execute a script file (source/.)
static int execute_line(const char *line);

static int builtin_source(const char *filename) {
    FILE *f = fopen(filename, "r");
    if (!f) {
        fprintf(stderr, "litebox-sh: source: %s: %s\n", filename,
                strerror(errno));
        return 1;
    }
    char line[MAX_LINE];
    int result = 0;
    while (fgets(line, sizeof(line), f)) {
        // Strip trailing newline
        int len = strlen(line);
        if (len > 0 && line[len - 1] == '\n')
            line[len - 1] = '\0';
        result = execute_line(line);
        if (opt_errexit && result != 0)
            break;
    }
    fclose(f);
    return result;
}

static int run_builtin(struct command *cmd) {
    char **argv = cmd->argv;
    int argc = cmd->argc;

    if (strcmp(argv[0], "true") == 0 || strcmp(argv[0], ":") == 0) {
        return 0;
    }
    if (strcmp(argv[0], "false") == 0) {
        return 1;
    }
    if (strcmp(argv[0], "echo") == 0) {
        int newline = 1;
        int start = 1;
        if (argc > 1 && strcmp(argv[1], "-n") == 0) {
            newline = 0;
            start = 2;
        }
        for (int i = start; i < argc; i++) {
            if (i > start)
                putchar(' ');
            fputs(argv[i], stdout);
        }
        if (newline)
            putchar('\n');
        fflush(stdout);
        return 0;
    }
    if (strcmp(argv[0], "cd") == 0) {
        const char *dir = argc > 1 ? argv[1] : get_var("HOME");
        if (!dir) {
            fprintf(stderr, "litebox-sh: cd: HOME not set\n");
            return 1;
        }
        if (chdir(dir) != 0) {
            fprintf(stderr, "litebox-sh: cd: %s: %s\n", dir, strerror(errno));
            return 1;
        }
        // Update PWD
        char cwd[MAX_PATH_LEN];
        if (getcwd(cwd, sizeof(cwd)))
            setenv("PWD", cwd, 1);
        return 0;
    }
    if (strcmp(argv[0], "pwd") == 0) {
        char cwd[MAX_PATH_LEN];
        if (getcwd(cwd, sizeof(cwd))) {
            puts(cwd);
            return 0;
        }
        fprintf(stderr, "litebox-sh: pwd: %s\n", strerror(errno));
        return 1;
    }
    if (strcmp(argv[0], "export") == 0) {
        for (int i = 1; i < argc; i++) {
            char *eq = strchr(argv[i], '=');
            if (eq) {
                *eq = '\0';
                set_var(argv[i], eq + 1, 1);
            } else {
                // Export existing variable
                const char *val = get_var(argv[i]);
                if (val)
                    setenv(argv[i], val, 1);
            }
        }
        return 0;
    }
    if (strcmp(argv[0], "unset") == 0) {
        for (int i = 1; i < argc; i++)
            unset_var(argv[i]);
        return 0;
    }
    if (strcmp(argv[0], "exit") == 0) {
        int code = argc > 1 ? atoi(argv[1]) : last_exit_status;
        exit(code);
    }
    if (strcmp(argv[0], "test") == 0 || strcmp(argv[0], "[") == 0) {
        return builtin_test(argc, argv);
    }
    if (strcmp(argv[0], "set") == 0) {
        for (int i = 1; i < argc; i++) {
            if (strcmp(argv[i], "-e") == 0)
                opt_errexit = 1;
            else if (strcmp(argv[i], "+e") == 0)
                opt_errexit = 0;
        }
        return 0;
    }
    if (strcmp(argv[0], "exec") == 0) {
        if (argc < 2)
            return 0;
        // Shift args: exec cmd args... → execvp(cmd, [cmd, args...])
        setup_redirections(cmd);
        execvp(argv[1], &argv[1]);
        fprintf(stderr, "litebox-sh: exec: %s: %s\n", argv[1],
                strerror(errno));
        return 127;
    }
    if (strcmp(argv[0], "source") == 0 || strcmp(argv[0], ".") == 0) {
        if (argc < 2) {
            fprintf(stderr, "litebox-sh: source: filename argument required\n");
            return 2;
        }
        return builtin_source(argv[1]);
    }
    if (strcmp(argv[0], "read") == 0) {
        if (argc < 2) {
            fprintf(stderr, "litebox-sh: read: variable name required\n");
            return 1;
        }
        char line[MAX_LINE];
        if (fgets(line, sizeof(line), stdin)) {
            int len = strlen(line);
            if (len > 0 && line[len - 1] == '\n')
                line[len - 1] = '\0';
            set_var(argv[1], line, 0);
            return 0;
        }
        return 1;
    }

    return 127;
}

// ----- PATH resolution -----

static int find_in_path(const char *name, char *resolved, int resolved_size) {
    // If name contains /, use it directly
    if (strchr(name, '/')) {
        strncpy(resolved, name, resolved_size - 1);
        resolved[resolved_size - 1] = '\0';
        return access(resolved, X_OK) == 0 ? 0 : -1;
    }

    const char *path = get_var("PATH");
    if (!path)
        path = "/usr/bin:/bin";

    char pathbuf[MAX_PATH_LEN];
    strncpy(pathbuf, path, sizeof(pathbuf) - 1);
    pathbuf[sizeof(pathbuf) - 1] = '\0';

    char *saveptr;
    char *dir = strtok_r(pathbuf, ":", &saveptr);
    while (dir) {
        snprintf(resolved, resolved_size, "%s/%s", dir, name);
        if (access(resolved, X_OK) == 0)
            return 0;
        dir = strtok_r(NULL, ":", &saveptr);
    }
    return -1;
}

// ----- Command execution -----

// Execute a single simple command (one pipeline stage).
// Returns exit status or -1 if this replaces the shell (exec).
static int execute_command(struct command *cmd) {
    if (cmd->argc == 0)
        return 0;

    // Handle VAR=value assignments (no command)
    if (cmd->argc == 1 && strchr(cmd->argv[0], '=') &&
        cmd->argv[0][0] != '=') {
        char *copy = strdup(cmd->argv[0]);
        char *eq = strchr(copy, '=');
        *eq = '\0';
        set_var(copy, eq + 1, 0);
        free(copy);
        return 0;
    }

    // Set up redirections for builtins too
    int saved_stdin = -1, saved_stdout = -1, saved_stderr = -1;
    int has_redir = cmd->redir_in || cmd->redir_out || cmd->redir_err ||
                    cmd->redir_err_to_out;
    if (has_redir && is_builtin(cmd->argv[0])) {
        // Save original fds for builtins (we restore after)
        if (cmd->redir_in)
            saved_stdin = dup(STDIN_FILENO);
        if (cmd->redir_out)
            saved_stdout = dup(STDOUT_FILENO);
        if (cmd->redir_err || cmd->redir_err_to_out)
            saved_stderr = dup(STDERR_FILENO);
        if (setup_redirections(cmd) < 0)
            return 1;
    }

    int result;
    if (is_builtin(cmd->argv[0])) {
        result = run_builtin(cmd);
        // Restore fds
        if (saved_stdin >= 0) {
            dup2(saved_stdin, STDIN_FILENO);
            close(saved_stdin);
        }
        if (saved_stdout >= 0) {
            dup2(saved_stdout, STDOUT_FILENO);
            close(saved_stdout);
        }
        if (saved_stderr >= 0) {
            dup2(saved_stderr, STDERR_FILENO);
            close(saved_stderr);
        }
    } else {
        // External command - this replaces the shell process
        if (has_redir)
            setup_redirections(cmd);

        char resolved[MAX_PATH_LEN];
        if (find_in_path(cmd->argv[0], resolved, sizeof(resolved)) < 0) {
            fprintf(stderr, "litebox-sh: %s: command not found\n",
                    cmd->argv[0]);
            return 127;
        }

        // Build null-terminated argv
        cmd->argv[cmd->argc] = NULL;
        execv(resolved, cmd->argv);
        // If we get here, exec failed
        fprintf(stderr, "litebox-sh: %s: %s\n", cmd->argv[0], strerror(errno));
        return 126;
    }

    return result;
}

// ----- Line execution -----

// Parse tokens into commands separated by operators, and execute with
// && / || / ; logic.
static int execute_line(const char *line) {
    // Skip empty lines and comments
    const char *p = line;
    while (*p == ' ' || *p == '\t')
        p++;
    if (*p == '\0' || *p == '#')
        return 0;

    struct token tokens[MAX_ARGS * 2];
    int ntokens = tokenize(line, tokens, MAX_ARGS * 2);
    if (ntokens == 0)
        return 0;

    int ti = 0;
    int result = 0;
    enum token_type pending_op = TOK_SEMI; // Start as if preceded by ;

    while (ti <= ntokens) {
        // Build a command from consecutive TOK_WORD and redirection tokens
        struct command cmd = {0};

        while (ti < ntokens && tokens[ti].type != TOK_AND &&
               tokens[ti].type != TOK_OR && tokens[ti].type != TOK_SEMI) {
            switch (tokens[ti].type) {
            case TOK_REDIR_IN:
                ti++;
                if (ti < ntokens && tokens[ti].type == TOK_WORD)
                    cmd.redir_in = tokens[ti].value;
                break;
            case TOK_REDIR_OUT:
                ti++;
                if (ti < ntokens && tokens[ti].type == TOK_WORD) {
                    cmd.redir_out = tokens[ti].value;
                    cmd.redir_append = 0;
                }
                break;
            case TOK_REDIR_APPEND:
                ti++;
                if (ti < ntokens && tokens[ti].type == TOK_WORD) {
                    cmd.redir_out = tokens[ti].value;
                    cmd.redir_append = 1;
                }
                break;
            case TOK_REDIR_ERR:
                ti++;
                if (ti < ntokens && tokens[ti].type == TOK_WORD)
                    cmd.redir_err = tokens[ti].value;
                break;
            case TOK_REDIR_ERR_OUT:
                cmd.redir_err_to_out = 1;
                break;
            case TOK_WORD:
                if (cmd.argc < MAX_ARGS - 1)
                    cmd.argv[cmd.argc++] = tokens[ti].value;
                break;
            default:
                break;
            }
            ti++;
        }

        // Check if we should execute based on pending operator
        int should_run = 0;
        switch (pending_op) {
        case TOK_SEMI:
            should_run = 1;
            break;
        case TOK_AND:
            should_run = (result == 0);
            break;
        case TOK_OR:
            should_run = (result != 0);
            break;
        default:
            should_run = 1;
            break;
        }

        if (should_run && cmd.argc > 0) {
            // Expand variables at execution time so earlier commands' exports
            // are visible to later commands in the chain.
            char *expanded[MAX_ARGS];
            int num_expanded = 0;
            for (int j = 0; j < cmd.argc; j++) {
                expanded[j] = expand_vars(cmd.argv[j]);
                cmd.argv[j] = expanded[j];
                num_expanded++;
            }
            char *exp_redir_in = NULL, *exp_redir_out = NULL,
                 *exp_redir_err = NULL;
            if (cmd.redir_in) {
                exp_redir_in = expand_vars(cmd.redir_in);
                cmd.redir_in = exp_redir_in;
            }
            if (cmd.redir_out) {
                exp_redir_out = expand_vars(cmd.redir_out);
                cmd.redir_out = exp_redir_out;
            }
            if (cmd.redir_err) {
                exp_redir_err = expand_vars(cmd.redir_err);
                cmd.redir_err = exp_redir_err;
            }

            result = execute_command(&cmd);
            last_exit_status = result;

            for (int j = 0; j < num_expanded; j++)
                free(expanded[j]);
            free(exp_redir_in);
            free(exp_redir_out);
            free(exp_redir_err);

            if (opt_errexit && result != 0 && pending_op != TOK_OR) {
                free_tokens(tokens, ntokens);
                exit(result);
            }
        }

        // Get the next operator
        if (ti < ntokens) {
            pending_op = tokens[ti].type;
            ti++;
        } else {
            break;
        }
    }

    free_tokens(tokens, ntokens);
    return result;
}

// ----- Interactive mode -----

static void interactive_loop(void) {
    char line[MAX_LINE];
    while (1) {
        const char *ps1 = get_var("PS1");
        if (!ps1)
            ps1 = "$ ";
        fputs(ps1, stderr);
        fflush(stderr);

        if (!fgets(line, sizeof(line), stdin))
            break;

        int len = strlen(line);
        if (len > 0 && line[len - 1] == '\n')
            line[len - 1] = '\0';

        execute_line(line);
    }
}

// ----- Main -----

int main(int argc, char **argv) {
    // Set up positional parameters
    shell_argc = argc < 10 ? argc : 10;
    for (int i = 0; i < shell_argc; i++)
        shell_argv[i] = argv[i];

    // Check for -c option
    int argi = 1;
    while (argi < argc) {
        if (strcmp(argv[argi], "-c") == 0) {
            if (argi + 1 >= argc) {
                fprintf(stderr, "litebox-sh: -c: option requires an argument\n");
                return 2;
            }
            return execute_line(argv[argi + 1]);
        }
        if (strcmp(argv[argi], "-e") == 0) {
            opt_errexit = 1;
            argi++;
            continue;
        }
        // Script file
        return builtin_source(argv[argi]);
    }

    // Interactive mode
    interactive_loop();
    return last_exit_status;
}
