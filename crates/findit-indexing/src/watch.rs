//! Live updates for standard mode with `ReadDirectoryChangesW`.
//!
//! One recursive watch per volume root, read with overlapped I/O so the thread can
//! be stopped. The buffer is re-armed immediately after each read; the kernel keeps
//! queuing changes in between. When the kernel buffer overflows the watcher reports
//! [`FsEvent::Overflow`] and the caller rescans.

use crate::win::{last_error, long_path, OwnedHandle};
use std::ffi::c_void;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use windows_sys::Win32::Foundation::{ERROR_NOTIFY_ENUM_DIR, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadDirectoryChangesW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

const NOTIFY_FILTER: u32 = 0x1 | 0x2 | 0x4 | 0x8 | 0x10; // file name, dir name, attributes, size, last write

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsAction {
    Added,
    Removed,
    Modified,
    RenamedOld,
    RenamedNew,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FsEvent {
    /// `path` is relative to the watched root, as exact UTF-16.
    Change { action: FsAction, path: Vec<u16> },
    /// Too many changes at once: the folder must be rescanned.
    Overflow,
    /// The watch stopped (drive removed, access lost). Carries the Win32 error.
    Failed(u32),
}

/// A running watcher. Dropping it stops the thread.
pub struct Watcher {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Watcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watcher").finish_non_exhaustive()
    }
}

impl Watcher {
    /// Watches `root` (for example `C:` or a folder path) recursively and calls
    /// `sink` from a background thread with batches of events.
    pub fn start<F>(root: &[u16], buffer_bytes: usize, sink: F) -> io::Result<Watcher>
    where
        F: Fn(Vec<FsEvent>) + Send + 'static,
    {
        let path = long_path(root);
        let h = unsafe {
            CreateFileW(
                path.as_ptr(),
                FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            )
        };
        let dir = OwnedHandle::new(h).ok_or_else(io::Error::last_os_error)?;
        let event =
            OwnedHandle::new(unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) }).ok_or_else(io::Error::last_os_error)?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let bytes = buffer_bytes.clamp(4096, 4 << 20) & !7;
        let thread = std::thread::Builder::new().name("findit-watch".into()).spawn(move || run(dir, event, bytes, &stop2, sink))?;
        Ok(Watcher { stop, thread: Some(thread) })
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run<F: Fn(Vec<FsEvent>)>(dir: OwnedHandle, event: OwnedHandle, bytes: usize, stop: &AtomicBool, sink: F) {
    // u64 storage keeps the buffer DWORD-aligned as the API requires.
    let mut buf = vec![0u64; bytes / 8];
    loop {
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = event.raw();
        let armed = unsafe {
            ReadDirectoryChangesW(
                dir.raw(),
                buf.as_mut_ptr() as *mut c_void,
                bytes as u32,
                1,
                NOTIFY_FILTER,
                std::ptr::null_mut(),
                &mut ov,
                None,
            )
        };
        if armed == 0 {
            sink(vec![FsEvent::Failed(last_error())]);
            return;
        }
        loop {
            let r = unsafe { WaitForSingleObject(event.raw(), 250) };
            if stop.load(Ordering::Relaxed) {
                let mut n = 0u32;
                unsafe {
                    CancelIoEx(dir.raw(), &ov);
                    GetOverlappedResult(dir.raw(), &ov, &mut n, 1);
                }
                return;
            }
            if r == WAIT_OBJECT_0 {
                break;
            }
            if r != WAIT_TIMEOUT {
                sink(vec![FsEvent::Failed(last_error())]);
                return;
            }
        }
        let mut n = 0u32;
        if unsafe { GetOverlappedResult(dir.raw(), &ov, &mut n, 0) } == 0 {
            let err = last_error();
            if err == ERROR_NOTIFY_ENUM_DIR {
                sink(vec![FsEvent::Overflow]);
                continue;
            }
            sink(vec![FsEvent::Failed(err)]);
            return;
        }
        if n == 0 {
            sink(vec![FsEvent::Overflow]);
            continue;
        }
        let data = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, (n as usize).min(bytes)) };
        let events = parse(data);
        if !events.is_empty() {
            sink(events);
        }
    }
}

/// Decodes a buffer of `FILE_NOTIFY_INFORMATION` records with bounds checks.
pub fn parse(data: &[u8]) -> Vec<FsEvent> {
    let rd = |o: usize| -> Option<u32> { data.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])) };
    let mut out = Vec::new();
    let mut off = 0usize;
    while let (Some(next), Some(action), Some(len)) = (rd(off), rd(off + 4), rd(off + 8)) {
        let start = off + 12;
        let Some(name) = data.get(start..start + len as usize) else { break };
        let path: Vec<u16> = name.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect();
        let action = match action {
            1 => Some(FsAction::Added),
            2 => Some(FsAction::Removed),
            3 => Some(FsAction::Modified),
            4 => Some(FsAction::RenamedOld),
            5 => Some(FsAction::RenamedNew),
            _ => None,
        };
        if let Some(action) = action {
            out.push(FsEvent::Change { action, path });
        }
        if next == 0 {
            break;
        }
        off += next as usize;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(next: u32, action: u32, name: &str) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut v = Vec::new();
        v.extend_from_slice(&next.to_le_bytes());
        v.extend_from_slice(&action.to_le_bytes());
        v.extend_from_slice(&((units.len() * 2) as u32).to_le_bytes());
        for u in units {
            v.extend_from_slice(&u.to_le_bytes());
        }
        while v.len() % 4 != 0 {
            v.push(0);
        }
        v
    }

    #[test]
    fn parses_records_and_ignores_garbage() {
        let mut buf = record(0, 4, "a\\old.txt");
        let len = buf.len() as u32;
        buf[..4].copy_from_slice(&len.to_le_bytes());
        buf.extend(record(0, 5, "a\\new.txt"));
        let ev = parse(&buf);
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[1], FsEvent::Change { action: FsAction::RenamedNew, path: "a\\new.txt".encode_utf16().collect() });
        // Truncated buffers do not panic.
        assert!(parse(&buf[..7]).is_empty());
        let mut bad = record(0, 1, "x");
        bad[8..12].copy_from_slice(&9999u32.to_le_bytes());
        assert!(parse(&bad).is_empty());
    }
}
