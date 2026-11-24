//! Small helpers around the Win32 API: wide strings, long paths, handles, attributes.

use findit_core::{flags, Meta};
use std::ffi::c_void;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW, WIN32_FIND_DATAW};

pub const ATTR_READONLY: u32 = 0x1;
pub const ATTR_HIDDEN: u32 = 0x2;
pub const ATTR_SYSTEM: u32 = 0x4;
pub const ATTR_DIRECTORY: u32 = 0x10;
pub const ATTR_REPARSE_POINT: u32 = 0x400;
pub const ATTR_OFFLINE: u32 = 0x1000;
pub const ATTR_RECALL_ON_OPEN: u32 = 0x4_0000;
pub const ATTR_RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;

/// Reparse tags with this bit (junctions, symbolic links) point somewhere else.
pub const REPARSE_TAG_NAME_SURROGATE: u32 = 0x2000_0000;

/// Seconds between 1601-01-01 (FILETIME epoch) and 1970-01-01.
const FILETIME_UNIX_DIFF: i64 = 11_644_473_600;

/// A Win32 handle closed on drop.
#[derive(Debug)]
pub struct OwnedHandle(pub HANDLE);

// SAFETY: kernel handles may be used from any thread.
unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}

impl OwnedHandle {
    pub fn new(h: HANDLE) -> Option<OwnedHandle> {
        (!h.is_null() && h != INVALID_HANDLE_VALUE).then_some(OwnedHandle(h))
    }
    pub fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.0) };
        }
    }
}

pub fn last_error() -> u32 {
    unsafe { GetLastError() }
}

/// UTF-16 with a terminating NUL.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Copies `w` and appends a NUL.
pub fn with_nul(w: &[u16]) -> Vec<u16> {
    let mut v = Vec::with_capacity(w.len() + 1);
    v.extend_from_slice(w);
    v.push(0);
    v
}

/// Adds the `\\?\` prefix (or `\\?\UNC\`) so paths longer than 260 characters work.
/// Returns a NUL-terminated buffer.
pub fn long_path(path: &[u16]) -> Vec<u16> {
    let bs = u16::from(b'\\');
    let mut out = Vec::with_capacity(path.len() + 10);
    let starts = |p: &str| path.len() >= p.len() && path.iter().zip(p.encode_utf16()).all(|(a, b)| *a == b);
    if starts(r"\\?\") || starts(r"\\.\") {
        out.extend_from_slice(path);
    } else if starts(r"\\") {
        out.extend(r"\\?\UNC\".encode_utf16());
        out.extend_from_slice(&path[2..]);
    } else {
        out.extend(r"\\?\".encode_utf16());
        out.extend_from_slice(path);
        // "C:" alone means "current directory on C:"; the root needs the backslash.
        if path.len() == 2 && path[1] == u16::from(b':') {
            out.push(bs);
        }
    }
    out.push(0);
    out
}

/// Joins a folder path and a child name with a backslash.
pub fn join(dir: &[u16], name: &[u16]) -> Vec<u16> {
    let mut v = Vec::with_capacity(dir.len() + name.len() + 1);
    v.extend_from_slice(dir);
    if v.last() != Some(&u16::from(b'\\')) {
        v.push(u16::from(b'\\'));
    }
    v.extend_from_slice(name);
    v
}

/// Length of a NUL-terminated buffer.
pub fn wstr_len(buf: &[u16]) -> usize {
    buf.iter().position(|&c| c == 0).unwrap_or(buf.len())
}

/// FILETIME ticks (100 ns since 1601) to Unix seconds. 0 stays 0.
pub fn filetime_to_unix(ticks: i64) -> i64 {
    if ticks <= 0 {
        0
    } else {
        ticks / 10_000_000 - FILETIME_UNIX_DIFF
    }
}

/// Converts Win32 attributes to index flags.
pub fn flags_from_attrs(attrs: u32, reparse_tag: u32) -> u16 {
    let mut f = 0u16;
    if attrs & ATTR_DIRECTORY != 0 {
        f |= flags::DIR;
    }
    if attrs & ATTR_HIDDEN != 0 {
        f |= flags::HIDDEN;
    }
    if attrs & ATTR_SYSTEM != 0 {
        f |= flags::SYSTEM;
    }
    if attrs & ATTR_READONLY != 0 {
        f |= flags::READONLY;
    }
    if attrs & ATTR_REPARSE_POINT != 0 && reparse_tag & REPARSE_TAG_NAME_SURROGATE != 0 {
        f |= flags::LINK;
    }
    if attrs & (ATTR_RECALL_ON_OPEN | ATTR_RECALL_ON_DATA_ACCESS | ATTR_OFFLINE) != 0 {
        f |= flags::CLOUD;
    }
    f
}

/// Converts a UTF-16 name. Invalid UTF-16 (unpaired surrogates) is replaced for
/// display and the exact units are returned so the file can still be opened.
pub fn name_from_wide(w: &[u16]) -> (String, Option<Vec<u16>>) {
    match String::from_utf16(w) {
        Ok(s) => (s, None),
        Err(_) => (String::from_utf16_lossy(w), Some(w.to_vec())),
    }
}

/// One directory entry as reported by `FindFirstFileExW`.
#[derive(Clone, Debug)]
pub struct FindItem {
    pub name: String,
    pub raw: Option<Vec<u16>>,
    pub wide_name: Vec<u16>,
    pub attrs: u32,
    pub meta: Meta,
}

/// Converts `WIN32_FIND_DATAW` to a [`FindItem`].
pub fn find_item(d: &WIN32_FIND_DATAW) -> FindItem {
    let len = wstr_len(&d.cFileName);
    let wide_name = d.cFileName[..len].to_vec();
    let (name, raw) = name_from_wide(&wide_name);
    let tag = if d.dwFileAttributes & ATTR_REPARSE_POINT != 0 { d.dwReserved0 } else { 0 };
    let is_dir = d.dwFileAttributes & ATTR_DIRECTORY != 0;
    let size = if is_dir { 0 } else { (u64::from(d.nFileSizeHigh) << 32) | u64::from(d.nFileSizeLow) };
    let ticks = ((u64::from(d.ftLastWriteTime.dwHighDateTime) << 32) | u64::from(d.ftLastWriteTime.dwLowDateTime)) as i64;
    FindItem {
        name,
        raw,
        wide_name,
        attrs: d.dwFileAttributes,
        meta: Meta { size, mtime: filetime_to_unix(ticks), flags: flags_from_attrs(d.dwFileAttributes, tag) },
    }
}

/// Metadata for one existing file or folder (not a drive root), without opening it,
/// so cloud placeholders are never downloaded.
pub fn stat(path: &[u16]) -> Option<FindItem> {
    let p = long_path(path);
    let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    let h = unsafe {
        FindFirstFileExW(
            p.as_ptr(),
            FindExInfoBasic,
            &mut data as *mut WIN32_FIND_DATAW as *mut c_void,
            FindExSearchNameMatch,
            std::ptr::null(),
            0,
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return None;
    }
    unsafe { FindClose(h) };
    Some(find_item(&data))
}

/// Splits a relative path at the last backslash: (parent, name).
pub fn split_last(rel: &[u16]) -> (&[u16], &[u16]) {
    match rel.iter().rposition(|&c| c == u16::from(b'\\')) {
        Some(i) => (&rel[..i], &rel[i + 1..]),
        None => (&[], rel),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(w: &[u16]) -> String {
        String::from_utf16_lossy(&w[..wstr_len(w)])
    }

    #[test]
    fn long_path_prefixes() {
        assert_eq!(s(&long_path(&wide("C:\\a")[..4])), r"\\?\C:\a");
        assert_eq!(s(&long_path(&wide("C:")[..2])), r"\\?\C:\");
        assert_eq!(s(&long_path(&wide(r"\\srv\share\x")[..13])), r"\\?\UNC\srv\share\x");
        assert_eq!(s(&long_path(&wide(r"\\?\C:\x")[..8])), r"\\?\C:\x");
    }

    #[test]
    fn attribute_flags() {
        let f = flags_from_attrs(ATTR_DIRECTORY | ATTR_HIDDEN | ATTR_REPARSE_POINT, 0xA000_0003);
        assert!(f & flags::DIR != 0 && f & flags::HIDDEN != 0 && f & flags::LINK != 0);
        let cloud = flags_from_attrs(ATTR_RECALL_ON_DATA_ACCESS | ATTR_REPARSE_POINT, 0x9000_001A);
        assert!(cloud & flags::CLOUD != 0 && cloud & flags::LINK == 0);
        assert_eq!(filetime_to_unix(116_444_736_000_000_000), 0);
    }

    #[test]
    fn names_and_splits() {
        let bad = [0x61u16, 0xD800];
        let (n, raw) = name_from_wide(&bad);
        assert_eq!(n, "a\u{FFFD}");
        assert_eq!(raw.unwrap(), bad.to_vec());
        let w: Vec<u16> = "a\\b\\c.txt".encode_utf16().collect();
        let (p, n) = split_last(&w);
        assert_eq!(String::from_utf16_lossy(p), "a\\b");
        assert_eq!(String::from_utf16_lossy(n), "c.txt");
    }
}
