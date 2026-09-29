//! Decoding `struct taskstats`.
//!
//! The struct is versioned and only ever grows at the end, so a field is
//! read by its fixed offset if the record is long enough to hold it and is
//! absent otherwise. The offsets are those of `<linux/taskstats.h>` on
//! x86-64 and aarch64, which share the layout: the struct's alignment
//! attributes pin every 64-bit field to an 8-byte boundary.

/// Offsets into `struct taskstats`, in bytes.
mod offset {
    pub const VERSION: usize = 0;
    pub const AC_EXITCODE: usize = 4;
    pub const AC_FLAG: usize = 8;
    pub const AC_NICE: usize = 9;
    pub const CPU_DELAY_TOTAL: usize = 24;
    pub const BLKIO_DELAY_TOTAL: usize = 40;
    pub const SWAPIN_DELAY_TOTAL: usize = 56;
    pub const AC_COMM: usize = 80;
    pub const AC_UID: usize = 120;
    pub const AC_GID: usize = 124;
    pub const AC_PID: usize = 128;
    pub const AC_PPID: usize = 132;
    pub const AC_BTIME: usize = 136;
    pub const AC_ETIME: usize = 144;
    pub const AC_UTIME: usize = 152;
    pub const AC_STIME: usize = 160;
    pub const AC_MINFLT: usize = 168;
    pub const AC_MAJFLT: usize = 176;
    pub const HIWATER_RSS: usize = 200;
    pub const HIWATER_VM: usize = 208;
    pub const READ_CHAR: usize = 216;
    pub const WRITE_CHAR: usize = 224;
    pub const READ_BYTES: usize = 248;
    pub const WRITE_BYTES: usize = 256;
    pub const NVCSW: usize = 272;
    pub const NIVCSW: usize = 280;
    pub const FREEPAGES_DELAY_TOTAL: usize = 320;
    pub const THRASHING_DELAY_TOTAL: usize = 336;
    pub const AC_BTIME64: usize = 344;
    pub const AC_TGID: usize = 368;
    pub const AC_TGETIME: usize = 376;
    pub const AC_EXE_DEV: usize = 384;
    pub const AC_EXE_INODE: usize = 392;
}

const COMM_LEN: usize = 32;

/// `ac_flag`: the task was made by fork and never called exec. Set on a
/// thread group's leader only.
pub const AFORK: u8 = 0x01;
/// `ac_flag`: the task dumped core.
pub const ACORE: u8 = 0x08;
/// `ac_flag`: the task was ended by a signal, its own or the one the kernel
/// sends the other threads of a process that is exiting.
#[cfg(test)]
pub const AXSIG: u8 = 0x10;
/// `ac_flag`: the task was the last of its thread group. Version 12 on.
pub const AGROUP: u8 = 0x20;

/// The first version that carries the thread group id and `AGROUP`.
pub const VERSION_WITH_THREAD_GROUP: u16 = 12;

/// What the kernel reports when one task (a thread) exits.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskExit {
    pub version: u16,
    /// A wait status: the signal in the low seven bits, the exit code in
    /// the second byte.
    pub exit_status: u32,
    pub flag: u8,
    pub nice: i8,
    pub comm: String,
    pub uid: u32,
    pub gid: u32,
    /// The thread's id. Equal to `tgid` for a thread group's leader.
    pub pid: u32,
    pub ppid: u32,
    pub tgid: u32,
    /// Epoch seconds.
    pub start_epoch: u64,
    pub elapsed_us: u64,
    /// Wall time of the whole thread group. Version 12 on.
    pub group_elapsed_us: u64,
    pub user_us: u64,
    pub system_us: u64,
    pub minor_faults: u64,
    pub major_faults: u64,
    pub peak_rss_kb: u64,
    pub peak_vm_kb: u64,
    pub read_char: u64,
    pub write_char: u64,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub voluntary_switches: u64,
    pub involuntary_switches: u64,
    pub cpu_delay_ns: u64,
    pub blkio_delay_ns: u64,
    pub swapin_delay_ns: u64,
    pub reclaim_delay_ns: u64,
    pub thrashing_delay_ns: u64,
    pub exe_dev: u64,
    pub exe_inode: u64,
}

impl TaskExit {
    pub fn is_last_in_group(&self) -> bool {
        self.flag & AGROUP != 0
    }

    pub fn has_thread_group(&self) -> bool {
        self.version >= VERSION_WITH_THREAD_GROUP
    }
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn bytes<const N: usize>(&self, at: usize) -> Option<[u8; N]> {
        self.0.get(at..at + N)?.try_into().ok()
    }

    fn u8(&self, at: usize) -> u8 {
        self.0.get(at).copied().unwrap_or(0)
    }

    fn u16(&self, at: usize) -> u16 {
        self.bytes(at).map_or(0, u16::from_ne_bytes)
    }

    fn u32(&self, at: usize) -> u32 {
        self.bytes(at).map_or(0, u32::from_ne_bytes)
    }

    fn u64(&self, at: usize) -> u64 {
        self.bytes(at).map_or(0, u64::from_ne_bytes)
    }
}

/// Decode one record. `None` when the bytes are too short to hold even the
/// fields every version has.
pub fn parse(bytes: &[u8]) -> Option<TaskExit> {
    use offset::*;
    // Through nivcsw: present since the versions of the 2.6 kernels.
    if bytes.len() < NIVCSW + 8 {
        return None;
    }
    let r = Reader(bytes);

    let comm = &bytes[AC_COMM..AC_COMM + COMM_LEN];
    let comm_end = comm.iter().position(|b| *b == 0).unwrap_or(COMM_LEN);
    let comm = String::from_utf8_lossy(&comm[..comm_end]).into_owned();

    let pid = r.u32(AC_PID);
    let start64 = r.u64(AC_BTIME64);
    let has_group = r.u16(VERSION) >= VERSION_WITH_THREAD_GROUP;

    Some(TaskExit {
        version: r.u16(VERSION),
        exit_status: r.u32(AC_EXITCODE),
        flag: r.u8(AC_FLAG),
        nice: r.u8(AC_NICE) as i8,
        comm,
        uid: r.u32(AC_UID),
        gid: r.u32(AC_GID),
        pid,
        ppid: r.u32(AC_PPID),
        tgid: if has_group { r.u32(AC_TGID) } else { pid },
        start_epoch: if start64 != 0 {
            start64
        } else {
            u64::from(r.u32(AC_BTIME))
        },
        elapsed_us: r.u64(AC_ETIME),
        group_elapsed_us: if has_group { r.u64(AC_TGETIME) } else { 0 },
        user_us: r.u64(AC_UTIME),
        system_us: r.u64(AC_STIME),
        minor_faults: r.u64(AC_MINFLT),
        major_faults: r.u64(AC_MAJFLT),
        peak_rss_kb: r.u64(HIWATER_RSS),
        peak_vm_kb: r.u64(HIWATER_VM),
        read_char: r.u64(READ_CHAR),
        write_char: r.u64(WRITE_CHAR),
        read_bytes: r.u64(READ_BYTES),
        write_bytes: r.u64(WRITE_BYTES),
        voluntary_switches: r.u64(NVCSW),
        involuntary_switches: r.u64(NIVCSW),
        cpu_delay_ns: r.u64(CPU_DELAY_TOTAL),
        blkio_delay_ns: r.u64(BLKIO_DELAY_TOTAL),
        swapin_delay_ns: r.u64(SWAPIN_DELAY_TOTAL),
        reclaim_delay_ns: r.u64(FREEPAGES_DELAY_TOTAL),
        thrashing_delay_ns: r.u64(THRASHING_DELAY_TOTAL),
        exe_dev: if has_group { r.u64(AC_EXE_DEV) } else { 0 },
        exe_inode: if has_group { r.u64(AC_EXE_INODE) } else { 0 },
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Build a record the way the kernel lays it out.
    pub(crate) fn encode(exit: &TaskExit, len: usize) -> Vec<u8> {
        use offset::*;
        let mut bytes = vec![0_u8; len];
        let mut put = |at: usize, value: &[u8]| {
            if at + value.len() <= len {
                bytes[at..at + value.len()].copy_from_slice(value);
            }
        };
        put(VERSION, &exit.version.to_ne_bytes());
        put(AC_EXITCODE, &exit.exit_status.to_ne_bytes());
        put(AC_FLAG, &[exit.flag]);
        put(AC_NICE, &[exit.nice as u8]);
        put(AC_COMM, exit.comm.as_bytes());
        put(AC_UID, &exit.uid.to_ne_bytes());
        put(AC_GID, &exit.gid.to_ne_bytes());
        put(AC_PID, &exit.pid.to_ne_bytes());
        put(AC_PPID, &exit.ppid.to_ne_bytes());
        put(AC_BTIME, &(exit.start_epoch as u32).to_ne_bytes());
        put(AC_BTIME64, &exit.start_epoch.to_ne_bytes());
        put(AC_TGID, &exit.tgid.to_ne_bytes());
        for (at, value) in [
            (AC_ETIME, exit.elapsed_us),
            (AC_TGETIME, exit.group_elapsed_us),
            (AC_UTIME, exit.user_us),
            (AC_STIME, exit.system_us),
            (AC_MINFLT, exit.minor_faults),
            (AC_MAJFLT, exit.major_faults),
            (HIWATER_RSS, exit.peak_rss_kb),
            (HIWATER_VM, exit.peak_vm_kb),
            (READ_CHAR, exit.read_char),
            (WRITE_CHAR, exit.write_char),
            (READ_BYTES, exit.read_bytes),
            (WRITE_BYTES, exit.write_bytes),
            (NVCSW, exit.voluntary_switches),
            (NIVCSW, exit.involuntary_switches),
            (CPU_DELAY_TOTAL, exit.cpu_delay_ns),
            (BLKIO_DELAY_TOTAL, exit.blkio_delay_ns),
            (SWAPIN_DELAY_TOTAL, exit.swapin_delay_ns),
            (FREEPAGES_DELAY_TOTAL, exit.reclaim_delay_ns),
            (THRASHING_DELAY_TOTAL, exit.thrashing_delay_ns),
            (AC_EXE_DEV, exit.exe_dev),
            (AC_EXE_INODE, exit.exe_inode),
        ] {
            put(at, &value.to_ne_bytes());
        }
        bytes
    }

    pub(crate) fn sample() -> TaskExit {
        TaskExit {
            version: 17,
            exit_status: 0x0100,
            flag: AGROUP,
            nice: -5,
            comm: "cc1plus".into(),
            uid: 1000,
            gid: 1000,
            pid: 4242,
            ppid: 4000,
            tgid: 4242,
            start_epoch: 1_753_000_000,
            elapsed_us: 2_500_000,
            group_elapsed_us: 2_500_000,
            user_us: 1_900_000,
            system_us: 300_000,
            minor_faults: 50_000,
            major_faults: 3,
            peak_rss_kb: 512_000,
            peak_vm_kb: 900_000,
            read_char: 10_000_000,
            write_char: 2_000_000,
            read_bytes: 4096,
            write_bytes: 1_048_576,
            voluntary_switches: 40,
            involuntary_switches: 900,
            cpu_delay_ns: 150_000_000,
            blkio_delay_ns: 20_000_000,
            swapin_delay_ns: 0,
            reclaim_delay_ns: 1_000,
            thrashing_delay_ns: 2_000,
            exe_dev: 66_306,
            exe_inode: 123_456,
        }
    }

    #[test]
    fn a_current_record_decodes_every_field() {
        let exit = sample();
        assert_eq!(parse(&encode(&exit, 688)).unwrap(), exit);
    }

    #[test]
    fn a_record_from_an_older_kernel_has_no_thread_group() {
        let mut exit = sample();
        exit.version = 9;
        // Version 9 ends after the thrashing delay fields.
        let decoded = parse(&encode(&exit, 344)).unwrap();
        assert_eq!(decoded.version, 9);
        assert_eq!(decoded.comm, "cc1plus");
        assert_eq!(decoded.user_us, 1_900_000);
        assert_eq!(decoded.start_epoch, 1_753_000_000);
        // Without a thread group id, each task is its own group.
        assert_eq!(decoded.tgid, decoded.pid);
        assert_eq!(decoded.group_elapsed_us, 0);
        assert_eq!(decoded.exe_inode, 0);
        assert!(!decoded.has_thread_group());
    }

    #[test]
    fn a_truncated_record_is_rejected() {
        assert!(parse(&encode(&sample(), 688)[..100]).is_none());
        assert!(parse(&[]).is_none());
    }

    #[test]
    fn a_command_name_that_fills_the_field_has_no_terminator() {
        let mut exit = sample();
        exit.comm = "x".repeat(32);
        assert_eq!(parse(&encode(&exit, 688)).unwrap().comm, "x".repeat(32));
    }
}
