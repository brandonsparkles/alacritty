use std::collections::{HashSet, VecDeque};
use std::ffi::{CStr, CString, IntoStringError};
use std::fmt::{self, Display, Formatter};
use std::io;
use std::mem::{self, MaybeUninit};
use std::os::raw::{c_int, c_void};
use std::path::PathBuf;

/// Error during working directory retrieval.
#[derive(Debug)]
pub enum Error {
    Io(io::Error),

    /// Error converting into utf8 string.
    IntoString(IntoStringError),

    /// Expected return size didn't match libproc's.
    InvalidSize,
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::InvalidSize => None,
            Error::Io(err) => err.source(),
            Error::IntoString(err) => err.source(),
        }
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Error::InvalidSize => write!(f, "Invalid proc_pidinfo return size"),
            Error::Io(err) => write!(f, "Error getting current working directory: {}", err),
            Error::IntoString(err) => {
                write!(f, "Error when parsing current working directory: {}", err)
            },
        }
    }
}

impl From<io::Error> for Error {
    fn from(val: io::Error) -> Self {
        Error::Io(val)
    }
}

impl From<IntoStringError> for Error {
    fn from(val: IntoStringError) -> Self {
        Error::IntoString(val)
    }
}

/// Return the direct children of `parent_pid`.
///
/// Uses `proc_listpids(PROC_PPID_ONLY, ppid, …)` which enumerates the
/// kernel's PIDs whose ppid matches. This is more reliable than checking
/// the TTY's foreground process group, because TUIs like Codex/Copilot may
/// share their parent shell's process group while still being children.
pub fn list_children(parent_pid: c_int) -> Vec<c_int> {
    // First call with a null buffer asks the kernel how many bytes it needs.
    let needed = unsafe {
        sys::proc_listpids(sys::PROC_PPID_ONLY, parent_pid as u32, std::ptr::null_mut(), 0)
    };
    if needed <= 0 {
        return Vec::new();
    }

    let count = (needed as usize) / std::mem::size_of::<c_int>();
    // Add slack — the set of children can grow between the size query and the read.
    let mut buf: Vec<c_int> = vec![0; count + 8];
    let buf_bytes = (buf.len() * std::mem::size_of::<c_int>()) as c_int;
    let actual = unsafe {
        sys::proc_listpids(
            sys::PROC_PPID_ONLY,
            parent_pid as u32,
            buf.as_mut_ptr() as *mut c_void,
            buf_bytes,
        )
    };
    if actual <= 0 {
        return Vec::new();
    }
    let n = (actual as usize) / std::mem::size_of::<c_int>();
    // The kernel can write trailing zero entries; ignore them.
    buf.into_iter().take(n).filter(|&p| p > 0).collect()
}

/// Return descendants of `parent_pid`, excluding `parent_pid` itself.
///
/// AI CLIs often run through one or more wrapper processes. Walking a bounded
/// tree lets tab activity use one marker for codex, claude, copilot, and their
/// wrapped binaries without chasing unrelated process trees.
pub fn list_descendants(parent_pid: c_int, max_depth: usize) -> Vec<c_int> {
    let mut descendants = Vec::new();
    let mut visited = HashSet::new();
    let mut queue = VecDeque::from([(parent_pid, 0usize)]);
    visited.insert(parent_pid);

    while let Some((pid, depth)) = queue.pop_front() {
        if depth >= max_depth {
            continue;
        }

        for child in list_children(pid) {
            if !visited.insert(child) {
                continue;
            }

            descendants.push(child);
            queue.push_back((child, depth + 1));
        }
    }

    descendants
}

/// Return the full filesystem path of the binary backing `pid`, resolving
/// any symlinks the kernel followed at exec time. `None` if the process is
/// gone or unreadable.
///
/// We use this in `cli_resume.rs` to identify AI CLIs whose user-visible
/// command (e.g. `claude`) is a symlink into a version-pinned binary
/// (`/.claude/versions/<x.y.z>`) — `pbi_comm` is set from the resolved
/// basename and so reads as `"2.1.144"`, not `"claude"`. The binary path
/// keeps the full prefix and is the only reliable signal.
pub fn pid_path(pid: c_int) -> Option<std::path::PathBuf> {
    // PROC_PIDPATHINFO_MAXSIZE is 4 * MAXPATHLEN = 4096 on macOS.
    let mut buf = vec![0u8; 4096];
    let n = unsafe { sys::proc_pidpath(pid, buf.as_mut_ptr() as *mut c_void, buf.len() as u32) };
    if n <= 0 {
        return None;
    }
    buf.truncate(n as usize);
    let s = std::str::from_utf8(&buf).ok()?.trim_end_matches('\0');
    if s.is_empty() {
        return None;
    }
    Some(std::path::PathBuf::from(s))
}

/// Return the short executable name of `pid` (`pbi_comm`, capped at 15 chars
/// by the kernel). `None` if the process can't be queried.
///
/// Currently unused — `cli_resume.rs` switched to `pid_path` after we
/// learned `pbi_comm` returns the symlink-resolved basename (e.g.
/// `"2.1.146"` for claude, not `"claude"`). Kept available for future
/// callers that want the truncated name regardless.
#[allow(dead_code)]
pub fn comm(pid: c_int) -> Option<String> {
    let mut info = MaybeUninit::<sys::proc_bsdinfo>::uninit();
    let size = mem::size_of::<sys::proc_bsdinfo>() as c_int;
    let res = unsafe {
        sys::proc_pidinfo(pid, sys::PROC_PIDTBSDINFO, 0, info.as_mut_ptr() as *mut c_void, size)
    };
    if res != size {
        return None;
    }
    let info = unsafe { info.assume_init() };
    // `pbi_comm` is NUL-padded; build a Rust string from it safely.
    let bytes: Vec<u8> = info.pbi_comm.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8(bytes).ok()
}

/// Wall-clock time (Unix seconds) when `pid` started, via the kernel's
/// `pbi_start_tvsec`. `None` on any error. Used by `cli_resume.rs` to
/// match a running codex process to its rollout `.jsonl` file (codex
/// writes no PID-keyed metadata, but its rollout filenames encode the
/// session-start timestamp, so we can join by proximity).
pub fn start_tvsec(pid: c_int) -> Option<u64> {
    let mut info = MaybeUninit::<sys::proc_bsdinfo>::uninit();
    let size = mem::size_of::<sys::proc_bsdinfo>() as c_int;
    let res = unsafe {
        sys::proc_pidinfo(pid, sys::PROC_PIDTBSDINFO, 0, info.as_mut_ptr() as *mut c_void, size)
    };
    if res != size {
        return None;
    }
    let info = unsafe { info.assume_init() };
    Some(info.pbi_start_tvsec)
}

/// Return the process argv via `KERN_PROCARGS2`.
///
/// Used by `cli_resume.rs` to preserve an explicit `codex resume <uuid>`
/// command after a restored Codex process is running. The rollout file's
/// timestamp can be much older than the current process when a session was
/// resumed, so argv is the durable signal in that case.
pub fn argv(pid: c_int) -> Option<Vec<String>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut buf = vec![0u8; 256 * 1024];
    let mut len = buf.len();
    let res = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            buf.as_mut_ptr() as *mut c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if res != 0 || len <= std::mem::size_of::<c_int>() {
        return None;
    }
    buf.truncate(len);

    let argc = c_int::from_ne_bytes(buf[..std::mem::size_of::<c_int>()].try_into().ok()?);
    let argc = usize::try_from(argc).ok()?;
    if argc == 0 {
        return None;
    }

    let mut idx = std::mem::size_of::<c_int>();
    while idx < buf.len() && buf[idx] != 0 {
        idx += 1;
    }
    while idx < buf.len() && buf[idx] == 0 {
        idx += 1;
    }

    let mut args = Vec::with_capacity(argc);
    for _ in 0..argc {
        if idx >= buf.len() {
            break;
        }
        let start = idx;
        while idx < buf.len() && buf[idx] != 0 {
            idx += 1;
        }
        if idx > start {
            args.push(String::from_utf8_lossy(&buf[start..idx]).into_owned());
        }
        while idx < buf.len() && buf[idx] == 0 {
            idx += 1;
        }
    }

    (!args.is_empty()).then_some(args)
}

pub fn cwd(pid: c_int) -> Result<PathBuf, Error> {
    let mut info = MaybeUninit::<sys::proc_vnodepathinfo>::uninit();
    let info_ptr = info.as_mut_ptr() as *mut c_void;
    let size = mem::size_of::<sys::proc_vnodepathinfo>() as c_int;

    let c_str = unsafe {
        let pidinfo_size = sys::proc_pidinfo(pid, sys::PROC_PIDVNODEPATHINFO, 0, info_ptr, size);
        match pidinfo_size {
            c if c < 0 => return Err(io::Error::last_os_error().into()),
            s if s != size => return Err(Error::InvalidSize),
            _ => CStr::from_ptr(info.assume_init().pvi_cdir.vip_path.as_ptr()),
        }
    };

    Ok(CString::from(c_str).into_string().map(PathBuf::from)?)
}

/// Bindings for libproc.
#[allow(non_camel_case_types)]
mod sys {
    use std::os::raw::{c_char, c_int, c_longlong, c_void};

    pub const PROC_PIDVNODEPATHINFO: c_int = 9;
    pub const PROC_PIDTBSDINFO: c_int = 3;

    /// `proc_listpids` selector: return PIDs whose parent matches `typeinfo`.
    pub const PROC_PPID_ONLY: u32 = 6;

    // Process states from <sys/proc.h>.
    type gid_t = c_int;
    type off_t = c_longlong;
    type uid_t = c_int;
    type fsid_t = fsid;

    #[repr(C)]
    #[derive(Debug, Copy, Clone)]
    pub struct fsid {
        pub val: [i32; 2usize],
    }

    #[repr(C)]
    #[derive(Debug, Copy, Clone)]
    pub struct vinfo_stat {
        pub vst_dev: u32,
        pub vst_mode: u16,
        pub vst_nlink: u16,
        pub vst_ino: u64,
        pub vst_uid: uid_t,
        pub vst_gid: gid_t,
        pub vst_atime: i64,
        pub vst_atimensec: i64,
        pub vst_mtime: i64,
        pub vst_mtimensec: i64,
        pub vst_ctime: i64,
        pub vst_ctimensec: i64,
        pub vst_birthtime: i64,
        pub vst_birthtimensec: i64,
        pub vst_size: off_t,
        pub vst_blocks: i64,
        pub vst_blksize: i32,
        pub vst_flags: u32,
        pub vst_gen: u32,
        pub vst_rdev: u32,
        pub vst_qspare: [i64; 2usize],
    }

    #[repr(C)]
    #[derive(Debug, Copy, Clone)]
    pub struct vnode_info {
        pub vi_stat: vinfo_stat,
        pub vi_type: c_int,
        pub vi_pad: c_int,
        pub vi_fsid: fsid_t,
    }

    #[repr(C)]
    #[derive(Copy, Clone)]
    pub struct vnode_info_path {
        pub vip_vi: vnode_info,
        pub vip_path: [c_char; 1024usize],
    }

    #[repr(C)]
    #[derive(Copy, Clone)]
    pub struct proc_vnodepathinfo {
        pub pvi_cdir: vnode_info_path,
        pub pvi_rdir: vnode_info_path,
    }

    /// Layout matches `struct proc_bsdinfo` from `<sys/proc_info.h>`. We only
    /// read `pbi_status`; the other fields are present to keep the struct's
    /// size correct (proc_pidinfo validates the buffer size against this).
    #[repr(C)]
    #[derive(Copy, Clone)]
    pub struct proc_bsdinfo {
        pub pbi_flags: u32,
        pub pbi_status: u32,
        pub pbi_xstatus: u32,
        pub pbi_pid: u32,
        pub pbi_ppid: u32,
        pub pbi_uid: uid_t,
        pub pbi_gid: gid_t,
        pub pbi_ruid: uid_t,
        pub pbi_rgid: gid_t,
        pub pbi_svuid: uid_t,
        pub pbi_svgid: gid_t,
        pub rfu_1: u32,
        pub pbi_comm: [c_char; 16],
        pub pbi_name: [c_char; 32],
        pub pbi_nfiles: u32,
        pub pbi_pgid: u32,
        pub pbi_pjobc: u32,
        pub e_tdev: u32,
        pub e_tpgid: u32,
        pub pbi_nice: i32,
        pub pbi_start_tvsec: u64,
        pub pbi_start_tvusec: u64,
    }

    unsafe extern "C" {
        pub fn proc_pidinfo(
            pid: c_int,
            flavor: c_int,
            arg: u64,
            buffer: *mut c_void,
            buffersize: c_int,
        ) -> c_int;

        pub fn proc_pidpath(pid: c_int, buffer: *mut c_void, buffersize: u32) -> c_int;

        pub fn proc_listpids(
            r#type: u32,
            typeinfo: u32,
            buffer: *mut c_void,
            buffersize: c_int,
        ) -> c_int;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::{env, process};

    #[test]
    fn cwd_matches_current_dir() {
        assert_eq!(cwd(process::id() as i32).ok(), env::current_dir().ok());
    }
}
