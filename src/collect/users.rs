//! uid to user name, asked once per uid.

use std::collections::HashMap;
use std::ffi::CStr;

#[derive(Debug, Default)]
pub struct Users {
    names: HashMap<u32, String>,
}

impl Users {
    /// The account name, or the number itself when the uid has no account
    /// (a container's user, a deleted account).
    pub fn name(&mut self, uid: u32) -> &str {
        self.names.entry(uid).or_insert_with(|| lookup(uid))
    }
}

fn lookup(uid: u32) -> String {
    let mut buf = vec![0_u8; 4096];
    loop {
        // SAFETY: passwd is plain data the call fills in; every pointer it
        // stores points into `buf`, which outlives the read below.
        let mut passwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let rc = unsafe {
            libc::getpwuid_r(
                uid,
                &mut passwd,
                buf.as_mut_ptr().cast(),
                buf.len(),
                &mut result,
            )
        };
        if rc == libc::ERANGE && buf.len() < (1 << 20) {
            buf.resize(buf.len() * 4, 0);
            continue;
        }
        if rc != 0 || result.is_null() || passwd.pw_name.is_null() {
            return uid.to_string();
        }
        // SAFETY: on success pw_name is a NUL-terminated string inside buf.
        let name = unsafe { CStr::from_ptr(passwd.pw_name) };
        return name.to_string_lossy().into_owned();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_has_a_name_and_an_unknown_uid_is_its_number() {
        let mut users = Users::default();
        assert_eq!(users.name(0), "root");
        assert_eq!(users.name(4_000_000_123), "4000000123");
    }
}
