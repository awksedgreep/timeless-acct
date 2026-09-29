//! Reading `/proc` and `/sys`.
//!
//! Parsers are pure functions over text so they can be tested against
//! captured files; only this module touches the filesystem. The roots are
//! configurable so tests can point at a fixture tree.

pub mod process;
pub mod system;

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct ProcRoot {
    proc: PathBuf,
    sys: PathBuf,
}

impl Default for ProcRoot {
    fn default() -> Self {
        Self::new("/proc", "/sys")
    }
}

impl ProcRoot {
    pub fn new(proc: impl Into<PathBuf>, sys: impl Into<PathBuf>) -> Self {
        Self {
            proc: proc.into(),
            sys: sys.into(),
        }
    }

    pub fn proc_path(&self, relative: &str) -> PathBuf {
        self.proc.join(relative)
    }

    pub fn sys_path(&self, relative: &str) -> PathBuf {
        self.sys.join(relative)
    }

    pub fn read(&self, relative: &str) -> io::Result<String> {
        fs::read_to_string(self.proc.join(relative))
    }

    pub fn read_sys(&self, relative: &str) -> io::Result<String> {
        fs::read_to_string(self.sys.join(relative))
    }

    /// Read one per-process file into a reused buffer. A sweep reads
    /// thousands of these, so the allocation is worth keeping.
    pub fn read_pid(&self, pid: u32, file: &str, buf: &mut String) -> io::Result<()> {
        buf.clear();
        let mut path = self.proc.join(itoa(pid));
        path.push(file);
        read_into(&path, buf)
    }

    pub fn pid_path(&self, pid: u32, file: &str) -> PathBuf {
        let mut path = self.proc.join(itoa(pid));
        path.push(file);
        path
    }

    /// Every numeric directory under the proc root.
    pub fn pids(&self) -> io::Result<Vec<u32>> {
        let mut pids = Vec::with_capacity(512);
        for entry in fs::read_dir(&self.proc)? {
            let entry = entry?;
            if let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse().ok()) {
                pids.push(pid);
            }
        }
        Ok(pids)
    }
}

/// What a process is, as opposed to what it is using: read when it is
/// first seen, and again when it calls exec.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Described {
    pub cmdline: String,
    pub exe: String,
    /// Its control group's path on the unified hierarchy.
    pub cgroup: String,
}

impl ProcRoot {
    /// Each part is empty if it could not be read: the process is gone, or
    /// belongs to someone else.
    pub fn describe(&self, pid: u32, cmdline_max: usize, buf: &mut String) -> Described {
        let cmdline = match self.read_pid(pid, "cmdline", buf) {
            Ok(()) => process::parse_cmdline(buf, cmdline_max),
            Err(_) => String::new(),
        };
        let exe = fs::read_link(self.pid_path(pid, "exe"))
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();
        Described {
            cmdline,
            exe,
            cgroup: self.cgroup_of(pid, buf),
        }
    }

    pub fn cgroup_of(&self, pid: u32, buf: &mut String) -> String {
        match self.read_pid(pid, "cgroup", buf) {
            Ok(()) => crate::cgroup::parse_proc_cgroup(buf)
                .unwrap_or_default()
                .to_string(),
            Err(_) => String::new(),
        }
    }
}

fn itoa(value: u32) -> String {
    value.to_string()
}

fn read_into(path: &Path, buf: &mut String) -> io::Result<()> {
    let mut file = fs::File::open(path)?;
    // Proc files hold bytes the kernel copied from user memory (a command
    // name, an argument vector); they are not guaranteed to be UTF-8.
    let mut bytes = Vec::with_capacity(1024);
    file.read_to_end(&mut bytes)?;
    buf.push_str(&String::from_utf8_lossy(&bytes));
    Ok(())
}
