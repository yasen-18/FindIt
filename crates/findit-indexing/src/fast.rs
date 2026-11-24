//! Fast mode, both ends:
//! * [`run_fast`] produces [`HelperMsg`]s from NTFS volumes (runs elevated);
//! * [`FastApplier`] applies them to the index (runs in the app).

use crate::proto::{ChangeItem, HelperMsg, LinkName, MetaItem, UsnItem};
use crate::usn::{is_metafile, JournalError, UsnVolume, REASON_FILE_DELETE, REASON_RENAME_NEW_NAME, REASON_RENAME_OLD_NAME};
use crate::walk::Exclusions;
use crate::win::{flags_from_attrs, name_from_wide, ATTR_DIRECTORY};
use findit_core::fold::{self, FoldMode};
use findit_core::{flags, Meta, SharedIndex, VolumeKind};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const BATCH: usize = 8192;
const POLL: Duration = Duration::from_millis(500);

/// One volume to serve, with the journal position saved in the snapshot (if any).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FastRequest {
    pub letter: char,
    pub resume: Option<(u64, i64)>,
}

impl FastRequest {
    /// Parses the helper command-line form `C` or `C:<journal id>:<usn>`.
    pub fn parse(s: &str) -> Option<FastRequest> {
        let mut parts = s.split(':');
        let letter = parts.next()?.chars().next()?.to_ascii_uppercase();
        if !letter.is_ascii_uppercase() || s.split(':').next()?.len() != 1 {
            return None;
        }
        let resume = match (parts.next(), parts.next()) {
            (Some(j), Some(u)) => Some((j.parse().ok()?, u.parse().ok()?)),
            (None, None) => None,
            _ => return None,
        };
        if parts.next().is_some() {
            return None;
        }
        Some(FastRequest { letter, resume })
    }

    pub fn to_arg(&self) -> String {
        match self.resume {
            Some((j, u)) => format!("{}:{j}:{u}", self.letter),
            None => self.letter.to_string(),
        }
    }
}

fn sleep_unless(stop: &AtomicBool, d: Duration) {
    let step = Duration::from_millis(50);
    let mut waited = Duration::ZERO;
    while waited < d && !stop.load(Ordering::Relaxed) {
        std::thread::sleep(step);
        waited += step;
    }
}

/// Sends a full enumeration of `vol`, then sizes and dates. Returns false when the
/// receiver went away.
fn enumerate_volume(vol: &UsnVolume, high_usn: i64, send: &mut dyn FnMut(HelperMsg) -> bool, stop: &AtomicBool) -> Result<bool, String> {
    let letter = vol.letter;
    let mut dirs: Vec<u64> = vec![vol.root_frn];
    // Folder of the name the enumeration reported for each file. A folder listing
    // that shows the file elsewhere has found another hard link (System32 files
    // are hard links into WinSxS, for example).
    let mut primary_parent: HashMap<u64, u64> = HashMap::new();
    let mut alive = true;
    vol.enumerate(high_usn, |records| {
        let items: Vec<UsnItem> = records
            .into_iter()
            .filter(|r| !is_metafile(r.frn) && r.frn != vol.root_frn)
            .map(|r| {
                if r.attrs & ATTR_DIRECTORY != 0 {
                    dirs.push(r.frn);
                } else {
                    primary_parent.insert(r.frn, r.parent);
                }
                let (name, raw) = r.name();
                UsnItem { frn: r.frn, parent: r.parent, attrs: r.attrs, name, raw }
            })
            .collect();
        alive = send(HelperMsg::Records { vol: letter, items });
        alive && !stop.load(Ordering::Relaxed)
    })
    .map_err(|e| format!("enumeration failed: {e}"))?;
    if !alive || stop.load(Ordering::Relaxed) {
        return Ok(false);
    }
    if !send(HelperMsg::EnumDone { vol: letter }) {
        return Ok(false);
    }
    let mut batch = Vec::with_capacity(BATCH);
    for d in dirs {
        if stop.load(Ordering::Relaxed) {
            return Ok(false);
        }
        let Ok(entries) = vol.list_dir_by_id(d) else { continue };
        for e in entries {
            let link = (e.attrs & ATTR_DIRECTORY == 0 && primary_parent.get(&e.frn).is_some_and(|&p| p != d)).then(|| {
                let (name, raw) = name_from_wide(&e.name);
                LinkName { parent: d, name, raw }
            });
            batch.push(MetaItem { frn: e.frn, size: e.size, mtime: e.mtime, attrs: e.attrs, link });
            if batch.len() >= BATCH && !send(HelperMsg::Meta { vol: letter, items: std::mem::take(&mut batch) }) {
                return Ok(false);
            }
        }
    }
    if !batch.is_empty() && !send(HelperMsg::Meta { vol: letter, items: batch }) {
        return Ok(false);
    }
    Ok(send(HelperMsg::MetaDone { vol: letter }))
}

/// Collapses raw journal records to the latest state per file and adds sizes and
/// dates by listing the affected folders.
fn collapse(vol: &UsnVolume, records: Vec<crate::usn::RawRecord>) -> Vec<ChangeItem> {
    let mut order: Vec<u64> = Vec::new();
    let mut latest: HashMap<u64, crate::usn::RawRecord> = HashMap::new();
    for r in records {
        if is_metafile(r.frn) || r.frn == vol.root_frn {
            continue;
        }
        if r.reason & REASON_RENAME_OLD_NAME != 0 && r.reason & (REASON_RENAME_NEW_NAME | REASON_FILE_DELETE) == 0 {
            continue;
        }
        if !latest.contains_key(&r.frn) {
            order.push(r.frn);
        }
        latest.insert(r.frn, r);
    }
    let mut items: Vec<ChangeItem> = order
        .into_iter()
        .filter_map(|f| latest.remove(&f))
        .map(|r| {
            let (name, raw) = r.name();
            ChangeItem {
                frn: r.frn,
                parent: r.parent,
                attrs: r.attrs,
                name,
                raw,
                deleted: r.reason & REASON_FILE_DELETE != 0,
                size: None,
                mtime: None,
            }
        })
        .collect();
    let mut parents: Vec<u64> = items.iter().filter(|i| !i.deleted).map(|i| i.parent).collect();
    parents.sort_unstable();
    parents.dedup();
    let mut meta: HashMap<u64, (u64, i64)> = HashMap::new();
    for p in parents.into_iter().take(20_000) {
        if let Ok(entries) = vol.list_dir_by_id(p) {
            for e in entries {
                meta.insert(e.frn, (e.size, e.mtime));
            }
        }
    }
    for it in &mut items {
        if let Some(&(s, m)) = meta.get(&it.frn) {
            it.size = Some(s);
            it.mtime = Some(m);
        }
    }
    items
}

/// The fast-mode producer. Opens all volumes, calls `after_open` (the helper drops
/// privileges there), enumerates or resumes each volume, then polls the journals
/// until `stop` is set or `send` returns false.
pub fn run_fast(reqs: &[FastRequest], send: &mut dyn FnMut(HelperMsg) -> bool, stop: &AtomicBool, after_open: &mut dyn FnMut()) {
    let mut opened = Vec::new();
    for r in reqs {
        match UsnVolume::open(r.letter) {
            Ok(v) => opened.push((v, r.resume)),
            Err(e) => {
                if !send(HelperMsg::VolumeError { vol: r.letter, message: format!("cannot open volume: {e}") }) {
                    return;
                }
            }
        }
    }
    after_open();
    // (volume, journal id, cursor)
    let mut live: Vec<(UsnVolume, u64, i64)> = Vec::new();
    for (vol, resume) in opened {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        match start_volume(&vol, resume, send, stop) {
            Ok(Some((jid, cursor))) => live.push((vol, jid, cursor)),
            Ok(None) => return,
            Err(message) => {
                if !send(HelperMsg::VolumeError { vol: vol.letter, message }) {
                    return;
                }
            }
        }
    }
    while !stop.load(Ordering::Relaxed) && !live.is_empty() {
        sleep_unless(stop, POLL);
        let mut i = 0;
        while i < live.len() {
            let (vol, jid, cursor) = (&live[i].0, live[i].1, live[i].2);
            match vol.read_changes(jid, cursor, 200_000) {
                Ok((records, next)) => {
                    if !records.is_empty() {
                        let items = collapse(vol, records);
                        if !send(HelperMsg::Changes { vol: vol.letter, items, next_usn: next }) {
                            return;
                        }
                    }
                    live[i].2 = next;
                    i += 1;
                }
                Err(JournalError::Wrapped) => {
                    // We fell too far behind: enumerate again (the app drops the old data).
                    match start_volume(vol, None, send, stop) {
                        Ok(Some((jid, cursor))) => {
                            live[i].1 = jid;
                            live[i].2 = cursor;
                            i += 1;
                        }
                        Ok(None) => return,
                        Err(message) => {
                            let letter = vol.letter;
                            live.remove(i);
                            if !send(HelperMsg::VolumeError { vol: letter, message }) {
                                return;
                            }
                        }
                    }
                }
                Err(e) => {
                    let letter = vol.letter;
                    live.remove(i);
                    let message = match e {
                        JournalError::Gone => "the change journal was deleted or disabled".to_string(),
                        JournalError::Io(e) => format!("journal read failed: {e}"),
                        JournalError::Wrapped => "journal wrapped".to_string(),
                    };
                    if !send(HelperMsg::VolumeError { vol: letter, message }) {
                        return;
                    }
                }
            }
        }
    }
}

/// Announces a volume and enumerates it unless the saved position is still valid.
/// `Ok(None)` means the receiver is gone.
fn start_volume(
    vol: &UsnVolume,
    resume: Option<(u64, i64)>,
    send: &mut dyn FnMut(HelperMsg) -> bool,
    stop: &AtomicBool,
) -> Result<Option<(u64, i64)>, String> {
    let j = vol.query_journal().map_err(|e| format!("the NTFS change journal is not available: {e}"))?;
    let resumed = resume.filter(|&(jid, usn)| jid == j.id && usn >= j.first_usn && usn <= j.next_usn);
    let cursor = resumed.map_or(j.next_usn, |(_, usn)| usn);
    if !send(HelperMsg::Journal { vol: vol.letter, journal_id: j.id, next_usn: cursor, root_frn: vol.root_frn, resumed: resumed.is_some() })
    {
        return Ok(None);
    }
    if resumed.is_none() && !enumerate_volume(vol, j.next_usn, send, stop)? {
        return Ok(None);
    }
    Ok(Some((j.id, cursor)))
}

// ----- app side ----------------------------------------------------------------------

/// What happened after applying a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FastEvent {
    Nothing,
    /// Records arrived (count so far for this volume).
    Progress {
        vol: char,
        count: usize,
    },
    /// Names are complete; sizes and dates are still arriving.
    NamesReady {
        vol: char,
    },
    /// The volume is fully indexed.
    Ready {
        vol: char,
    },
    Changed,
    /// This volume must fall back to standard mode.
    Failed {
        vol: char,
        message: String,
    },
    Fatal(String),
}

#[derive(Default)]
struct VolState {
    slot: u8,
    root_frn: u64,
    journal_id: u64,
    next_usn: i64,
    pending: Vec<(u32, u64)>,
    count: usize,
}

/// Applies helper messages to the index.
pub struct FastApplier {
    index: SharedIndex,
    excl: Exclusions,
    vols: HashMap<char, VolState>,
}

impl std::fmt::Debug for FastApplier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FastApplier").field("volumes", &self.vols.keys().collect::<Vec<_>>()).finish()
    }
}

fn meta_of(attrs: u32, size: Option<u64>, mtime: Option<i64>) -> Meta {
    let mut fl = flags_from_attrs(attrs, 0);
    if size.is_none() {
        fl |= flags::NO_META;
    }
    Meta { size: size.unwrap_or(0), mtime: mtime.unwrap_or(0), flags: fl }
}

impl FastApplier {
    pub fn new(index: SharedIndex, excl: Exclusions) -> FastApplier {
        FastApplier { index, excl, vols: HashMap::new() }
    }

    fn label(vol: char) -> String {
        format!("{}:", vol.to_ascii_uppercase())
    }

    pub fn apply(&mut self, msg: HelperMsg) -> FastEvent {
        match msg {
            HelperMsg::Hello { version } => {
                if version != crate::proto::PROTOCOL_VERSION {
                    return FastEvent::Fatal(format!("helper protocol {version} does not match"));
                }
                FastEvent::Nothing
            }
            HelperMsg::Fatal { message } => FastEvent::Fatal(message),
            HelperMsg::VolumeError { vol, message } => {
                self.vols.remove(&vol);
                FastEvent::Failed { vol, message }
            }
            HelperMsg::Journal { vol, journal_id, next_usn, root_frn, resumed } => {
                let mut ix = self.index.write();
                let label = Self::label(vol);
                let slot = match ix.volume_slot(&label) {
                    Some(s) if !ix.volumes()[s as usize].removed => s,
                    _ => ix.add_volume(&label, VolumeKind::Fast, 0),
                };
                if !resumed {
                    ix.clear_volume(slot);
                }
                let root = ix.volumes()[slot as usize].root;
                ix.set_frn(root, root_frn);
                if let Some(v) = ix.volume_mut(slot) {
                    v.kind = VolumeKind::Fast;
                    if resumed {
                        v.journal_id = journal_id;
                        v.next_usn = next_usn;
                    }
                }
                self.vols.insert(vol, VolState { slot, root_frn, journal_id, next_usn, pending: Vec::new(), count: 0 });
                if resumed {
                    FastEvent::Ready { vol }
                } else {
                    FastEvent::Progress { vol, count: 0 }
                }
            }
            HelperMsg::Records { vol, items } => {
                let Some(st) = self.vols.get_mut(&vol) else { return FastEvent::Nothing };
                let prepared: Vec<(UsnItem, String)> = items
                    .into_iter()
                    .filter(|i| !i.name.is_empty() && i.name.len() <= 2048)
                    .map(|i| {
                        let f = fold::fold(&i.name, FoldMode::LOOSE);
                        (i, f)
                    })
                    .collect();
                let mut ix = self.index.write();
                let stamp = ix.next_stamp();
                for (it, folded) in &prepared {
                    let meta = meta_of(it.attrs, None, None);
                    let id = ix.push_pending(st.slot, &it.name, Some(folded), it.raw.as_deref(), meta, stamp);
                    ix.set_frn(id, it.frn);
                    st.pending.push((id, it.parent));
                }
                st.count += prepared.len();
                FastEvent::Progress { vol, count: st.count }
            }
            HelperMsg::EnumDone { vol } => {
                let Some(st) = self.vols.get_mut(&vol) else { return FastEvent::Nothing };
                let mut ix = self.index.write();
                let root = ix.volumes()[st.slot as usize].root;
                for &(id, parent_frn) in &st.pending {
                    let parent = if parent_frn == st.root_frn { Some(root) } else { ix.by_frn(st.slot, parent_frn) };
                    match parent {
                        Some(p) if p != id => ix.attach(id, p),
                        _ => ix.remove(id),
                    }
                }
                // Entries that never got a parent (children of NTFS metadata folders).
                for &(id, _) in &st.pending {
                    if ix.entry(id).is_some_and(|e| e.flags & flags::PENDING != 0) {
                        ix.remove(id);
                    }
                }
                for &(id, _) in &st.pending {
                    let excluded = ix.entry(id).is_some_and(|e| !e.is_deleted()) && self.excl.excludes_name(ix.name(id));
                    if excluded {
                        ix.remove(id);
                    }
                }
                if self.excl.has_prefixes() {
                    for &(id, _) in &st.pending {
                        if ix.entry(id).is_some_and(|e| !e.is_deleted() && e.is_dir()) && self.excl.excludes_path(&ix.path(id)) {
                            ix.remove(id);
                        }
                    }
                }
                ix.sweep_orphans();
                st.pending = Vec::new();
                if let Some(v) = ix.volume_mut(st.slot) {
                    v.journal_id = st.journal_id;
                    v.next_usn = st.next_usn;
                    v.complete = true;
                }
                FastEvent::NamesReady { vol }
            }
            HelperMsg::Meta { vol, items } => {
                let Some(st) = self.vols.get(&vol) else { return FastEvent::Nothing };
                let mut ix = self.index.write();
                let root = ix.volumes()[st.slot as usize].root;
                let stamp = ix.next_stamp();
                for m in items {
                    let meta = meta_of(m.attrs, Some(m.size), Some(m.mtime));
                    match m.link {
                        None => {
                            if let Some(id) = ix.by_frn(st.slot, m.frn) {
                                ix.update_meta(id, meta);
                            }
                        }
                        Some(link) => {
                            // Another name of an indexed file: list it in its own folder too.
                            let parent = if link.parent == st.root_frn { Some(root) } else { ix.by_frn(st.slot, link.parent) };
                            let Some(parent) = parent.filter(|&p| ix.entry(p).is_some_and(|e| !e.is_deleted())) else { continue };
                            if link.name.is_empty() || self.excl.excludes_name(&link.name) {
                                continue;
                            }
                            let id = ix.upsert(parent, &link.name, None, link.raw.as_deref(), meta, stamp);
                            if ix.by_frn(st.slot, m.frn) != Some(id) {
                                ix.set_frn_alias(id, m.frn);
                            }
                        }
                    }
                }
                FastEvent::Changed
            }
            HelperMsg::MetaDone { vol } => FastEvent::Ready { vol },
            HelperMsg::Changes { vol, items, next_usn } => {
                let Some(st) = self.vols.get_mut(&vol) else { return FastEvent::Nothing };
                let mut ix = self.index.write();
                let root = ix.volumes()[st.slot as usize].root;
                let stamp = ix.next_stamp();
                for c in items {
                    let existing = ix.by_frn(st.slot, c.frn);
                    if c.deleted {
                        if let Some(id) = existing {
                            ix.remove(id);
                        }
                        for alias in ix.aliases_of(st.slot, c.frn) {
                            ix.remove(alias);
                        }
                        continue;
                    }
                    let parent = if c.parent == st.root_frn { Some(root) } else { ix.by_frn(st.slot, c.parent) };
                    let Some(parent) = parent else { continue };
                    if ix.entry(parent).is_none_or(|e| e.is_deleted()) || self.excl.excludes_name(&c.name) {
                        if let Some(id) = existing {
                            ix.remove(id);
                        }
                        continue;
                    }
                    let meta = match (c.size, c.mtime) {
                        (Some(_), Some(_)) => Some(meta_of(c.attrs, c.size, c.mtime)),
                        _ => None,
                    };
                    match existing {
                        Some(id) => {
                            let e = *ix.entry(id).unwrap_or(&findit_core::Entry::default());
                            if (e.parent != parent || ix.name(id) != c.name) && !ix.rename(id, parent, &c.name, c.raw.as_deref()) {
                                ix.remove(id);
                                continue;
                            }
                            match meta {
                                Some(m) => ix.update_meta(id, m),
                                None => {
                                    let keep = Meta {
                                        size: e.size,
                                        mtime: e.mtime,
                                        flags: flags_from_attrs(c.attrs, 0) | (e.flags & flags::NO_META),
                                    };
                                    ix.update_meta(id, keep);
                                }
                            }
                            ix.touch(id, stamp);
                        }
                        None => {
                            let m = meta.unwrap_or_else(|| meta_of(c.attrs, None, None));
                            let id = ix.upsert(parent, &c.name, None, c.raw.as_deref(), m, stamp);
                            ix.set_frn(id, c.frn);
                        }
                    }
                }
                ix.sweep_orphans();
                st.next_usn = next_usn;
                if let Some(v) = ix.volume_mut(st.slot) {
                    v.next_usn = next_usn;
                }
                FastEvent::Changed
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use findit_core::{shared, Index};

    const ROOT: u64 = 0x0005_0000_0000_0005;

    fn item(frn: u64, parent: u64, dir: bool, name: &str) -> UsnItem {
        UsnItem { frn, parent, attrs: if dir { ATTR_DIRECTORY } else { 0x20 }, name: name.into(), raw: None }
    }

    #[test]
    fn request_args() {
        assert_eq!(FastRequest::parse("c"), Some(FastRequest { letter: 'C', resume: None }));
        assert_eq!(FastRequest::parse("D:12:-5"), Some(FastRequest { letter: 'D', resume: Some((12, -5)) }));
        assert_eq!(FastRequest::parse("CD"), None);
        assert_eq!(FastRequest::parse("C:1"), None);
        assert_eq!(FastRequest::parse("C:x:1"), None);
        assert_eq!(FastRequest::parse("C:1:2:3"), None);
        let r = FastRequest { letter: 'E', resume: Some((7, 9)) };
        assert_eq!(FastRequest::parse(&r.to_arg()), Some(r));
    }

    #[test]
    fn applies_enumeration_meta_and_changes() {
        let index = shared(Index::new());
        let mut fa = FastApplier::new(index.clone(), Exclusions::new(&["$Recycle.Bin".into()]));
        assert_eq!(
            fa.apply(HelperMsg::Journal { vol: 'X', journal_id: 3, next_usn: 1000, root_frn: ROOT, resumed: false }),
            FastEvent::Progress { vol: 'X', count: 0 }
        );
        // Children arrive before their parents, as they do in MFT order.
        fa.apply(HelperMsg::Records {
            vol: 'X',
            items: vec![
                item(0x40, 0x41, false, "report.pdf"),
                item(0x41, ROOT, true, "Docs"),
                item(0x42, 0x43, false, "trash.txt"),
                item(0x43, ROOT, true, "$Recycle.Bin"),
                item(0x44, 0x99, false, "orphan.txt"),
            ],
        });
        {
            let ix = index.read();
            assert!(ix.entries().iter().all(|e| e.is_root() || e.flags & flags::PENDING != 0));
        }
        assert_eq!(fa.apply(HelperMsg::EnumDone { vol: 'X' }), FastEvent::NamesReady { vol: 'X' });
        {
            let ix = index.read();
            let f = ix.resolve(0, "Docs\\report.pdf").unwrap();
            assert_eq!(ix.path(f), "X:\\Docs\\report.pdf");
            assert!(ix.entry(f).unwrap().flags & flags::NO_META != 0);
            assert!(ix.resolve(0, "$Recycle.Bin").is_none());
            assert_eq!(ix.live_count(), 3);
            assert_eq!(ix.volumes()[0].journal_id, 3);
            assert!(ix.volumes()[0].complete);
        }
        fa.apply(HelperMsg::Meta {
            vol: 'X',
            items: vec![
                MetaItem { frn: 0x40, size: 99, mtime: 1_700_000_000, attrs: 0x20, link: None },
                // The same file is also hard-linked at the volume root.
                MetaItem {
                    frn: 0x40,
                    size: 99,
                    mtime: 1_700_000_000,
                    attrs: 0x20,
                    link: Some(LinkName { parent: ROOT, name: "report-link.pdf".into(), raw: None }),
                },
            ],
        });
        assert!(index.read().resolve(0, "report-link.pdf").is_some());
        {
            let ix = index.read();
            let f = ix.resolve(0, "Docs\\report.pdf").unwrap();
            let e = ix.entry(f).unwrap();
            assert_eq!(e.size, 99);
            assert_eq!(e.flags & flags::NO_META, 0);
        }
        // Rename + move, create, delete folder.
        let ch = |frn, parent, name: &str, dir: bool, deleted: bool| ChangeItem {
            frn,
            parent,
            attrs: if dir { ATTR_DIRECTORY } else { 0x20 },
            name: name.into(),
            raw: None,
            deleted,
            size: Some(5),
            mtime: Some(1_800_000_000),
        };
        fa.apply(HelperMsg::Changes {
            vol: 'X',
            items: vec![
                ch(0x50, ROOT, "Archive", true, false),
                ch(0x40, 0x50, "report-final.pdf", false, false),
                ch(0x51, 0x41, "new.txt", false, false),
            ],
            next_usn: 2000,
        });
        {
            let ix = index.read();
            let f = ix.by_frn(0, 0x40).unwrap();
            assert_eq!(ix.path(f), "X:\\Archive\\report-final.pdf");
            assert!(ix.resolve(0, "Docs\\new.txt").is_some());
            assert_eq!(ix.volumes()[0].next_usn, 2000);
        }
        fa.apply(HelperMsg::Changes { vol: 'X', items: vec![ch(0x41, ROOT, "Docs", true, true)], next_usn: 2100 });
        {
            let ix = index.read();
            assert!(ix.resolve(0, "Docs").is_none());
            assert!(ix.by_frn(0, 0x51).is_none(), "children of a deleted folder are gone");
            assert!(ix.by_frn(0, 0x40).is_some());
        }
        // Deleting the file removes every hard-link name.
        fa.apply(HelperMsg::Changes { vol: 'X', items: vec![ch(0x40, 0x50, "report-final.pdf", false, true)], next_usn: 2050 });
        {
            let ix = index.read();
            assert!(ix.by_frn(0, 0x40).is_none());
            assert!(ix.resolve(0, "report-link.pdf").is_none());
        }
        // A resumed session keeps everything.
        assert_eq!(
            fa.apply(HelperMsg::Journal { vol: 'X', journal_id: 3, next_usn: 2100, root_frn: ROOT, resumed: true }),
            FastEvent::Ready { vol: 'X' }
        );
        assert!(index.read().by_frn(0, 0x50).is_some());
        assert!(matches!(fa.apply(HelperMsg::VolumeError { vol: 'X', message: "x".into() }), FastEvent::Failed { .. }));
    }
}
