//! A netlink socket, and the framing every netlink protocol shares.
//!
//! Two kernel interfaces are spoken over it: taskstats, for the accounting
//! of tasks that exit, and the process connector, for word of each exec.

use std::io;
use std::mem::size_of;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

pub const NLMSG_ERROR: u16 = 2;
pub const NLMSG_DONE: u16 = 3;
pub const NLM_F_REQUEST: u16 = 0x01;
pub const NLM_F_ACK: u16 = 0x04;

pub const NLMSG_HEADER: usize = 16;

/// Room for a burst of events between two reads. The kernel drops what
/// does not fit, and reports that it did.
const RECEIVE_BUFFER_BYTES: libc::c_int = 8 * 1024 * 1024;

pub fn align4(len: usize) -> usize {
    (len + 3) & !3
}

/// One netlink message: its type and what follows the netlink header.
pub struct Message<'a> {
    pub kind: u16,
    pub body: &'a [u8],
}

/// Walk the messages in one datagram. A malformed length ends the walk.
pub fn messages(mut bytes: &[u8]) -> impl Iterator<Item = Message<'_>> {
    std::iter::from_fn(move || {
        if bytes.len() < NLMSG_HEADER {
            return None;
        }
        let len = u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        let kind = u16::from_ne_bytes([bytes[4], bytes[5]]);
        if len < NLMSG_HEADER || len > bytes.len() {
            return None;
        }
        let body = &bytes[NLMSG_HEADER..len];
        bytes = &bytes[align4(len).min(bytes.len())..];
        Some(Message { kind, body })
    })
}

/// A netlink header followed by `body`, padded to the alignment.
pub fn message(kind: u16, flags: u16, body: &[u8]) -> Vec<u8> {
    let total = NLMSG_HEADER + body.len();
    let mut out = Vec::with_capacity(align4(total));
    out.extend_from_slice(&(total as u32).to_ne_bytes());
    out.extend_from_slice(&kind.to_ne_bytes());
    out.extend_from_slice(&flags.to_ne_bytes());
    out.extend_from_slice(&1_u32.to_ne_bytes()); // sequence
    out.extend_from_slice(&0_u32.to_ne_bytes()); // port: the kernel fills it
    out.extend_from_slice(body);
    out.resize(align4(total), 0);
    out
}

/// The error in an `NLMSG_ERROR` body; `None` for an acknowledgement.
pub fn error_of(body: &[u8]) -> Option<io::Error> {
    let code = i32::from_ne_bytes(body.get(..4)?.try_into().ok()?);
    if code == 0 {
        None
    } else {
        Some(io::Error::from_raw_os_error(-code))
    }
}

/// What one read produced.
pub enum Received {
    /// A datagram of this many bytes is in the buffer.
    Data(usize),
    /// Nothing arrived within the timeout.
    Idle,
    /// The kernel dropped events because the buffer was full.
    Overrun,
}

pub struct Netlink {
    fd: OwnedFd,
}

impl Netlink {
    /// Open a socket of `protocol`, subscribed to the multicast `groups`.
    pub fn open(protocol: libc::c_int, groups: u32) -> io::Result<Self> {
        // SAFETY: socket() has no memory preconditions; the descriptor is
        // owned from here on and closed on drop.
        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                protocol,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };

        // SAFETY: sockaddr_nl is plain data; a zero port is "let the kernel
        // choose".
        let mut address: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        address.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        address.nl_groups = groups;
        let rc = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                (&address as *const libc::sockaddr_nl).cast(),
                size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd })
    }

    fn set_option<T>(&self, option: libc::c_int, value: &T) -> io::Result<()> {
        // SAFETY: the pointer and length describe `value` exactly.
        let rc = unsafe {
            libc::setsockopt(
                self.fd.as_raw_fd(),
                libc::SOL_SOCKET,
                option,
                (value as *const T).cast(),
                size_of::<T>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// How long a read waits before it reports that nothing arrived.
    pub fn set_timeout(&self, timeout: Duration) -> io::Result<()> {
        let value = libc::timeval {
            tv_sec: timeout.as_secs() as libc::time_t,
            tv_usec: timeout.subsec_micros() as libc::suseconds_t,
        };
        self.set_option(libc::SO_RCVTIMEO, &value)
    }

    /// Make room for a burst. The forced variant is not capped by
    /// `net.core.rmem_max`; it needs `CAP_NET_ADMIN`, and the plain one is
    /// the fallback when it is refused.
    pub fn grow_receive_buffer(&self) -> io::Result<()> {
        if self
            .set_option(libc::SO_RCVBUFFORCE, &RECEIVE_BUFFER_BYTES)
            .is_err()
        {
            self.set_option(libc::SO_RCVBUF, &RECEIVE_BUFFER_BYTES)?;
        }
        Ok(())
    }

    pub fn send(&self, bytes: &[u8]) -> io::Result<()> {
        // SAFETY: as in bind; a zero port addresses the kernel.
        let mut kernel: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        kernel.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        let sent = unsafe {
            libc::sendto(
                self.fd.as_raw_fd(),
                bytes.as_ptr().cast(),
                bytes.len(),
                0,
                (&kernel as *const libc::sockaddr_nl).cast(),
                size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            )
        };
        if sent < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// Read one datagram into `buf`.
    pub fn receive(&self, buf: &mut [u8]) -> io::Result<Received> {
        // SAFETY: the pointer and length describe `buf` exactly.
        let len = unsafe { libc::recv(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
        if len >= 0 {
            return Ok(Received::Data(len as usize));
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EAGAIN) | Some(libc::EINTR) => Ok(Received::Idle),
            Some(libc::ENOBUFS) => Ok(Received::Overrun),
            _ => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_is_framed_and_padded() {
        let bytes = message(27, NLM_F_REQUEST, &[1, 2, 3, 4, 5]);
        assert_eq!(bytes.len(), 24);
        // The header's own length excludes the padding.
        assert_eq!(u32::from_ne_bytes(bytes[..4].try_into().unwrap()), 21);
        let read: Vec<_> = messages(&bytes).collect();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].kind, 27);
        assert_eq!(read[0].body, [1, 2, 3, 4, 5]);
    }

    #[test]
    fn several_messages_in_one_datagram_are_all_walked() {
        let mut bytes = message(1, 0, &[9; 3]);
        bytes.extend(message(2, 0, &[8; 8]));
        let kinds: Vec<u16> = messages(&bytes).map(|m| m.kind).collect();
        assert_eq!(kinds, [1, 2]);
    }

    #[test]
    fn a_malformed_length_ends_the_walk_without_reading_past_the_end() {
        let mut bytes = message(1, 0, &[0; 64]);
        bytes.truncate(40);
        assert_eq!(messages(&bytes).count(), 0);
        assert_eq!(messages(&[0xff; 7]).count(), 0);
        assert_eq!(messages(&[0; 16]).count(), 0);
    }

    #[test]
    fn an_acknowledgement_is_not_an_error() {
        assert!(error_of(&0_i32.to_ne_bytes()).is_none());
        let denied = error_of(&(-libc::EPERM).to_ne_bytes()).unwrap();
        assert_eq!(denied.kind(), io::ErrorKind::PermissionDenied);
    }
}
