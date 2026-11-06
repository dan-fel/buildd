//! What the daemon compares and measures about a file: its identity, the disk
//! it takes and when it changed.

use std::path::Path;

/// One file's facts. Two readings that differ mean the file changed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Facts {
    /// The volume and file index: hard links to a file share these.
    pub(crate) dev: u64,
    pub(crate) inode: u64,
    pub(crate) mode: u32,
    pub(crate) is_dir: bool,
    pub(crate) len: u64,
    pub(crate) links: u64,
    /// The disk the file takes.
    pub(crate) bytes: u64,
    pub(crate) modified: (i64, i64),
    pub(crate) changed: (i64, i64),
}

impl Facts {
    /// The facts of the file at `path`, which `metadata` was read from
    /// without following a link.
    ///
    /// # Errors
    /// When the file cannot be read, which only Windows asks to do here.
    #[cfg(unix)]
    pub(crate) fn read(_path: &Path, metadata: &std::fs::Metadata) -> std::io::Result<Self> {
        use std::os::unix::fs::MetadataExt as _;

        Ok(Self {
            dev: metadata.dev(),
            inode: metadata.ino(),
            mode: metadata.mode(),
            is_dir: metadata.is_dir(),
            len: metadata.len(),
            links: metadata.nlink(),
            bytes: metadata.blocks() * 512,
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        })
    }

    /// The facts of the file at `path`, which `metadata` was read from
    /// without following a link. Windows reports a file's identity and links
    /// only through an open handle, and has no separate change time.
    ///
    /// # Errors
    /// When the file cannot be opened.
    #[cfg(windows)]
    pub(crate) fn read(path: &Path, metadata: &std::fs::Metadata) -> std::io::Result<Self> {
        use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle,
        };

        /// NTFS's usual allocation unit, which file sizes round up to.
        const CLUSTER: u64 = 4096;

        let file = std::fs::OpenOptions::new()
            .access_mode(0)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        // SAFETY: the structure holds integers, for which zeroes are valid.
        let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: the handle is open and `information` is valid for writes.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &raw mut information) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let modified = metadata.last_write_time();
        let modified = (
            i64::try_from(modified / 10_000_000).expect("a file time's seconds fit i64"),
            i64::try_from(modified % 10_000_000).expect("below a second") * 100,
        );
        Ok(Self {
            dev: u64::from(information.dwVolumeSerialNumber),
            inode: (u64::from(information.nFileIndexHigh) << 32)
                | u64::from(information.nFileIndexLow),
            mode: information.dwFileAttributes,
            is_dir: metadata.is_dir(),
            len: metadata.len(),
            links: u64::from(information.nNumberOfLinks),
            bytes: metadata.len().next_multiple_of(CLUSTER),
            modified,
            changed: modified,
        })
    }
}
