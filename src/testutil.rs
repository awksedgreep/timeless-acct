//! A throwaway `/proc` and `/sys` tree for tests.

use std::fs;
use std::path::PathBuf;

use crate::procfs::ProcRoot;

pub struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    /// A fresh directory. Anything a previous, interrupted run left under
    /// the same name is removed first, and the directory is removed again
    /// when the fixture is dropped.
    pub fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("timeless-acct-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("proc")).unwrap();
        fs::create_dir_all(dir.join("sys")).unwrap();
        Self { dir }
    }

    pub fn root(&self) -> ProcRoot {
        ProcRoot::new(self.dir.join("proc"), self.dir.join("sys"))
    }

    pub fn path(&self, relative: &str) -> PathBuf {
        self.dir.join(relative)
    }

    pub fn proc_file(&self, relative: &str, content: &str) {
        let path = self.dir.join("proc").join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    pub fn remove_proc(&self, relative: &str) {
        let _ = fs::remove_dir_all(self.dir.join("proc").join(relative));
    }

    pub fn sys_file(&self, relative: &str, content: &str) {
        let path = self.dir.join("sys").join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    pub fn sys_link(&self, relative: &str, target: &str) {
        let path = self.dir.join("sys").join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let _ = fs::remove_file(&path);
        std::os::unix::fs::symlink(target, path).unwrap();
    }

    pub fn remove_sys(&self, relative: &str) {
        let _ = fs::remove_dir_all(self.dir.join("sys").join(relative));
    }

    pub fn sys_dir(&self, relative: &str) {
        fs::create_dir_all(self.dir.join("sys").join(relative)).unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}
