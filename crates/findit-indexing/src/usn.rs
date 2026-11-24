//! NTFS fast mode: master file table enumeration and the USN change journal.
//!
//! * `FSCTL_ENUM_USN_DATA` lists every file record on the volume (names, parents,
//!   attributes) in seconds. It is documented and much safer than parsing the raw MFT.
//! * Sizes and dates are not part of those records, so they are filled in by listing
//!   each folder by id (`OpenFileById` + `FileIdBothDirectoryInfo`). Reading a
//!   folder never opens the files in it, so cloud placeholders stay online-only.
//! * `FSCTL_READ_USN_JOURNAL` reports later changes.
//!
//! Opening a volume handle needs administrator rights, so this runs in the elevated
//! helper (or in the app itself if the user started it as administrator).

use crate::win::{filetime_to_unix, last_error, name_from_wide, wide, OwnedHandle};
use std::ffi::c_void;
use std::io;
use std::mem::{offset_of, size_of};
use windows_sys::Win32::Foundation::{
    ERROR_HANDLE_EOF, ERROR_JOURNAL_DELETE_IN_PROGRESS, ERROR_JOURNAL_ENTRY_DELETED, ERROR_JOURNAL_NOT_ACTIVE, ERROR_NO_MORE_FILES,
    GENERIC_READ,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FileIdBothDirectoryInfo, FileIdBothDirectoryRestartInfo, FileIdType, GetFileInformationByHandle,
    GetFileInformationByHandleEx, OpenFileById, BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_BOTH_DIR_INFO,
    FILE_ID_DESCRIPTOR, FILE_ID_DESCRIPTOR_0, FILE_LIST_DIRECTORY, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    SYNCHRONIZE,
};
use windows_sys::Win32::System::Ioctl::{
    FSCTL_ENUM_USN_DATA, FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL, MFT_ENUM_DATA_V0, READ_USN_JOURNAL_DATA_V0, USN_JOURNAL_DATA_V0,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

pub const REASON_FILE_DELETE: u32 = 0x0000_0200;
pub const REASON_RENAME_OLD_NAME: u32 = 0x0000_1000;
pub const REASON_RENAME_NEW_NAME: u32 = 0x0000_2000;

/// Low 48 bits of a file reference number: the MFT record index.
pub fn record_index(frn: u64) -> u64 {
    frn & 0x0000_FFFF_FFFF_FFFF
}

/// MFT records 0-15 are NTFS metadata files ($MFT, $LogFile, ...); 5 is the root.
pub fn is_metafile(frn: u64) -> bool {
    let i = record_index(frn);
    i < 16 && i != 5
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JournalInfo {
    pub id: u64,
    pub first_usn: i64,
    pub next_usn: i64,
}

/// One USN record (V2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawRecord {
    pub frn: u64,
    pub parent: u64,
    pub usn: i64,
    pub reason: u32,
    pub attrs: u32,
    pub name: Vec<u16>,
}

impl RawRecord {
    pub fn name(&self) -> (String, Option<Vec<u16>>) {
        name_from_wide(&self.name)
    }
}

/// One entry of a folder listed by id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub frn: u64,
    pub attrs: u32,
    pub size: u64,
    pub mtime: i64,
    /// The name in this folder (differs between hard links of one file).
    pub name: Vec<u16>,
}

#[derive(Debug)]
pub enum JournalError {
    /// Records we have not read yet were overwritten (journal wrapped).
    Wrapped,
    /// The journal was deleted or disabled.
    Gone,
    Io(io::Error),
}

/// An open NTFS volume.
pub struct UsnVolume {
    pub letter: char,
    volume: OwnedHandle,
    root: OwnedHandle,
    pub root_frn: u64,
}

impl std::fmt::Debug for UsnVolume {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UsnVolume").field("letter", &self.letter).finish_non_exhaustive()
    }
}

fn rd_u16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn rd_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn rd_u64(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}

/// Parses a buffer of `USN_RECORD_V2` records with bounds checks.
pub fn parse_records(data: &[u8]) -> Vec<RawRecord> {
    const MIN: usize = 60;
    let mut out = Vec::new();
    let mut off = 0usize;
    while off + MIN <= data.len() {
        let len = rd_u32(data, off) as usize;
        if len < MIN || off + len > data.len() {
            break;
        }
        let rec = &data[off..off + len];
        if rd_u16(rec, 4) == 2 {
            let name_len = rd_u16(rec, 56) as usize;
            let name_off = rd_u16(rec, 58) as usize;
            if let Some(nb) = rec.get(name_off..name_off + name_len) {
                out.push(RawRecord {
                    frn: rd_u64(rec, 8),
                    parent: rd_u64(rec, 16),
                    usn: rd_u64(rec, 24) as i64,
                    reason: rd_u32(rec, 40),
                    attrs: rd_u32(rec, 52),
                    name: nb.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect(),
                });
            }
        }
        off += len;
    }
    out
}

impl UsnVolume {
    /// Opens `X:` for enumeration. Fails without administrator rights.
    pub fn open(letter: char) -> io::Result<UsnVolume> {
        let letter = letter.to_ascii_uppercase();
        if !letter.is_ascii_uppercase() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "bad drive letter"));
        }
        let share = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
        let vpath = wide(&format!("\\\\.\\{letter}:"));
        let volume = OwnedHandle::new(unsafe {
            CreateFileW(vpath.as_ptr(), GENERIC_READ, share, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut())
        })
        .ok_or_else(io::Error::last_os_error)?;
        let rpath = wide(&format!("{letter}:\\"));
        let root = OwnedHandle::new(unsafe {
            CreateFileW(
                rpath.as_ptr(),
                FILE_LIST_DIRECTORY | SYNCHRONIZE,
                share,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        })
        .ok_or_else(io::Error::last_os_error)?;
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(root.raw(), &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let root_frn = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
        Ok(UsnVolume { letter, volume, root, root_frn })
    }

    /// Current journal id and range.
    pub fn query_journal(&self) -> io::Result<JournalInfo> {
        let mut data: USN_JOURNAL_DATA_V0 = unsafe { std::mem::zeroed() };
        let mut ret = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                self.volume.raw(),
                FSCTL_QUERY_USN_JOURNAL,
                std::ptr::null(),
                0,
                &mut data as *mut USN_JOURNAL_DATA_V0 as *mut c_void,
                size_of::<USN_JOURNAL_DATA_V0>() as u32,
                &mut ret,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(JournalInfo { id: data.UsnJournalID, first_usn: data.FirstUsn, next_usn: data.NextUsn })
    }

    /// Streams every file record up to `high_usn` to `on_batch`. Return `false`
    /// from the callback to stop early.
    pub fn enumerate(&self, high_usn: i64, mut on_batch: impl FnMut(Vec<RawRecord>) -> bool) -> io::Result<()> {
        let mut med = MFT_ENUM_DATA_V0 { StartFileReferenceNumber: 0, LowUsn: 0, HighUsn: high_usn };
        let mut buf = vec![0u64; (1 << 20) / 8];
        loop {
            let mut ret = 0u32;
            let ok = unsafe {
                DeviceIoControl(
                    self.volume.raw(),
                    FSCTL_ENUM_USN_DATA,
                    &med as *const MFT_ENUM_DATA_V0 as *const c_void,
                    size_of::<MFT_ENUM_DATA_V0>() as u32,
                    buf.as_mut_ptr() as *mut c_void,
                    (buf.len() * 8) as u32,
                    &mut ret,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                let e = last_error();
                if e == ERROR_HANDLE_EOF {
                    return Ok(());
                }
                return Err(io::Error::from_raw_os_error(e as i32));
            }
            let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, (ret as usize).min(buf.len() * 8)) };
            if bytes.len() < 8 {
                return Ok(());
            }
            let next = rd_u64(bytes, 0);
            if !on_batch(parse_records(&bytes[8..])) {
                return Ok(());
            }
            if next <= med.StartFileReferenceNumber {
                return Ok(());
            }
            med.StartFileReferenceNumber = next;
        }
    }

    /// Reads journal records from `start`. Returns the records and the USN to
    /// continue from.
    pub fn read_changes(&self, journal_id: u64, start: i64, max_records: usize) -> Result<(Vec<RawRecord>, i64), JournalError> {
        let mut rd = READ_USN_JOURNAL_DATA_V0 {
            StartUsn: start,
            ReasonMask: 0xFFFF_FFFF,
            ReturnOnlyOnClose: 0,
            Timeout: 0,
            BytesToWaitFor: 0,
            UsnJournalID: journal_id,
        };
        let mut buf = vec![0u64; (256 << 10) / 8];
        let mut all = Vec::new();
        loop {
            let mut ret = 0u32;
            let ok = unsafe {
                DeviceIoControl(
                    self.volume.raw(),
                    FSCTL_READ_USN_JOURNAL,
                    &rd as *const READ_USN_JOURNAL_DATA_V0 as *const c_void,
                    size_of::<READ_USN_JOURNAL_DATA_V0>() as u32,
                    buf.as_mut_ptr() as *mut c_void,
                    (buf.len() * 8) as u32,
                    &mut ret,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                let e = last_error();
                return Err(match e {
                    ERROR_JOURNAL_ENTRY_DELETED => JournalError::Wrapped,
                    ERROR_JOURNAL_DELETE_IN_PROGRESS | ERROR_JOURNAL_NOT_ACTIVE => JournalError::Gone,
                    _ => JournalError::Io(io::Error::from_raw_os_error(e as i32)),
                });
            }
            let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, (ret as usize).min(buf.len() * 8)) };
            if bytes.len() < 8 {
                return Ok((all, rd.StartUsn));
            }
            let next = rd_u64(bytes, 0) as i64;
            let records = parse_records(&bytes[8..]);
            if records.is_empty() || next <= rd.StartUsn {
                return Ok((all, next.max(rd.StartUsn)));
            }
            all.extend(records);
            rd.StartUsn = next;
            if all.len() >= max_records {
                return Ok((all, next));
            }
        }
    }

    /// Lists a folder by its file reference number.
    pub fn list_dir_by_id(&self, dir_frn: u64) -> io::Result<Vec<DirEntry>> {
        let desc = FILE_ID_DESCRIPTOR {
            dwSize: size_of::<FILE_ID_DESCRIPTOR>() as u32,
            Type: FileIdType,
            Anonymous: FILE_ID_DESCRIPTOR_0 { FileId: dir_frn as i64 },
        };
        let h = OwnedHandle::new(unsafe {
            OpenFileById(
                self.root.raw(),
                &desc,
                FILE_LIST_DIRECTORY | SYNCHRONIZE,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                FILE_FLAG_BACKUP_SEMANTICS,
            )
        })
        .ok_or_else(io::Error::last_os_error)?;
        let mut buf = vec![0u64; (64 << 10) / 8];
        let mut class = FileIdBothDirectoryRestartInfo;
        let mut out = Vec::new();
        let o_next = offset_of!(FILE_ID_BOTH_DIR_INFO, NextEntryOffset);
        let o_write = offset_of!(FILE_ID_BOTH_DIR_INFO, LastWriteTime);
        let o_eof = offset_of!(FILE_ID_BOTH_DIR_INFO, EndOfFile);
        let o_attr = offset_of!(FILE_ID_BOTH_DIR_INFO, FileAttributes);
        let o_nlen = offset_of!(FILE_ID_BOTH_DIR_INFO, FileNameLength);
        let o_id = offset_of!(FILE_ID_BOTH_DIR_INFO, FileId);
        let o_name = offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
        loop {
            let ok = unsafe { GetFileInformationByHandleEx(h.raw(), class, buf.as_mut_ptr() as *mut c_void, (buf.len() * 8) as u32) };
            if ok == 0 {
                let e = last_error();
                if e == ERROR_NO_MORE_FILES {
                    break;
                }
                return Err(io::Error::from_raw_os_error(e as i32));
            }
            class = FileIdBothDirectoryInfo;
            let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, buf.len() * 8) };
            let mut off = 0usize;
            loop {
                if off + o_name > bytes.len() {
                    break;
                }
                let rec = &bytes[off..];
                let next = rd_u32(rec, o_next) as usize;
                let nlen = rd_u32(rec, o_nlen) as usize;
                let is_dot =
                    matches!(nlen, 2 | 4) && rec.get(o_name..o_name + nlen).is_some_and(|n| n.chunks_exact(2).all(|c| c == [b'.', 0]));
                if !is_dot {
                    let attrs = rd_u32(rec, o_attr);
                    let is_dir = attrs & crate::win::ATTR_DIRECTORY != 0;
                    let name = rec
                        .get(o_name..o_name + nlen)
                        .map(|n| n.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect())
                        .unwrap_or_default();
                    out.push(DirEntry {
                        frn: rd_u64(rec, o_id),
                        attrs,
                        size: if is_dir { 0 } else { rd_u64(rec, o_eof) },
                        mtime: filetime_to_unix(rd_u64(rec, o_write) as i64),
                        name,
                    });
                }
                if next == 0 {
                    break;
                }
                off += next;
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(frn: u64, parent: u64, reason: u32, attrs: u32, name: &str) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let len = (60 + units.len() * 2 + 7) & !7;
        let mut v = vec![0u8; len];
        v[0..4].copy_from_slice(&(len as u32).to_le_bytes());
        v[4..6].copy_from_slice(&2u16.to_le_bytes());
        v[8..16].copy_from_slice(&frn.to_le_bytes());
        v[16..24].copy_from_slice(&parent.to_le_bytes());
        v[40..44].copy_from_slice(&reason.to_le_bytes());
        v[52..56].copy_from_slice(&attrs.to_le_bytes());
        v[56..58].copy_from_slice(&((units.len() * 2) as u16).to_le_bytes());
        v[58..60].copy_from_slice(&60u16.to_le_bytes());
        for (i, u) in units.iter().enumerate() {
            v[60 + i * 2..62 + i * 2].copy_from_slice(&u.to_le_bytes());
        }
        v
    }

    #[test]
    fn parses_usn_records() {
        let mut buf = record(0x1_0000_0000_0040, 0x5_0000_0000_0005, REASON_FILE_DELETE, 0x20, "отчёт.docx");
        buf.extend(record(0x1_0000_0000_0041, 0x5_0000_0000_0005, 0, 0x10, "dir"));
        let recs = parse_records(&buf);
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].name().0, "отчёт.docx");
        assert_eq!(recs[0].reason, REASON_FILE_DELETE);
        assert_eq!(record_index(recs[1].parent), 5);
        // Truncated input stops cleanly.
        assert_eq!(parse_records(&buf[..buf.len() - 3]).len(), 1);
        assert!(is_metafile(0x1_0000_0000_0000));
        assert!(!is_metafile(0x5_0000_0000_0005));
    }

    #[test]
    fn opening_a_volume_needs_admin() {
        // Without elevation the volume handle is refused; with it, the journal is readable.
        match UsnVolume::open('C') {
            Ok(v) => {
                assert!(v.query_journal().is_ok());
            }
            Err(e) => assert_eq!(e.raw_os_error(), Some(5), "{e}"),
        }
    }
}
