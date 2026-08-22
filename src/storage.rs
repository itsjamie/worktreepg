//! Whether a filesystem can share blocks between a template and its clone. Postgres 18's
//! `file_copy_method = clone` asks the kernel to clone; what the kernel does depends on the
//! filesystem under the data directory, and nothing reports that back, so it is inferred here.

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sharing {
    /// Clones share blocks (btrfs, bcachefs, APFS).
    Shared,
    /// Depends on how the filesystem was created (XFS needs `reflink=1`, ZFS needs block cloning).
    Depends,
    /// The kernel falls back to a full copy.
    Copied,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filesystem {
    pub name: String,
    pub sharing: Sharing,
}

/// The filesystem holding `path`, or `None` when it cannot be determined from this host.
pub fn filesystem(path: &Path) -> Option<Filesystem> {
    let name = filesystem_name(path)?;
    let sharing = match name.as_str() {
        "btrfs" | "bcachefs" | "apfs" => Sharing::Shared,
        "xfs" | "zfs" => Sharing::Depends,
        _ => Sharing::Copied,
    };
    Some(Filesystem { name, sharing })
}

#[cfg(target_os = "linux")]
fn filesystem_name(path: &Path) -> Option<String> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: c_path is a valid NUL-terminated string and buf is a properly sized out-parameter.
    if unsafe { libc::statfs(c_path.as_ptr(), &mut buf) } != 0 {
        return None;
    }
    // Magic numbers from linux/magic.h.
    let name = match buf.f_type as u64 {
        0x9123_683E => "btrfs",
        0xCA45_1A4E => "bcachefs",
        0x5846_5342 => "xfs",
        0x2FC1_2FC1 => "zfs",
        0xEF53 => "ext4",
        0x0001_854D => "tmpfs",
        0x794C_7630 => "overlay",
        0x6969 => "nfs",
        0x4D44 => "vfat",
        0xF2F5_2010 => "f2fs",
        _ => return Some(format!("fs:0x{:x}", buf.f_type)),
    };
    Some(name.to_string())
}

#[cfg(any(target_os = "macos", target_os = "freebsd"))]
fn filesystem_name(path: &Path) -> Option<String> {
    use std::ffi::{CStr, CString};
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    if unsafe { libc::statfs(c_path.as_ptr(), &mut buf) } != 0 {
        return None;
    }
    let name = unsafe { CStr::from_ptr(buf.f_fstypename.as_ptr()) };
    Some(name.to_string_lossy().to_lowercase())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "freebsd")))]
fn filesystem_name(_path: &Path) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_known_filesystems() {
        assert_eq!(filesystem(Path::new("/definitely/not/here")), None);
        let root = filesystem(Path::new("/")).expect("root filesystem is readable");
        assert!(!root.name.is_empty());
    }
}
