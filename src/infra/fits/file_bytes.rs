//! Byte-source policy for whole-file FITS reads: mmap on local filesystems,
//! one big sequential `read` on network mounts.
//!
//! Why: on NFS/Lustre/SMB-style mounts, mmap is the wrong tool --
//! (1) an I/O failure on a mapped page (server timeout, stale handle, file
//! truncated by another client) raises SIGBUS and kills the whole process
//! instead of surfacing as an `io::Error` for one request; (2) page-fault-
//! driven access defeats the NFS client's readahead, which performs far
//! better for large sequential `read(2)` calls; (3) concurrent modification
//! of a mapped file by another client is undefined behavior. A single
//! sequential read of the whole file is close to optimal on such mounts,
//! and the decode pipeline downstream is already `&[u8]`-based so it never
//! sees the difference.
//!
//! Policy: `ASTROBURST_IO_MODE` = `auto` (default) | `mmap` | `read`, read
//! once per process (OnceLock -- changing it requires a restart). `auto`
//! detects the filesystem type per-file via `fstatfs(2)` on Linux and picks
//! `read` for known network/remote filesystems, `mmap` otherwise; on
//! non-Linux platforms `auto` means `mmap` (today's behavior). The env
//! override is the escape hatch for network filesystems the detection list
//! doesn't know about.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::ops::Deref;
use std::str::FromStr;
use std::sync::OnceLock;

use anyhow::{Context, Result};
use memmap2::{Mmap, MmapOptions};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoMode {
    /// Detect per-file: network filesystem -> `Read`, otherwise `Mmap`.
    Auto,
    /// Always memory-map (previous unconditional behavior).
    Mmap,
    /// Always read the whole file into an owned buffer.
    Read,
}

impl FromStr for IoMode {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(IoMode::Auto),
            "mmap" => Ok(IoMode::Mmap),
            "read" => Ok(IoMode::Read),
            other => Err(format!("invalid io mode '{other}' (expected auto|mmap|read)")),
        }
    }
}

/// Process-wide policy from `ASTROBURST_IO_MODE`, defaulting to `Auto`
/// (warn + default on an unparseable value, mirroring the server config's
/// `env_or` behavior).
pub fn io_mode() -> IoMode {
    static MODE: OnceLock<IoMode> = OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("ASTROBURST_IO_MODE") {
        Err(_) => IoMode::Auto,
        Ok(raw) => raw.parse().unwrap_or_else(|e| {
            log::warn!("ASTROBURST_IO_MODE: {e}; using auto");
            IoMode::Auto
        }),
    })
}

/// Whole-file bytes, from either a memory mapping or an owned buffer.
/// Derefs to `&[u8]` so downstream code is agnostic to the source.
pub enum FileBytes {
    Mapped(Mmap),
    Owned(Vec<u8>),
}

impl Deref for FileBytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            FileBytes::Mapped(m) => m,
            FileBytes::Owned(v) => v,
        }
    }
}

impl AsRef<[u8]> for FileBytes {
    fn as_ref(&self) -> &[u8] {
        self
    }
}

/// Resolve the mmap-vs-read decision for `file` under `mode`. Also used by
/// the lazy cube reader (`core/cube/lazy.rs`) to pick between its mapped
/// and pread-based sources.
pub(crate) fn prefer_mmap(file: &File, mode: IoMode) -> bool {
    match mode {
        IoMode::Mmap => true,
        IoMode::Read => false,
        IoMode::Auto => !is_network_fs(file),
    }
}

/// Read a file's full contents per the process-wide [`io_mode`] policy.
pub fn read_file_bytes(file: &File) -> Result<FileBytes> {
    read_file_bytes_with_mode(file, io_mode())
}

/// Mode-explicit variant, used directly by tests (the env-driven [`io_mode`]
/// is cached process-wide, and mutating env vars in parallel tests races).
pub fn read_file_bytes_with_mode(file: &File, mode: IoMode) -> Result<FileBytes> {
    let use_mmap = prefer_mmap(file, mode);

    if use_mmap {
        let mmap = unsafe { MmapOptions::new().map(file).context("mmap failed")? };
        #[cfg(unix)]
        {
            let _ = mmap.advise(memmap2::Advice::Sequential);
        }
        Ok(FileBytes::Mapped(mmap))
    } else {
        let len = file.metadata().map(|m| m.len() as usize).unwrap_or(0);
        let mut buf = Vec::with_capacity(len);
        let mut f = file;
        f.seek(SeekFrom::Start(0)).context("seek failed")?;
        f.read_to_end(&mut buf).context("file read failed")?;
        Ok(FileBytes::Owned(buf))
    }
}

/// `fstatfs(2)` `f_type` magics for network/remote filesystems where mmap
/// page-fault I/O is a poor fit (see module docs). Curated, not exhaustive
/// -- an unknown network FS falls back to mmap; `ASTROBURST_IO_MODE=read`
/// is the override. Values from `linux/magic.h` and module sources.
#[cfg(target_os = "linux")]
const NETWORK_FS_MAGICS: &[u64] = &[
    0x6969,       // NFS
    0x517B,       // SMB
    0xFE534D42,   // SMB2
    0xFF534D42,   // CIFS
    0x0BD00BD0,   // Lustre
    0x00C36400,   // Ceph
    0x65735546,   // FUSE (sshfs, s3fs, many network-backed mounts)
    0x01021997,   // 9p (VM shared folders)
    0x47504653,   // GPFS / IBM Spectrum Scale
    0x01161970,   // GFS2
    0x7461636F,   // OCFS2
    0x5346414F,   // AFS
    0x73757245,   // Coda
];

#[cfg(target_os = "linux")]
fn is_network_fs(file: &File) -> bool {
    use std::os::unix::io::AsRawFd;
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::fstatfs(file.as_raw_fd(), &mut stat) };
    if rc != 0 {
        // Detection failure: preserve previous (mmap) behavior.
        return false;
    }
    // f_type is __fsword_t (i64) on glibc and c_ulong on musl; the magics
    // are all small positives, so comparing through u64 is exact for both.
    let ftype = stat.f_type as u64;
    NETWORK_FS_MAGICS.contains(&ftype)
}

#[cfg(not(target_os = "linux"))]
fn is_network_fs(_file: &File) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn io_mode_parses_expected_values() {
        assert_eq!("auto".parse::<IoMode>().unwrap(), IoMode::Auto);
        assert_eq!("MMAP".parse::<IoMode>().unwrap(), IoMode::Mmap);
        assert_eq!(" read ".parse::<IoMode>().unwrap(), IoMode::Read);
        assert!("fast".parse::<IoMode>().is_err());
    }

    #[test]
    fn mapped_and_owned_yield_identical_bytes() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        let content: Vec<u8> = (0..10_000u32).flat_map(|v| v.to_be_bytes()).collect();
        tmp.write_all(&content).unwrap();
        tmp.flush().unwrap();

        let file = File::open(tmp.path()).unwrap();
        let mapped = read_file_bytes_with_mode(&file, IoMode::Mmap).unwrap();
        let owned = read_file_bytes_with_mode(&file, IoMode::Read).unwrap();
        assert!(matches!(mapped, FileBytes::Mapped(_)));
        assert!(matches!(owned, FileBytes::Owned(_)));
        assert_eq!(&*mapped, &content[..]);
        assert_eq!(&*owned, &content[..]);
    }

    #[test]
    fn owned_read_is_independent_of_prior_cursor_position() {
        // read_file_bytes must rewind: a caller may have already read from
        // the same File handle (e.g. a format-sniffing probe).
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"0123456789").unwrap();
        tmp.flush().unwrap();

        let mut file = File::open(tmp.path()).unwrap();
        let mut probe = [0u8; 4];
        std::io::Read::read_exact(&mut file, &mut probe).unwrap();

        let owned = read_file_bytes_with_mode(&file, IoMode::Read).unwrap();
        assert_eq!(&*owned, b"0123456789");
    }
}
