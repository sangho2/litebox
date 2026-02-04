// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Virtual /proc filesystem emulation.
//!
//! This module provides synthetic /proc files for containerized applications
//! that expect to read process and system information from /proc.

use alloc::string::String;
use alloc::vec::Vec;

/// Types of virtual /proc files we support
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcFile {
    /// /proc/self/exe - path to the executable
    SelfExe,
    /// /proc/self/cmdline - command line arguments (NUL-separated)
    SelfCmdline,
    /// /proc/self/environ - environment variables (NUL-separated)
    SelfEnviron,
    /// /proc/self/cwd - current working directory (symlink)
    SelfCwd,
    /// /proc/self/fd/<n> - file descriptor symlinks
    SelfFd(u32),
    /// /proc/self/maps - memory mappings
    SelfMaps,
    /// /proc/self/stat - process status
    SelfStat,
    /// /proc/self/status - human-readable process status
    SelfStatus,
    /// /proc/mounts or /proc/self/mounts - mount points
    Mounts,
    /// /proc/cpuinfo - CPU information
    CpuInfo,
    /// /proc/meminfo - memory information
    MemInfo,
    /// /proc/stat - system statistics
    Stat,
    /// /proc/loadavg - load average
    LoadAvg,
    /// /proc/uptime - system uptime
    Uptime,
    /// /proc/version - kernel version string
    Version,
    /// /proc/sys/kernel/hostname
    SysKernelHostname,
    /// /proc/sys/kernel/osrelease
    SysKernelOsrelease,
    /// /proc/filesystems - supported filesystems
    Filesystems,
}

impl ProcFile {
    /// Try to parse a path into a ProcFile variant.
    /// Returns None if the path is not a recognized /proc path.
    pub fn from_path(path: &str) -> Option<Self> {
        let path = path.strip_prefix("/proc")?;

        // Handle /proc/self/* paths
        if let Some(rest) = path.strip_prefix("/self") {
            return Self::parse_self_path(rest);
        }

        // Handle /proc/<pid>/* where pid matches current process
        // For now, we treat any numeric PID as "self" since we're single-process
        if let Some(rest) = path.strip_prefix('/') {
            if let Some(idx) = rest.find('/') {
                let (pid_str, remaining) = rest.split_at(idx);
                if pid_str.parse::<u32>().is_ok() {
                    return Self::parse_self_path(remaining);
                }
            }
        }

        // Handle top-level /proc files
        match path {
            "/mounts" => Some(ProcFile::Mounts),
            "/cpuinfo" => Some(ProcFile::CpuInfo),
            "/meminfo" => Some(ProcFile::MemInfo),
            "/stat" => Some(ProcFile::Stat),
            "/loadavg" => Some(ProcFile::LoadAvg),
            "/uptime" => Some(ProcFile::Uptime),
            "/version" => Some(ProcFile::Version),
            "/filesystems" => Some(ProcFile::Filesystems),
            "/sys/kernel/hostname" => Some(ProcFile::SysKernelHostname),
            "/sys/kernel/osrelease" => Some(ProcFile::SysKernelOsrelease),
            _ => None,
        }
    }

    fn parse_self_path(path: &str) -> Option<Self> {
        match path {
            "/exe" => Some(ProcFile::SelfExe),
            "/cmdline" => Some(ProcFile::SelfCmdline),
            "/environ" => Some(ProcFile::SelfEnviron),
            "/cwd" => Some(ProcFile::SelfCwd),
            "/maps" => Some(ProcFile::SelfMaps),
            "/stat" => Some(ProcFile::SelfStat),
            "/status" => Some(ProcFile::SelfStatus),
            "/mounts" => Some(ProcFile::Mounts),
            _ if path.starts_with("/fd/") => {
                let fd_str = path.strip_prefix("/fd/")?;
                let fd = fd_str.parse::<u32>().ok()?;
                Some(ProcFile::SelfFd(fd))
            }
            _ => None,
        }
    }

    /// Check if this proc file is a symlink
    pub fn is_symlink(&self) -> bool {
        matches!(
            self,
            ProcFile::SelfExe | ProcFile::SelfCwd | ProcFile::SelfFd(_)
        )
    }
}

/// Context for generating /proc content
pub struct ProcContext {
    /// Process ID
    pub pid: u32,
    /// Parent process ID
    pub ppid: u32,
    /// User ID
    pub uid: u32,
    /// Group ID
    pub gid: u32,
    /// Executable path
    pub exe_path: String,
    /// Command line arguments
    pub cmdline: Vec<String>,
    /// Environment variables
    pub environ: Vec<String>,
    /// Current working directory
    pub cwd: String,
    /// Hostname
    pub hostname: String,
}

impl Default for ProcContext {
    fn default() -> Self {
        Self {
            pid: 1,
            ppid: 0,
            uid: 0,
            gid: 0,
            exe_path: String::new(),
            cmdline: Vec::new(),
            environ: Vec::new(),
            cwd: "/".into(),
            hostname: "litebox".into(),
        }
    }
}

/// Generate content for a /proc file
pub fn generate_proc_content(file: &ProcFile, ctx: &ProcContext) -> Vec<u8> {
    match file {
        ProcFile::SelfExe => ctx.exe_path.as_bytes().to_vec(),
        ProcFile::SelfCmdline => generate_cmdline(ctx),
        ProcFile::SelfEnviron => generate_environ(ctx),
        ProcFile::SelfCwd => ctx.cwd.as_bytes().to_vec(),
        ProcFile::SelfFd(fd) => generate_fd_link(*fd),
        ProcFile::SelfMaps => generate_maps(),
        ProcFile::SelfStat => generate_self_stat(ctx),
        ProcFile::SelfStatus => generate_self_status(ctx),
        ProcFile::Mounts => generate_mounts(),
        ProcFile::CpuInfo => generate_cpuinfo(),
        ProcFile::MemInfo => generate_meminfo(),
        ProcFile::Stat => generate_stat(),
        ProcFile::LoadAvg => generate_loadavg(),
        ProcFile::Uptime => generate_uptime(),
        ProcFile::Version => generate_version(),
        ProcFile::SysKernelHostname => {
            let mut v = ctx.hostname.as_bytes().to_vec();
            v.push(b'\n');
            v
        }
        ProcFile::SysKernelOsrelease => b"6.1.0-litebox\n".to_vec(),
        ProcFile::Filesystems => generate_filesystems(),
    }
}

fn generate_cmdline(ctx: &ProcContext) -> Vec<u8> {
    let mut result = Vec::new();
    for arg in &ctx.cmdline {
        result.extend_from_slice(arg.as_bytes());
        result.push(0); // NUL separator
    }
    result
}

fn generate_environ(ctx: &ProcContext) -> Vec<u8> {
    let mut result = Vec::new();
    for var in &ctx.environ {
        result.extend_from_slice(var.as_bytes());
        result.push(0); // NUL separator
    }
    result
}

fn generate_fd_link(fd: u32) -> Vec<u8> {
    match fd {
        0 => b"/dev/stdin".to_vec(),
        1 => b"/dev/stdout".to_vec(),
        2 => b"/dev/stderr".to_vec(),
        _ => alloc::format!("pipe:[{}]", fd).into_bytes(),
    }
}

fn generate_maps() -> Vec<u8> {
    // Minimal memory map - just show the stack region
    // Format: address perms offset dev inode pathname
    b"7ffe00000000-7ffe00100000 rw-p 00000000 00:00 0                          [stack]\n".to_vec()
}

fn generate_self_stat(ctx: &ProcContext) -> Vec<u8> {
    // Format: pid (comm) state ppid pgrp session tty_nr tpgid flags ...
    // We provide minimal required fields
    let comm = ctx
        .cmdline
        .first()
        .map(|s| {
            s.rsplit('/')
                .next()
                .unwrap_or(s)
                .chars()
                .take(15)
                .collect::<String>()
        })
        .unwrap_or_else(|| "litebox".into());

    alloc::format!(
        "{} ({}) R {} {} {} 0 0 0 0 0 0 0 0 0 0 0 1 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n",
        ctx.pid, comm, ctx.ppid, ctx.pid, ctx.pid
    ).into_bytes()
}

fn generate_self_status(ctx: &ProcContext) -> Vec<u8> {
    let comm = ctx
        .cmdline
        .first()
        .map(|s| {
            s.rsplit('/')
                .next()
                .unwrap_or(s)
                .chars()
                .take(15)
                .collect::<String>()
        })
        .unwrap_or_else(|| "litebox".into());

    alloc::format!(
        "Name:\t{}\n\
         State:\tR (running)\n\
         Pid:\t{}\n\
         PPid:\t{}\n\
         Uid:\t{}\t{}\t{}\t{}\n\
         Gid:\t{}\t{}\t{}\t{}\n\
         Threads:\t1\n\
         VmSize:\t    4096 kB\n\
         VmRSS:\t    1024 kB\n",
        comm,
        ctx.pid,
        ctx.ppid,
        ctx.uid,
        ctx.uid,
        ctx.uid,
        ctx.uid,
        ctx.gid,
        ctx.gid,
        ctx.gid,
        ctx.gid
    )
    .into_bytes()
}

fn generate_mounts() -> Vec<u8> {
    // Provide minimal mount information that tools like `df` expect
    // Format: device mountpoint fstype options dump pass
    b"none / tmpfs rw,relatime 0 0\n\
      none /dev tmpfs rw,nosuid,noexec,relatime 0 0\n\
      none /tmp tmpfs rw,nosuid,nodev,relatime 0 0\n"
        .to_vec()
}

fn generate_cpuinfo() -> Vec<u8> {
    // Basic x86_64 CPU info
    b"processor\t: 0\n\
      vendor_id\t: GenuineIntel\n\
      cpu family\t: 6\n\
      model\t\t: 85\n\
      model name\t: LiteBox Virtual CPU\n\
      stepping\t: 0\n\
      cpu MHz\t\t: 2000.000\n\
      cache size\t: 4096 KB\n\
      physical id\t: 0\n\
      siblings\t: 1\n\
      core id\t\t: 0\n\
      cpu cores\t: 1\n\
      flags\t\t: fpu vme de pse tsc msr pae mce cx8 apic sep mtrr pge mca cmov pat pse36 clflush mmx fxsr sse sse2 ss syscall nx lm constant_tsc rep_good nopl cpuid pni ssse3 cx16 sse4_1 sse4_2 popcnt aes xsave avx\n\
      bogomips\t: 4000.00\n\
      clflush size\t: 64\n\
      cache_alignment\t: 64\n\
      address sizes\t: 46 bits physical, 48 bits virtual\n\n"
        .to_vec()
}

fn generate_meminfo() -> Vec<u8> {
    // Report 4GB total, mostly available
    b"MemTotal:        4194304 kB\n\
      MemFree:         3145728 kB\n\
      MemAvailable:    3670016 kB\n\
      Buffers:           65536 kB\n\
      Cached:           524288 kB\n\
      SwapCached:            0 kB\n\
      Active:           262144 kB\n\
      Inactive:         262144 kB\n\
      SwapTotal:             0 kB\n\
      SwapFree:              0 kB\n\
      Dirty:                 0 kB\n\
      Writeback:             0 kB\n\
      AnonPages:        131072 kB\n\
      Mapped:            65536 kB\n\
      Shmem:             32768 kB\n\
      Slab:              65536 kB\n\
      SReclaimable:      32768 kB\n\
      SUnreclaim:        32768 kB\n\
      KernelStack:        4096 kB\n\
      PageTables:         8192 kB\n\
      Committed_AS:     262144 kB\n\
      VmallocTotal:   34359738367 kB\n\
      VmallocUsed:       16384 kB\n\
      VmallocChunk:          0 kB\n"
        .to_vec()
}

fn generate_stat() -> Vec<u8> {
    // System-wide statistics
    // cpu line: user nice system idle iowait irq softirq steal guest guest_nice
    b"cpu  1000 0 500 100000 0 0 0 0 0 0\n\
      cpu0 1000 0 500 100000 0 0 0 0 0 0\n\
      intr 10000 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n\
      ctxt 50000\n\
      btime 1704067200\n\
      processes 100\n\
      procs_running 1\n\
      procs_blocked 0\n\
      softirq 1000 0 100 0 0 0 0 100 0 0 800\n"
        .to_vec()
}

fn generate_loadavg() -> Vec<u8> {
    // Load average: 1min 5min 15min running/total last_pid
    b"0.00 0.00 0.00 1/1 1\n".to_vec()
}

fn generate_uptime() -> Vec<u8> {
    // uptime_seconds idle_seconds
    b"3600.00 3600.00\n".to_vec()
}

fn generate_version() -> Vec<u8> {
    b"Linux version 6.1.0-litebox (litebox@litebox) (gcc version 12.0.0) #1 SMP PREEMPT_DYNAMIC Mon Jan 1 00:00:00 UTC 2024\n".to_vec()
}

fn generate_filesystems() -> Vec<u8> {
    b"nodev\ttmpfs\n\
      nodev\tramfs\n\
      nodev\tdevtmpfs\n\
      \text4\n\
      \text3\n\
      \text2\n"
        .to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn test_parse_proc_self_paths() {
        assert_eq!(
            ProcFile::from_path("/proc/self/exe"),
            Some(ProcFile::SelfExe)
        );
        assert_eq!(
            ProcFile::from_path("/proc/self/cmdline"),
            Some(ProcFile::SelfCmdline)
        );
        assert_eq!(
            ProcFile::from_path("/proc/self/fd/0"),
            Some(ProcFile::SelfFd(0))
        );
        assert_eq!(
            ProcFile::from_path("/proc/self/fd/123"),
            Some(ProcFile::SelfFd(123))
        );
    }

    #[test]
    fn test_parse_proc_pid_paths() {
        // Should treat numeric PIDs as self
        assert_eq!(ProcFile::from_path("/proc/1/exe"), Some(ProcFile::SelfExe));
        assert_eq!(
            ProcFile::from_path("/proc/123/cmdline"),
            Some(ProcFile::SelfCmdline)
        );
    }

    #[test]
    fn test_parse_proc_toplevel_paths() {
        assert_eq!(
            ProcFile::from_path("/proc/cpuinfo"),
            Some(ProcFile::CpuInfo)
        );
        assert_eq!(
            ProcFile::from_path("/proc/meminfo"),
            Some(ProcFile::MemInfo)
        );
        assert_eq!(ProcFile::from_path("/proc/mounts"), Some(ProcFile::Mounts));
        assert_eq!(
            ProcFile::from_path("/proc/version"),
            Some(ProcFile::Version)
        );
    }

    #[test]
    fn test_parse_unknown_paths() {
        assert_eq!(ProcFile::from_path("/proc/unknown"), None);
        assert_eq!(ProcFile::from_path("/not/proc"), None);
        assert_eq!(ProcFile::from_path("/proc/self/unknown"), None);
    }

    #[test]
    fn test_is_symlink() {
        assert!(ProcFile::SelfExe.is_symlink());
        assert!(ProcFile::SelfCwd.is_symlink());
        assert!(ProcFile::SelfFd(0).is_symlink());
        assert!(!ProcFile::CpuInfo.is_symlink());
        assert!(!ProcFile::Mounts.is_symlink());
    }

    #[test]
    fn test_generate_cmdline() {
        let ctx = ProcContext {
            cmdline: vec!["/bin/test".into(), "arg1".into(), "arg2".into()],
            ..Default::default()
        };
        let content = generate_cmdline(&ctx);
        assert_eq!(content, b"/bin/test\0arg1\0arg2\0");
    }

    #[test]
    fn test_generate_mounts() {
        let content = generate_mounts();
        let s = core::str::from_utf8(&content).unwrap();
        assert!(s.contains("/ tmpfs"));
    }
}
