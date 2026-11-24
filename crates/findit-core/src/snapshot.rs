//! On-disk snapshot of the index for fast startup.
//!
//! Layout: a 32-byte header (magic, version, payload length, CRC-32 of the payload)
//! followed by the payload. Only live entries are written, so saving also compacts.
//! Saving writes to a temporary file and renames it over the old snapshot, so a
//! crash mid-save never leaves a half-written file behind.

use crate::index::{Entry, Index, Volume, VolumeKind, NONE};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

const MAGIC: &[u8; 8] = b"FINDITIX";
/// Bump whenever the layout changes; older snapshots are then rebuilt.
pub const VERSION: u32 = 1;
const HEADER_LEN: usize = 32;
const ENTRY_LEN: usize = 40;

#[derive(Debug)]
pub enum SnapshotError {
    Missing,
    Io(io::Error),
    BadMagic,
    WrongVersion(u32),
    Checksum,
    Corrupt(&'static str),
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SnapshotError::Missing => write!(f, "no snapshot"),
            SnapshotError::Io(e) => write!(f, "I/O error: {e}"),
            SnapshotError::BadMagic => write!(f, "not a FindIt snapshot"),
            SnapshotError::WrongVersion(v) => write!(f, "snapshot version {v}, expected {VERSION}"),
            SnapshotError::Checksum => write!(f, "checksum mismatch"),
            SnapshotError::Corrupt(what) => write!(f, "corrupt snapshot: {what}"),
        }
    }
}

impl std::error::Error for SnapshotError {}

impl From<io::Error> for SnapshotError {
    fn from(e: io::Error) -> Self {
        if e.kind() == io::ErrorKind::NotFound {
            SnapshotError::Missing
        } else {
            SnapshotError::Io(e)
        }
    }
}

struct CrcWriter<W: Write> {
    inner: W,
    crc: crc32fast::Hasher,
    len: u64,
}

impl<W: Write> Write for CrcWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.crc.update(&buf[..n]);
        self.len += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn kind_byte(k: VolumeKind) -> u8 {
    match k {
        VolumeKind::Standard => 0,
        VolumeKind::Fast => 1,
    }
}

/// Writes the payload (live entries only, renumbered) to `w`.
fn write_payload<W: Write>(ix: &Index, w: &mut W) -> io::Result<()> {
    let n = ix.entries.len();
    let mut remap = vec![NONE; n];
    let mut live = 0u32;
    let (mut names_len, mut folded_len) = (0u64, 0u64);
    for (i, e) in ix.entries.iter().enumerate() {
        if !e.is_deleted() {
            remap[i] = live;
            live += 1;
            names_len += u64::from(e.name_len);
            folded_len += u64::from(e.fold_len);
        }
    }
    w.write_all(&(ix.volumes.len() as u32).to_le_bytes())?;
    for v in &ix.volumes {
        let label = v.label.as_bytes();
        w.write_all(&(label.len() as u16).to_le_bytes())?;
        w.write_all(label)?;
        let root = if v.root == NONE { NONE } else { remap[v.root as usize] };
        w.write_all(&root.to_le_bytes())?;
        w.write_all(&[kind_byte(v.kind)])?;
        w.write_all(&v.serial.to_le_bytes())?;
        w.write_all(&v.journal_id.to_le_bytes())?;
        w.write_all(&v.next_usn.to_le_bytes())?;
        w.write_all(&[u8::from(v.complete), u8::from(v.removed || root == NONE)])?;
    }
    w.write_all(&live.to_le_bytes())?;
    let (mut name_off, mut fold_off) = (0u32, 0u32);
    let mut buf = Vec::with_capacity(ENTRY_LEN * 4096);
    for e in ix.entries.iter().filter(|e| !e.is_deleted()) {
        let parent = if e.parent == NONE { NONE } else { remap[e.parent as usize] };
        buf.extend_from_slice(&name_off.to_le_bytes());
        buf.extend_from_slice(&fold_off.to_le_bytes());
        buf.extend_from_slice(&parent.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&e.name_len.to_le_bytes());
        buf.extend_from_slice(&e.fold_len.to_le_bytes());
        buf.extend_from_slice(&e.flags.to_le_bytes());
        buf.push(e.vol);
        buf.push(0);
        buf.extend_from_slice(&e.size.to_le_bytes());
        buf.extend_from_slice(&e.mtime.to_le_bytes());
        name_off += u32::from(e.name_len);
        fold_off += u32::from(e.fold_len);
        if buf.len() >= ENTRY_LEN * 4096 {
            w.write_all(&buf)?;
            buf.clear();
        }
    }
    w.write_all(&buf)?;
    w.write_all(&names_len.to_le_bytes())?;
    for e in ix.entries.iter().filter(|e| !e.is_deleted()) {
        w.write_all(&ix.names.as_bytes()[e.name_off as usize..e.name_off as usize + e.name_len as usize])?;
    }
    w.write_all(&folded_len.to_le_bytes())?;
    for e in ix.entries.iter().filter(|e| !e.is_deleted()) {
        w.write_all(&ix.folded.as_bytes()[e.fold_off as usize..e.fold_off as usize + e.fold_len as usize])?;
    }
    let raws: Vec<(u32, &Box<[u16]>)> = ix
        .raw_names
        .iter()
        .filter(|(id, _)| remap.get(**id as usize).is_some_and(|&r| r != NONE))
        .map(|(id, r)| (remap[*id as usize], r))
        .collect();
    w.write_all(&(raws.len() as u32).to_le_bytes())?;
    for (id, raw) in raws {
        w.write_all(&id.to_le_bytes())?;
        w.write_all(&(raw.len() as u16).to_le_bytes())?;
        for u in raw.iter() {
            w.write_all(&u.to_le_bytes())?;
        }
    }
    let frns: Vec<(u32, u64)> = ix
        .frns
        .iter()
        .enumerate()
        .filter(|(i, &f)| f != 0 && remap.get(*i).is_some_and(|&r| r != NONE))
        .map(|(i, &f)| (remap[i], f))
        .collect();
    w.write_all(&(frns.len() as u32).to_le_bytes())?;
    for (id, f) in frns {
        w.write_all(&id.to_le_bytes())?;
        w.write_all(&f.to_le_bytes())?;
    }
    Ok(())
}

/// Saves `ix` to `path` atomically. Returns the file size in bytes.
pub fn save(ix: &Index, path: &Path) -> io::Result<u64> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    let result = (|| -> io::Result<u64> {
        let mut file = File::create(&tmp)?;
        file.write_all(&[0u8; HEADER_LEN])?;
        let mut cw = CrcWriter { inner: BufWriter::with_capacity(1 << 20, &mut file), crc: crc32fast::Hasher::new(), len: 0 };
        write_payload(ix, &mut cw)?;
        cw.flush()?;
        let (len, crc) = (cw.len, cw.crc.clone().finalize());
        drop(cw);
        let mut header = [0u8; HEADER_LEN];
        header[..8].copy_from_slice(MAGIC);
        header[8..12].copy_from_slice(&VERSION.to_le_bytes());
        header[16..24].copy_from_slice(&len.to_le_bytes());
        header[24..28].copy_from_slice(&crc.to_le_bytes());
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&header)?;
        file.sync_all()?;
        Ok(len + HEADER_LEN as u64)
    })();
    match result {
        Ok(size) => {
            fs::rename(&tmp, path)?;
            Ok(size)
        }
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], SnapshotError> {
        let end = self.pos.checked_add(n).ok_or(SnapshotError::Corrupt("length overflow"))?;
        let s = self.buf.get(self.pos..end).ok_or(SnapshotError::Corrupt("truncated payload"))?;
        self.pos = end;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, SnapshotError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, SnapshotError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().map_err(|_| SnapshotError::Corrupt("u16"))?))
    }
    fn u32(&mut self) -> Result<u32, SnapshotError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().map_err(|_| SnapshotError::Corrupt("u32"))?))
    }
    fn u64(&mut self) -> Result<u64, SnapshotError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().map_err(|_| SnapshotError::Corrupt("u64"))?))
    }
    fn i64(&mut self) -> Result<i64, SnapshotError> {
        Ok(self.u64()? as i64)
    }
}

/// Loads and fully validates a snapshot.
pub fn load(path: &Path) -> Result<Index, SnapshotError> {
    let mut file = File::open(path)?;
    let mut header = [0u8; HEADER_LEN];
    file.read_exact(&mut header).map_err(|_| SnapshotError::Corrupt("truncated header"))?;
    if &header[..8] != MAGIC {
        return Err(SnapshotError::BadMagic);
    }
    let version = u32::from_le_bytes([header[8], header[9], header[10], header[11]]);
    if version != VERSION {
        return Err(SnapshotError::WrongVersion(version));
    }
    let len = u64::from_le_bytes(header[16..24].try_into().map_err(|_| SnapshotError::Corrupt("header"))?);
    let crc = u32::from_le_bytes(header[24..28].try_into().map_err(|_| SnapshotError::Corrupt("header"))?);
    let file_len = file.metadata()?.len();
    if len != file_len.saturating_sub(HEADER_LEN as u64) {
        return Err(SnapshotError::Corrupt("length does not match file size"));
    }
    let mut payload = Vec::with_capacity(len as usize);
    file.read_to_end(&mut payload)?;
    if crc32fast::hash(&payload) != crc {
        return Err(SnapshotError::Checksum);
    }
    parse_payload(&payload)
}

fn parse_payload(payload: &[u8]) -> Result<Index, SnapshotError> {
    let mut c = Cursor { buf: payload, pos: 0 };
    let nvol = c.u32()? as usize;
    if nvol > 256 {
        return Err(SnapshotError::Corrupt("too many volumes"));
    }
    let mut volumes = Vec::with_capacity(nvol);
    for _ in 0..nvol {
        let ll = c.u16()? as usize;
        let label = std::str::from_utf8(c.take(ll)?).map_err(|_| SnapshotError::Corrupt("volume label"))?.to_string();
        let root = c.u32()?;
        let kind = match c.u8()? {
            0 => VolumeKind::Standard,
            1 => VolumeKind::Fast,
            _ => return Err(SnapshotError::Corrupt("volume kind")),
        };
        let serial = c.u32()?;
        let journal_id = c.u64()?;
        let next_usn = c.i64()?;
        let complete = c.u8()? != 0;
        let removed = c.u8()? != 0;
        volumes.push(Volume { label, root, kind, serial, journal_id, next_usn, complete, removed });
    }
    let n = c.u32()? as usize;
    if n.checked_mul(ENTRY_LEN).is_none_or(|b| b > payload.len()) {
        return Err(SnapshotError::Corrupt("entry count"));
    }
    let raw_entries = c.take(n * ENTRY_LEN)?;
    let mut entries = Vec::with_capacity(n);
    for chunk in raw_entries.chunks_exact(ENTRY_LEN) {
        let u32_at = |o: usize| u32::from_le_bytes([chunk[o], chunk[o + 1], chunk[o + 2], chunk[o + 3]]);
        let u16_at = |o: usize| u16::from_le_bytes([chunk[o], chunk[o + 1]]);
        let u64_at = |o: usize| {
            let mut b = [0u8; 8];
            b.copy_from_slice(&chunk[o..o + 8]);
            u64::from_le_bytes(b)
        };
        entries.push(Entry {
            name_off: u32_at(0),
            fold_off: u32_at(4),
            parent: u32_at(8),
            stamp: 0,
            name_len: u16_at(16),
            fold_len: u16_at(18),
            flags: u16_at(20),
            vol: chunk[22],
            reserved: 0,
            size: u64_at(24),
            mtime: u64_at(32) as i64,
        });
    }
    let names_len = c.u64()? as usize;
    let names = String::from_utf8(c.take(names_len)?.to_vec()).map_err(|_| SnapshotError::Corrupt("names are not UTF-8"))?;
    let folded_len = c.u64()? as usize;
    let folded = String::from_utf8(c.take(folded_len)?.to_vec()).map_err(|_| SnapshotError::Corrupt("folded names are not UTF-8"))?;
    for e in &entries {
        let (no, nl) = (e.name_off as usize, e.name_len as usize);
        let (fo, fl) = (e.fold_off as usize, e.fold_len as usize);
        if no + nl > names.len() || !names.is_char_boundary(no) || !names.is_char_boundary(no + nl) {
            return Err(SnapshotError::Corrupt("name out of range"));
        }
        if fo + fl > folded.len() || !folded.is_char_boundary(fo) || !folded.is_char_boundary(fo + fl) {
            return Err(SnapshotError::Corrupt("folded name out of range"));
        }
        if e.parent != NONE && e.parent as usize >= n {
            return Err(SnapshotError::Corrupt("parent out of range"));
        }
        if e.vol as usize >= nvol.max(1) || e.is_deleted() {
            return Err(SnapshotError::Corrupt("entry flags"));
        }
    }
    for v in &volumes {
        if v.root != NONE && v.root as usize >= n {
            return Err(SnapshotError::Corrupt("volume root out of range"));
        }
    }
    let nraw = c.u32()? as usize;
    let mut raw_names = HashMap::with_capacity(nraw.min(n));
    for _ in 0..nraw {
        let id = c.u32()?;
        let len = c.u16()? as usize;
        let bytes = c.take(len * 2)?;
        let raw: Box<[u16]> = bytes.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect();
        if id as usize >= n {
            return Err(SnapshotError::Corrupt("raw name id"));
        }
        raw_names.insert(id, raw);
    }
    let nfrn = c.u32()? as usize;
    let mut frns = if nfrn > 0 { vec![0u64; n] } else { Vec::new() };
    for _ in 0..nfrn {
        let id = c.u32()? as usize;
        let f = c.u64()?;
        *frns.get_mut(id).ok_or(SnapshotError::Corrupt("frn id"))? = f;
    }
    if c.pos != payload.len() {
        return Err(SnapshotError::Corrupt("trailing bytes"));
    }
    Ok(Index::from_parts(entries, names, folded, volumes, raw_names, frns))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{flags, Meta};

    fn temp_path(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("findit-snap-{tag}-{}-{}.bin", std::process::id(), tag.len()));
        p
    }

    fn sample() -> Index {
        let mut ix = Index::new();
        let slot = ix.add_volume("C:", VolumeKind::Fast, 0xABCD);
        ix.volume_mut(slot).unwrap().journal_id = 77;
        ix.volume_mut(slot).unwrap().next_usn = 12345;
        ix.volume_mut(slot).unwrap().complete = true;
        let root = ix.volumes()[0].root;
        let s = ix.next_stamp();
        let d = ix.upsert(root, "Документы", None, None, Meta { flags: flags::DIR, ..Default::default() }, s);
        let gone = ix.upsert(d, "temp.txt", None, None, Meta::default(), s);
        let f = ix.upsert(d, "report 🎉.pdf", None, None, Meta { size: 42, mtime: 1_700_000_000, flags: flags::HIDDEN }, s);
        ix.set_frn(f, 0x0002_0000_0000_1234);
        let raw: Vec<u16> = vec![0x61, 0xDC00];
        ix.upsert(d, &String::from_utf16_lossy(&raw), None, Some(&raw), Meta::default(), s);
        ix.remove(gone);
        ix
    }

    #[test]
    fn round_trip() {
        let ix = sample();
        let path = temp_path("rt");
        let size = save(&ix, &path).unwrap();
        assert_eq!(size, fs::metadata(&path).unwrap().len());
        let back = load(&path).unwrap();
        fs::remove_file(&path).ok();
        assert_eq!(back.live_count(), ix.live_count());
        assert_eq!(back.deleted_count(), 0);
        assert_eq!(back.volumes(), ix.volumes().iter().map(|v| Volume { root: 0, ..v.clone() }).collect::<Vec<_>>().as_slice());
        let f = back.resolve(0, "документы\\REPORT 🎉.PDF").unwrap();
        assert_eq!(back.path(f), "C:\\Документы\\report 🎉.pdf");
        assert_eq!(back.entry(f).unwrap().size, 42);
        assert_eq!(back.by_frn(0, 0x0002_0000_0000_1234), Some(f));
        let lossy = back.entries().iter().position(|e| e.flags & flags::LOSSY != 0).unwrap() as u32;
        assert_eq!(back.raw_name(lossy), Some(&[0x61u16, 0xDC00][..]));
    }

    #[test]
    fn truncated_file_is_rejected() {
        let ix = sample();
        let path = temp_path("trunc");
        save(&ix, &path).unwrap();
        let bytes = fs::read(&path).unwrap();
        fs::write(&path, &bytes[..bytes.len() - 7]).unwrap();
        let err = load(&path).unwrap_err();
        fs::remove_file(&path).ok();
        assert!(matches!(err, SnapshotError::Corrupt(_)), "{err}");
    }

    #[test]
    fn flipped_bit_fails_checksum() {
        let ix = sample();
        let path = temp_path("crc");
        save(&ix, &path).unwrap();
        let mut bytes = fs::read(&path).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0x10;
        fs::write(&path, &bytes).unwrap();
        let err = load(&path).unwrap_err();
        fs::remove_file(&path).ok();
        assert!(matches!(err, SnapshotError::Checksum), "{err}");
    }

    #[test]
    fn wrong_version_and_magic() {
        let ix = sample();
        let path = temp_path("ver");
        save(&ix, &path).unwrap();
        let mut bytes = fs::read(&path).unwrap();
        bytes[8] = 99;
        fs::write(&path, &bytes).unwrap();
        assert!(matches!(load(&path).unwrap_err(), SnapshotError::WrongVersion(99)));
        bytes[0] = b'X';
        fs::write(&path, &bytes).unwrap();
        assert!(matches!(load(&path).unwrap_err(), SnapshotError::BadMagic));
        fs::remove_file(&path).ok();
        assert!(matches!(load(&path).unwrap_err(), SnapshotError::Missing));
    }
}
