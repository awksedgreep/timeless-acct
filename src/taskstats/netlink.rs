//! The generic netlink conversation with the kernel's taskstats family.
//!
//! Small enough to speak directly: resolve the family's id, register for
//! exit records on every CPU, then read datagrams.

use std::io;
use std::time::Duration;

use crate::netlink::{
    align4, error_of, message, messages, Netlink, Received, NLMSG_DONE, NLMSG_ERROR, NLM_F_ACK,
    NLM_F_REQUEST,
};

const NETLINK_GENERIC: libc::c_int = 16;
const GENL_ID_CTRL: u16 = 0x10;

const CTRL_CMD_GETFAMILY: u8 = 3;
const CTRL_ATTR_FAMILY_ID: u16 = 1;
const CTRL_ATTR_FAMILY_NAME: u16 = 2;

const TASKSTATS_GENL_NAME: &[u8] = b"TASKSTATS\0";
const TASKSTATS_GENL_VERSION: u8 = 1;
const TASKSTATS_CMD_GET: u8 = 1;
const TASKSTATS_CMD_ATTR_REGISTER_CPUMASK: u16 = 3;
const TASKSTATS_CMD_ATTR_DEREGISTER_CPUMASK: u16 = 4;

pub const TASKSTATS_TYPE_STATS: u16 = 3;
pub const TASKSTATS_TYPE_AGGR_PID: u16 = 4;

const GENL_HEADER: usize = 4;
const ATTR_HEADER: usize = 4;
/// Flag bits the kernel may set in an attribute's type.
const NLA_TYPE_MASK: u16 = 0x3fff;

/// One attribute: its type with flag bits removed, and its payload.
pub struct Attr<'a> {
    pub kind: u16,
    pub payload: &'a [u8],
}

/// Walk a run of attributes. A malformed length ends the walk.
pub fn attrs(mut bytes: &[u8]) -> impl Iterator<Item = Attr<'_>> {
    std::iter::from_fn(move || {
        if bytes.len() < ATTR_HEADER {
            return None;
        }
        let len = u16::from_ne_bytes([bytes[0], bytes[1]]) as usize;
        let kind = u16::from_ne_bytes([bytes[2], bytes[3]]) & NLA_TYPE_MASK;
        if len < ATTR_HEADER || len > bytes.len() {
            return None;
        }
        let payload = &bytes[ATTR_HEADER..len];
        bytes = &bytes[align4(len).min(bytes.len())..];
        Some(Attr { kind, payload })
    })
}

/// A generic netlink request carrying one attribute.
fn request(
    family: u16,
    flags: u16,
    command: u8,
    version: u8,
    kind: u16,
    payload: &[u8],
) -> Vec<u8> {
    let attr_len = ATTR_HEADER + payload.len();
    let attr_len = u16::try_from(attr_len).expect("taskstats payload fits in an attribute");
    let mut body = Vec::with_capacity(GENL_HEADER + align4(attr_len as usize));
    body.push(command);
    body.push(version);
    body.extend_from_slice(&0_u16.to_ne_bytes());
    body.extend_from_slice(&attr_len.to_ne_bytes());
    body.extend_from_slice(&kind.to_ne_bytes());
    body.extend_from_slice(payload);
    body.resize(GENL_HEADER + align4(attr_len as usize), 0);
    message(family, flags, &body)
}

pub struct Socket {
    netlink: Netlink,
    family: u16,
    cpus: Vec<u8>,
}

impl Socket {
    /// Open a socket and register for the exit records of every CPU in
    /// `cpus`, a kernel CPU list such as `0-21`.
    ///
    /// Fails with `PermissionDenied` without `CAP_NET_ADMIN`.
    pub fn listen(cpus: &str, timeout: Duration) -> io::Result<Self> {
        let mut socket = Self {
            netlink: Netlink::open(NETLINK_GENERIC, 0)?,
            family: 0,
            cpus: Vec::new(),
        };
        socket.netlink.set_timeout(Duration::from_secs(2))?;
        socket.family = socket.resolve_family()?;
        socket.netlink.grow_receive_buffer()?;

        let trimmed = cpus.trim();
        if trimmed.is_empty() || trimmed.bytes().any(|b| b == 0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty CPU list for taskstats registration",
            ));
        }
        let mut mask = trimmed.as_bytes().to_vec();
        mask.push(0);
        socket.command(TASKSTATS_CMD_ATTR_REGISTER_CPUMASK, &mask)?;
        socket.cpus = mask;
        socket.netlink.set_timeout(timeout)?;
        Ok(socket)
    }

    /// Read one datagram into `buf`.
    pub fn receive(&self, buf: &mut [u8]) -> io::Result<Received> {
        self.netlink.receive(buf)
    }

    fn resolve_family(&self) -> io::Result<u16> {
        self.netlink.send(&request(
            GENL_ID_CTRL,
            NLM_F_REQUEST,
            CTRL_CMD_GETFAMILY,
            1,
            CTRL_ATTR_FAMILY_NAME,
            TASKSTATS_GENL_NAME,
        ))?;
        let mut buf = vec![0_u8; 8192];
        let Received::Data(len) = self.netlink.receive(&mut buf)? else {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "no reply resolving the taskstats family",
            ));
        };
        for message in messages(&buf[..len]) {
            if message.kind == NLMSG_ERROR {
                return Err(error_of(message.body).unwrap_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "taskstats family not found")
                }));
            }
            if message.kind != GENL_ID_CTRL || message.body.len() < GENL_HEADER {
                continue;
            }
            for attr in attrs(&message.body[GENL_HEADER..]) {
                if attr.kind == CTRL_ATTR_FAMILY_ID && attr.payload.len() >= 2 {
                    return Ok(u16::from_ne_bytes([attr.payload[0], attr.payload[1]]));
                }
            }
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "the kernel has no taskstats family (CONFIG_TASKSTATS)",
        ))
    }

    /// Send a taskstats command and wait for the kernel's verdict.
    fn command(&self, kind: u16, payload: &[u8]) -> io::Result<()> {
        self.netlink.send(&request(
            self.family,
            NLM_F_REQUEST | NLM_F_ACK,
            TASKSTATS_CMD_GET,
            TASKSTATS_GENL_VERSION,
            kind,
            payload,
        ))?;
        let mut buf = vec![0_u8; 8192];
        // Exit records may already be arriving; the verdict is the first
        // error-typed message among them. Bounded so a storm of records
        // cannot hold registration forever; large so it does not time out
        // spuriously while they arrive. Replies are not correlated by
        // sequence: one command is outstanding at a time.
        for _ in 0..1024 {
            let Received::Data(len) = self.netlink.receive(&mut buf)? else {
                break;
            };
            for message in messages(&buf[..len]) {
                if message.kind == NLMSG_ERROR {
                    return match error_of(message.body) {
                        Some(error) => Err(error),
                        None => Ok(()),
                    };
                }
            }
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "no acknowledgement from the taskstats family",
        ))
    }

    pub fn family(&self) -> u16 {
        self.family
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        if !self.cpus.is_empty() {
            // Closing the socket removes the listener as well; this only
            // makes it prompt. Nothing can be done about a failure here.
            let _ = self.netlink.send(&request(
                self.family,
                NLM_F_REQUEST,
                TASKSTATS_CMD_GET,
                TASKSTATS_GENL_VERSION,
                TASKSTATS_CMD_ATTR_DEREGISTER_CPUMASK,
                &self.cpus,
            ));
        }
    }
}

/// The exit records in one datagram, as the raw bytes of each
/// `struct taskstats`.
pub fn exit_records(datagram: &[u8], family: u16) -> Vec<&[u8]> {
    let mut records = Vec::new();
    for message in messages(datagram) {
        if message.kind == NLMSG_DONE || message.kind != family {
            continue;
        }
        if message.body.len() < GENL_HEADER {
            continue;
        }
        for attr in attrs(&message.body[GENL_HEADER..]) {
            // The per-thread-group aggregate carries delay totals only, not
            // the accounting fields; the per-task records are summed here
            // instead.
            if attr.kind != TASKSTATS_TYPE_AGGR_PID {
                continue;
            }
            for inner in attrs(attr.payload) {
                if inner.kind == TASKSTATS_TYPE_STATS {
                    records.push(inner.payload);
                }
            }
        }
    }
    records
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::netlink::NLMSG_HEADER;

    fn attr(kind: u16, payload: &[u8]) -> Vec<u8> {
        let len = ATTR_HEADER + payload.len();
        let mut out = Vec::new();
        out.extend_from_slice(&(len as u16).to_ne_bytes());
        out.extend_from_slice(&kind.to_ne_bytes());
        out.extend_from_slice(payload);
        out.resize(align4(len), 0);
        out
    }

    /// An exit message the way the kernel builds one: a padding attribute
    /// ahead of the struct to align it, inside a nested attribute.
    pub(crate) fn exit_message(family: u16, aggregate: u16, stats: &[u8]) -> Vec<u8> {
        const TASKSTATS_TYPE_PID: u16 = 1;
        const TASKSTATS_TYPE_NULL: u16 = 6;
        const NLA_F_NESTED: u16 = 0x8000;
        let mut nested = attr(TASKSTATS_TYPE_PID, &4242_u32.to_ne_bytes());
        nested.extend(attr(TASKSTATS_TYPE_NULL, &[]));
        nested.extend(attr(TASKSTATS_TYPE_STATS, stats));
        let mut body = vec![2, 1, 0, 0];
        body.extend(attr(aggregate | NLA_F_NESTED, &nested));
        message(family, 0, &body)
    }

    #[test]
    fn exit_records_are_found_inside_the_nesting_and_past_the_padding() {
        let stats = [7_u8; 688];
        let datagram = exit_message(27, TASKSTATS_TYPE_AGGR_PID, &stats);
        let records = exit_records(&datagram, 27);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0], &stats[..]);
    }

    #[test]
    fn several_messages_in_one_datagram_are_all_read() {
        let mut datagram = exit_message(27, TASKSTATS_TYPE_AGGR_PID, &[1; 688]);
        datagram.extend(exit_message(27, TASKSTATS_TYPE_AGGR_PID, &[2; 688]));
        let records = exit_records(&datagram, 27);
        assert_eq!(records.len(), 2);
        assert_eq!(records[1][0], 2);
    }

    #[test]
    fn thread_group_aggregates_and_other_families_are_skipped() {
        const TASKSTATS_TYPE_AGGR_TGID: u16 = 5;
        let aggregate = exit_message(27, TASKSTATS_TYPE_AGGR_TGID, &[1; 688]);
        assert!(exit_records(&aggregate, 27).is_empty());
        let foreign = exit_message(99, TASKSTATS_TYPE_AGGR_PID, &[1; 688]);
        assert!(exit_records(&foreign, 27).is_empty());
    }

    #[test]
    fn a_truncated_datagram_yields_nothing_rather_than_garbage() {
        let mut datagram = exit_message(27, TASKSTATS_TYPE_AGGR_PID, &[1; 688]);
        let cut = datagram.len() - 100;
        datagram.truncate(cut);
        assert!(exit_records(&datagram, 27).is_empty());
        assert_eq!(attrs(&[0, 0, 1, 0]).count(), 0);
    }

    #[test]
    fn a_request_is_padded_to_the_alignment() {
        let bytes = request(27, NLM_F_REQUEST, 1, 1, 3, b"0-21\0");
        assert_eq!(bytes.len(), NLMSG_HEADER + GENL_HEADER + 12);
        // The attribute's own length excludes the padding.
        assert_eq!(u16::from_ne_bytes([bytes[20], bytes[21]]), 9);
    }
}
