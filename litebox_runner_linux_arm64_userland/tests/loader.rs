// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

mod cache;
mod common;

use std::ffi::CString;
use std::path::Path;

use litebox::fs::{FileSystem as _, Mode, OFlags};
use litebox_platform_multiplex::Platform;

/// The rtld audit shared library built by build.rs, needed for rewriter-mode dynamic linking.
#[cfg(target_arch = "aarch64")]
const RTLD_AUDIT_SO: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/litebox_rtld_audit_arm64.so"));

/// Run a test body in a forked child process.
///
/// Each loader test needs its own `Platform`, but `set_platform()` can only be called once per
/// process (global static), and `Platform::new()` leaks memory via `Box::leak`. Running multiple
/// tests in the same process causes ENOMEM on the 3rd test because the PageManager's address space
/// is exhausted. Fork gives each test a fresh process with its own platform.
///
/// The child process runs `f()` and then exits. The parent waits for the child and asserts it
/// exited successfully. Stdout/stderr are inherited so test output is visible.
fn run_in_fork(f: impl FnOnce()) {
    let pid = unsafe { libc::fork() };
    match pid {
        -1 => panic!("fork() failed: {}", std::io::Error::last_os_error()),
        0 => {
            // Child process: run the test body, then exit.
            f();
            std::process::exit(0);
        }
        child_pid => {
            // Parent process: wait for child.
            let mut status: i32 = 0;
            let ret = unsafe { libc::waitpid(child_pid, &mut status, 0) };
            assert!(ret == child_pid, "waitpid failed");
            assert!(
                libc::WIFEXITED(status),
                "child did not exit normally (status=0x{:x})",
                status
            );
            let exit_code = libc::WEXITSTATUS(status);
            assert_eq!(
                exit_code, 0,
                "child exited with code {} (test failed in fork)",
                exit_code
            );
        }
    }
}

/// Create a fresh platform for this process. Called inside a forked child.
fn create_platform() -> &'static Platform {
    let platform = Platform::new(None);
    litebox_platform_multiplex::set_platform(platform);
    litebox_platform_multiplex::platform()
}

struct TestLauncher {
    platform: &'static Platform,
    shim_builder: litebox_shim_linux::LinuxShimBuilder,
    fs: litebox_shim_linux::DefaultFS,
}

impl TestLauncher {
    fn init(
        tar_data: &'static [u8],
        initial_dirs: &[&str],
    ) -> Self {
        let platform = create_platform();
        let shim_builder = litebox_shim_linux::LinuxShimBuilder::new();
        let litebox = shim_builder.litebox();

        let mut in_mem_fs = litebox::fs::in_mem::FileSystem::new(litebox);
        in_mem_fs.with_root_privileges(|fs| {
            fs.chmod("/", Mode::RWXU | Mode::RWXG | Mode::RWXO)
                .expect("Failed to set permissions on root");
        });
        let tar_ro_fs = litebox::fs::tar_ro::FileSystem::new(
            litebox,
            if tar_data.is_empty() {
                litebox::fs::tar_ro::EMPTY_TAR_FILE.into()
            } else {
                tar_data.into()
            },
        );
        let fs = shim_builder.default_fs(in_mem_fs, tar_ro_fs);
        let mut this = Self {
            platform,
            shim_builder,
            fs,
        };

        for each in initial_dirs {
            this.install_dir(each);
        }

        this
    }

    fn install_dir(&mut self, path: &str) {
        self.fs
            .mkdir(path, Mode::RWXU | Mode::RWXG | Mode::RWXO)
            .expect("Failed to create directory");
    }

    fn install_file(&mut self, contents: Vec<u8>, out: &str) {
        let fd = self
            .fs
            .open(
                out,
                OFlags::CREAT | OFlags::WRONLY,
                Mode::RWXG | Mode::RWXO | Mode::RWXU,
            )
            .unwrap();
        self.fs.write(&fd, &contents, None).unwrap();
        self.fs.close(&fd).unwrap();
    }

    fn test_load_exec_common(mut self, executable_path: &str, extra_envp: &[&str]) {
        let argv = vec![
            CString::new(executable_path).unwrap(),
            CString::new("hello").unwrap(),
        ];
        let mut envp = vec![
            CString::new("PATH=/bin").unwrap(),
            CString::new("HOME=/").unwrap(),
        ];
        for e in extra_envp {
            envp.push(CString::new(*e).unwrap());
        }
        self.shim_builder.set_fs(self.fs);
        let shim = self.shim_builder.build();
        let program = shim
            .load_program(self.platform.init_task(), executable_path, argv, envp)
            .unwrap();
        let _ = unsafe {
            litebox_platform_linux_userland::run_thread(
                program.entrypoints,
                &mut litebox_common_linux::PtRegs::default(),
            )
        };
        assert_eq!(
            program.process.wait(),
            0,
            "process exited with non-zero code"
        );
    }
}

/// Rewrite a file in-place: reads from `input`, rewrites to `output`.
fn rewrite_file(input: &Path, output: &Path) {
    let success = common::rewrite_with_cache(input, output, &["--allow-no-syscalls"]);
    assert!(
        success,
        "failed to rewrite {}",
        input.display()
    );
}

#[cfg(target_arch = "aarch64")]
#[test]
fn test_load_exec_dynamic() {
    // Compile and rewrite outside the fork so build artifacts are shared on disk.
    let dir_path = common::get_out_dir();
    let path = common::compile("./tests/hello.c", "hello_dylib", false, false);

    let hooked_path = dir_path.join("hello_dylib.hooked");
    rewrite_file(&path, &hooked_path);
    let executable_data = std::fs::read(&hooked_path).unwrap();

    let deps = common::find_dependencies(path.to_str().unwrap());

    // Rewrite all dependencies before the fork.
    let mut dep_entries: Vec<(String, Vec<u8>)> = Vec::new();
    for dep in &deps {
        let dep_path = Path::new(dep.as_str());
        let hooked_dep = dir_path.join(format!(
            "{}.loader.hooked",
            dep_path.file_name().unwrap().to_str().unwrap()
        ));
        rewrite_file(dep_path, &hooked_dep);
        let dep_data = std::fs::read(&hooked_dep).unwrap();
        dep_entries.push((dep.clone(), dep_data));
    }

    let rtld_audit_data = RTLD_AUDIT_SO.to_vec();

    run_in_fork(move || {
        let lib_dirs = [
            "/lib",
            "/lib/aarch64-linux-gnu",
            "/usr",
            "/usr/lib",
            "/usr/lib/aarch64-linux-gnu",
        ];

        let mut launcher = TestLauncher::init(&[], &lib_dirs);

        for (dep_name, dep_data) in dep_entries {
            launcher.install_file(dep_data, &dep_name);
        }

        launcher.install_file(rtld_audit_data, "/lib/litebox_rtld_audit_arm64.so");

        let executable_path = "/hello_dylib";
        launcher.install_file(executable_data, executable_path);
        launcher.test_load_exec_common(
            executable_path,
            &[
                "LD_LIBRARY_PATH=/lib:/lib/aarch64-linux-gnu:/usr/lib/aarch64-linux-gnu",
                "LD_AUDIT=/lib/litebox_rtld_audit_arm64.so",
            ],
        );
    });
}

#[cfg(target_arch = "aarch64")]
#[test]
fn test_load_exec_static() {
    let dir_path = common::get_out_dir();
    let path = common::compile("./tests/hello.c", "hello_exec", true, false);

    let hooked_path = dir_path.join("hello_exec.hooked");
    rewrite_file(&path, &hooked_path);
    let executable_data = std::fs::read(&hooked_path).unwrap();

    run_in_fork(move || {
        let executable_path = "/hello_exec";
        let mut launcher = TestLauncher::init(&[], &[]);
        launcher.install_file(executable_data, executable_path);
        launcher.test_load_exec_common(executable_path, &[]);
    });
}

const HELLO_WORLD_NOLIBC: &str = r#"
// gcc tests/test.c -o test -static -nostdlib
#if defined(__aarch64__)
int write(int fd, const char *buf, int length)
{
    register long x8 asm("x8") = 64; // SYS_write
    register long x0 asm("x0") = fd;
    register long x1 asm("x1") = (long)buf;
    register long x2 asm("x2") = length;
    long ret;

    asm volatile("svc #0"
        : "=r" (x0)
        : "r" (x8), "0" (x0), "r" (x1), "r" (x2)
        : "memory");

    ret = x0;
    return (int)ret;
}

_Noreturn void exit_group(int code)
{
    register long x8 asm("x8") = 94; // SYS_exit_group
    register long x0 asm("x0") = code;

    for (;;) {
        asm volatile("svc #0"
            :
            : "r" (x8), "r" (x0)
            : "memory");
    }
}
#else
#error "Unsupported architecture"
#endif

int main() {
    // use write to print a string
    write(1, "Hello, World!\n", 14);
    return 0;
}

void _start() {
    exit_group(main());
}
"#;

#[test]
fn test_syscall_rewriter() {
    let dir_path = common::get_out_dir();
    let src_path = dir_path.join("hello_exec_nolibc.c");
    std::fs::write(src_path.clone(), HELLO_WORLD_NOLIBC).unwrap();
    let path = dir_path.join("hello_exec_nolibc");
    common::compile(
        src_path.to_str().unwrap(),
        path.to_str().unwrap(),
        true,
        true,
    );

    let hooked_path = dir_path.join("hello_exec_nolibc.hooked");
    let _ = std::fs::remove_file(hooked_path.clone());
    let rewrite_success = common::rewrite_with_cache(&path, &hooked_path, &[]);
    assert!(rewrite_success, "failed to run syscall rewriter");

    let executable_data = std::fs::read(&hooked_path).unwrap();

    run_in_fork(move || {
        let executable_path = "/hello_exec_nolibc.hooked";
        let mut launcher = TestLauncher::init(&[], &[]);
        launcher.install_file(executable_data, executable_path);
        launcher.test_load_exec_common(executable_path, &[]);
    });
}
