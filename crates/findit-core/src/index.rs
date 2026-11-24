//! Compact in-memory file index.
//!
//! Layout (see section 3 of the spec):
//! * every name is appended to one contiguous UTF-8 pool (`names`) and its folded
//!   form to a second pool (`folded`), so searching is a linear scan over bytes;
//! * entries are fixed-size records `{name, folded name, parent, size, mtime, flags}`;
//! * full paths are never stored: they are built on demand by walking parent ids.
//!
//! Ids are indexes into the entry array. Deleted entries are tombstoned and removed
//! by [`Index::compact`], which bumps [`Index::epoch`] so callers know old ids are void.

use crate::fold::{self, FoldMode};
use hashbrown::HashTable;
use parking_lot::{Mutex, RwLock};
use rayon::prelude::*;
use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher, RandomState};
use std::sync::Arc;

/// An index shared between scanner threads, watchers and the search engine.
pub type SharedIndex = Arc<RwLock<Index>>;

/// "No entry" marker (used as the parent of volume roots).
pub const NONE: u32 = u32::MAX;

/// Upper bound for parent walks; protects against cycles in corrupt data.
const MAX_DEPTH: usize = 4096;

/// Longest name accepted (NTFS allows 255 UTF-16 units, at most 765 UTF-8 bytes).
const MAX_NAME_BYTES: usize = 4096;

/// Entry flag bits.
pub mod flags {
    pub const DIR: u16 = 1 << 0;
    pub const HIDDEN: u16 = 1 << 1;
    pub const SYSTEM: u16 = 1 << 2;
    pub const DELETED: u16 = 1 << 3;
    /// Junction or symbolic link. Indexed but never followed.
    pub const LINK: u16 = 1 << 4;
    /// Cloud placeholder (OneDrive etc.). Never opened, so never downloaded.
    pub const CLOUD: u16 = 1 << 5;
    pub const ROOT: u16 = 1 << 6;
    /// The real name contains invalid UTF-16; the exact name is kept in a side table.
    pub const LOSSY: u16 = 1 << 7;
    pub const READONLY: u16 = 1 << 8;
    /// Size and date are not known yet (fast mode fills them in after enumeration).
    pub const NO_META: u16 = 1 << 9;
    /// Added during a bulk load and not searchable until the load finishes.
    pub const PENDING: u16 = 1 << 10;

    /// Flags that describe the file itself and can be changed by an update.
    pub const ATTRS: u16 = DIR | HIDDEN | SYSTEM | LINK | CLOUD | READONLY | NO_META;
}

/// One file or folder. 40 bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct Entry {
    pub name_off: u32,
    pub fold_off: u32,
    pub parent: u32,
    /// Scan stamp: the newest scan (or update batch) that saw this entry.
    pub stamp: u32,
    pub name_len: u16,
    pub fold_len: u16,
    pub flags: u16,
    /// Volume slot (index into [`Index::volumes`]).
    pub vol: u8,
    pub reserved: u8,
    pub size: u64,
    /// Last write time, seconds since 1970-01-01 UTC.
    pub mtime: i64,
}

impl Entry {
    #[inline]
    pub fn is_dir(&self) -> bool {
        self.flags & flags::DIR != 0
    }
    #[inline]
    pub fn is_deleted(&self) -> bool {
        self.flags & flags::DELETED != 0
    }
    #[inline]
    pub fn is_root(&self) -> bool {
        self.flags & flags::ROOT != 0
    }
    /// Live, not a root and not pending: may appear in search results.
    #[inline]
    pub fn is_searchable(&self) -> bool {
        self.flags & (flags::DELETED | flags::ROOT | flags::PENDING) == 0
    }
}

/// File metadata supplied by scanners.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Meta {
    pub size: u64,
    pub mtime: i64,
    /// Only bits from [`flags::ATTRS`] are used.
    pub flags: u16,
}

/// How a volume is kept up to date.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum VolumeKind {
    /// Folder walk plus `ReadDirectoryChangesW`.
    Standard,
    /// NTFS master file table enumeration plus the USN change journal.
    Fast,
}

/// A drive (or, in tests, any folder) whose contents are indexed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Volume {
    /// Root label, for example `C:`.
    pub label: String,
    pub root: u32,
    pub kind: VolumeKind,
    pub serial: u32,
    pub journal_id: u64,
    pub next_usn: i64,
    /// A full scan finished at least once.
    pub complete: bool,
    pub removed: bool,
}

/// Folded full paths of all folders, used for `path:` matching and path sorting.
#[derive(Debug, Default)]
pub struct DirPaths {
    off: Vec<u32>,
    len: Vec<u32>,
    pool: Vec<u8>,
}

impl DirPaths {
    /// Folded full path of folder `id` (or `None` for files and deleted entries).
    #[inline]
    pub fn get(&self, id: u32) -> Option<&[u8]> {
        let i = id as usize;
        let off = *self.off.get(i)?;
        if off == NONE {
            return None;
        }
        Some(&self.pool[off as usize..off as usize + self.len[i] as usize])
    }
}

#[derive(Default)]
struct Caches {
    path_lens: Option<(u64, Arc<Vec<u32>>)>,
    dir_paths: Option<(u64, Arc<DirPaths>)>,
    name_order: Option<(u64, Arc<Vec<u32>>)>,
}

/// The file index.
pub struct Index {
    pub(crate) entries: Vec<Entry>,
    pub(crate) names: String,
    pub(crate) folded: String,
    lookup: HashTable<u32>,
    hasher: RandomState,
    pub(crate) raw_names: HashMap<u32, Box<[u16]>>,
    /// Per volume slot: NTFS file reference number -> id (fast mode only).
    frn_maps: Vec<hashbrown::HashMap<u64, u32>>,
    /// Per volume slot: file reference number -> extra hard-link names of that file.
    frn_aliases: Vec<hashbrown::HashMap<u64, Vec<u32>>>,
    /// id -> file reference number (0 = none), grown lazily.
    pub(crate) frns: Vec<u64>,
    pub(crate) volumes: Vec<Volume>,
    generation: u64,
    structure_gen: u64,
    epoch: u64,
    live: usize,
    deleted: usize,
    garbage_bytes: usize,
    stamp: u32,
    needs_orphan_sweep: bool,
    caches: Mutex<Caches>,
}

impl Default for Index {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Index {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Index").field("entries", &self.entries.len()).field("live", &self.live).field("volumes", &self.volumes).finish()
    }
}

#[inline]
fn key_hash(hasher: &RandomState, parent: u32, name: &str) -> u64 {
    let mut h = hasher.build_hasher();
    h.write_u32(parent);
    for c in name.chars() {
        h.write_u32(fold::simple_upper(c) as u32);
    }
    h.finish()
}

#[inline]
fn name_of<'a>(entries: &[Entry], names: &'a str, id: u32) -> &'a str {
    let e = &entries[id as usize];
    &names[e.name_off as usize..e.name_off as usize + e.name_len as usize]
}

fn truncate_name(name: &str) -> &str {
    if name.len() <= MAX_NAME_BYTES {
        return name;
    }
    let mut end = MAX_NAME_BYTES;
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    &name[..end]
}

impl Index {
    pub fn new() -> Self {
        Index {
            entries: Vec::new(),
            names: String::new(),
            folded: String::new(),
            lookup: HashTable::new(),
            hasher: RandomState::new(),
            raw_names: HashMap::new(),
            frn_maps: Vec::new(),
            frn_aliases: Vec::new(),
            frns: Vec::new(),
            volumes: Vec::new(),
            generation: 1,
            structure_gen: 1,
            epoch: 1,
            live: 0,
            deleted: 0,
            garbage_bytes: 0,
            stamp: 1,
            needs_orphan_sweep: false,
            caches: Mutex::new(Caches::default()),
        }
    }

    // ----- read access ---------------------------------------------------------

    /// Number of slots, including deleted entries. Valid ids are `0..len()`.
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.live == 0
    }
    /// Number of live entries, including volume roots.
    pub fn live_count(&self) -> usize {
        self.live
    }
    pub fn deleted_count(&self) -> usize {
        self.deleted
    }
    /// Changes on every mutation.
    pub fn generation(&self) -> u64 {
        self.generation
    }
    /// Changes when names or the tree shape change (not for size or date updates).
    pub fn structure_generation(&self) -> u64 {
        self.structure_gen
    }
    /// Changes when ids are renumbered by [`Index::compact`].
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
    /// Moves the epoch forward, so ids handed out by a replaced index are never
    /// mistaken for ids of this one.
    pub fn advance_epoch_past(&mut self, other: u64) {
        self.epoch = self.epoch.max(other) + 1;
    }
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }
    pub fn entry(&self, id: u32) -> Option<&Entry> {
        self.entries.get(id as usize)
    }
    pub fn volumes(&self) -> &[Volume] {
        &self.volumes
    }
    pub fn volume(&self, slot: u8) -> Option<&Volume> {
        self.volumes.get(slot as usize)
    }
    pub fn volume_mut(&mut self, slot: u8) -> Option<&mut Volume> {
        self.volumes.get_mut(slot as usize)
    }
    pub fn volume_slot(&self, label: &str) -> Option<u8> {
        self.volumes.iter().position(|v| fold::eq_ignore_case(&v.label, label)).map(|i| i as u8)
    }

    /// Display name of an entry.
    #[inline]
    pub fn name(&self, id: u32) -> &str {
        name_of(&self.entries, &self.names, id)
    }

    /// Folded (case- and accent-insensitive) name of an entry.
    #[inline]
    pub fn folded_name(&self, id: u32) -> &str {
        let e = &self.entries[id as usize];
        &self.folded[e.fold_off as usize..e.fold_off as usize + e.fold_len as usize]
    }

    #[inline]
    pub fn folded_bytes(&self, id: u32) -> &[u8] {
        self.folded_name(id).as_bytes()
    }

    /// Exact UTF-16 name for entries whose name is not valid Unicode.
    pub fn raw_name(&self, id: u32) -> Option<&[u16]> {
        self.raw_names.get(&id).map(|b| &b[..])
    }

    /// Ids from the volume root down to `id` (inclusive).
    pub fn ancestry(&self, id: u32) -> Vec<u32> {
        let mut chain = Vec::with_capacity(16);
        let mut cur = id;
        while cur != NONE && (cur as usize) < self.entries.len() && chain.len() < MAX_DEPTH {
            chain.push(cur);
            cur = self.entries[cur as usize].parent;
        }
        chain.reverse();
        chain
    }

    /// Full display path, for example `C:\Users\me\file.txt`.
    pub fn path(&self, id: u32) -> String {
        let chain = self.ancestry(id);
        let mut out = String::with_capacity(chain.len() * 16);
        for (i, &c) in chain.iter().enumerate() {
            if i > 0 {
                out.push('\\');
            }
            out.push_str(self.name(c));
        }
        if chain.len() == 1 && out.ends_with(':') {
            out.push('\\');
        }
        out
    }

    /// Display path of the folder that contains `id`.
    pub fn parent_path(&self, id: u32) -> String {
        match self.entries.get(id as usize) {
            Some(e) if e.parent != NONE => self.path(e.parent),
            _ => String::new(),
        }
    }

    /// Full path as UTF-16 (exact even for names with invalid UTF-16).
    pub fn wide_path(&self, id: u32) -> Vec<u16> {
        let chain = self.ancestry(id);
        let mut out: Vec<u16> = Vec::with_capacity(chain.len() * 16);
        for (i, &c) in chain.iter().enumerate() {
            if i > 0 {
                out.push(u16::from(b'\\'));
            }
            match self.raw_names.get(&c) {
                Some(raw) => out.extend_from_slice(raw),
                None => out.extend(self.name(c).encode_utf16()),
            }
        }
        if chain.len() == 1 && out.last() == Some(&u16::from(b':')) {
            out.push(u16::from(b'\\'));
        }
        out
    }

    /// True if `ancestor` is `id` or one of its parents.
    pub fn is_within(&self, id: u32, ancestor: u32) -> bool {
        let mut cur = id;
        let mut depth = 0;
        while cur != NONE && (cur as usize) < self.entries.len() && depth < MAX_DEPTH {
            if cur == ancestor {
                return true;
            }
            cur = self.entries[cur as usize].parent;
            depth += 1;
        }
        false
    }

    /// Finds a live child by name: exact match first, then case-insensitive.
    pub fn find_child(&self, parent: u32, name: &str) -> Option<u32> {
        let h = key_hash(&self.hasher, parent, name);
        let (entries, names) = (&self.entries, &self.names);
        self.lookup
            .find(h, |&id| entries[id as usize].parent == parent && name_of(entries, names, id) == name)
            .or_else(|| {
                self.lookup.find(h, |&id| entries[id as usize].parent == parent && fold::eq_ignore_case(name_of(entries, names, id), name))
            })
            .copied()
    }

    /// Resolves a path relative to a volume root (components separated by `\`).
    pub fn resolve(&self, slot: u8, rel: &str) -> Option<u32> {
        let vol = self.volumes.get(slot as usize)?;
        if vol.removed {
            return None;
        }
        let mut cur = vol.root;
        for comp in rel.split('\\').filter(|c| !c.is_empty()) {
            cur = self.find_child(cur, comp)?;
        }
        Some(cur)
    }

    /// Looks up an entry by NTFS file reference number.
    pub fn by_frn(&self, slot: u8, frn: u64) -> Option<u32> {
        self.frn_maps.get(slot as usize)?.get(&frn).copied()
    }

    /// The file reference number recorded for `id` (0 when none).
    pub fn frn_of(&self, id: u32) -> u64 {
        self.frns.get(id as usize).copied().unwrap_or(0)
    }

    // ----- volumes -------------------------------------------------------------

    /// Adds a volume (or revives a removed one with the same label) and returns its slot.
    pub fn add_volume(&mut self, label: &str, kind: VolumeKind, serial: u32) -> u8 {
        if let Some(slot) = self.volume_slot(label) {
            let needs_root = self.volumes[slot as usize].removed;
            if needs_root {
                let root = self.push_root(label, slot);
                let v = &mut self.volumes[slot as usize];
                v.root = root;
                v.removed = false;
                v.complete = false;
                v.kind = kind;
                v.serial = serial;
                v.journal_id = 0;
                v.next_usn = 0;
            }
            return slot;
        }
        let slot = self.volumes.len() as u8;
        let root = self.push_root(label, slot);
        self.volumes.push(Volume {
            label: label.to_string(),
            root,
            kind,
            serial,
            journal_id: 0,
            next_usn: 0,
            complete: false,
            removed: false,
        });
        self.frn_maps.resize_with(self.volumes.len(), Default::default);
        self.frn_aliases.resize_with(self.volumes.len(), Default::default);
        slot
    }

    fn push_root(&mut self, label: &str, slot: u8) -> u32 {
        let meta = Meta { size: 0, mtime: 0, flags: flags::DIR };
        let id = self.push_entry(NONE, slot, label, None, None, meta, self.stamp, 0);
        self.entries[id as usize].flags |= flags::ROOT;
        id
    }

    /// Removes every entry of a volume (drive unplugged or deselected).
    pub fn remove_volume(&mut self, slot: u8) {
        let Some(v) = self.volumes.get(slot as usize) else { return };
        if v.removed {
            return;
        }
        let root = v.root;
        self.force_delete(root);
        self.needs_orphan_sweep = true;
        self.sweep_orphans();
        if let Some(m) = self.frn_maps.get_mut(slot as usize) {
            m.clear();
        }
        if let Some(m) = self.frn_aliases.get_mut(slot as usize) {
            m.clear();
        }
        let v = &mut self.volumes[slot as usize];
        v.removed = true;
        v.complete = false;
        v.root = NONE;
    }

    /// Removes all entries below a volume root but keeps the volume (full rebuild).
    pub fn clear_volume(&mut self, slot: u8) {
        let Some(v) = self.volumes.get(slot as usize) else { return };
        let root = v.root;
        if root == NONE {
            return;
        }
        let ids: Vec<u32> = (0..self.entries.len() as u32)
            .filter(|&i| {
                let e = &self.entries[i as usize];
                e.vol == slot && !e.is_deleted() && !e.is_root()
            })
            .collect();
        for id in ids {
            self.force_delete(id);
        }
        if let Some(m) = self.frn_maps.get_mut(slot as usize) {
            m.clear();
        }
        if let Some(m) = self.frn_aliases.get_mut(slot as usize) {
            m.clear();
        }
        let v = &mut self.volumes[slot as usize];
        v.complete = false;
        v.journal_id = 0;
        v.next_usn = 0;
    }

    // ----- mutation ------------------------------------------------------------

    /// Starts a new scan or update batch and returns its stamp.
    pub fn next_stamp(&mut self) -> u32 {
        self.stamp = self.stamp.wrapping_add(1).max(1);
        self.stamp
    }

    fn bump(&mut self, structural: bool) {
        self.generation += 1;
        if structural {
            self.structure_gen += 1;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn push_entry(
        &mut self,
        parent: u32,
        vol: u8,
        name: &str,
        folded: Option<&str>,
        raw: Option<&[u16]>,
        meta: Meta,
        stamp: u32,
        extra_flags: u16,
    ) -> u32 {
        let name = truncate_name(name);
        let id = self.entries.len() as u32;
        let name_off = self.names.len() as u32;
        self.names.push_str(name);
        let fold_off = self.folded.len() as u32;
        match folded {
            Some(f) => self.folded.push_str(truncate_name(f)),
            None => fold::fold_into(name, FoldMode::LOOSE, &mut self.folded),
        }
        let fold_len = (self.folded.len() - fold_off as usize).min(u16::MAX as usize) as u16;
        let mut fl = (meta.flags & flags::ATTRS) | extra_flags;
        if let Some(r) = raw {
            self.raw_names.insert(id, r.into());
            fl |= flags::LOSSY;
        }
        self.entries.push(Entry {
            name_off,
            fold_off,
            parent,
            stamp,
            name_len: name.len() as u16,
            fold_len,
            flags: fl,
            vol,
            reserved: 0,
            size: meta.size,
            mtime: meta.mtime,
        });
        if parent != NONE && extra_flags & flags::PENDING == 0 {
            self.insert_lookup(id);
        }
        self.live += 1;
        self.bump(true);
        id
    }

    fn insert_lookup(&mut self, id: u32) {
        let e = self.entries[id as usize];
        let h = key_hash(&self.hasher, e.parent, name_of(&self.entries, &self.names, id));
        let Self { lookup, entries, names, hasher, .. } = self;
        lookup.insert_unique(h, id, |&other| {
            let o = &entries[other as usize];
            key_hash(hasher, o.parent, name_of(entries, names, other))
        });
    }

    fn remove_lookup(&mut self, id: u32) {
        let e = self.entries[id as usize];
        if e.parent == NONE {
            return;
        }
        let h = key_hash(&self.hasher, e.parent, name_of(&self.entries, &self.names, id));
        if let Ok(occupied) = self.lookup.find_entry(h, |&x| x == id) {
            occupied.remove();
        }
    }

    /// Inserts or updates the child `name` of `parent`. Names are compared exactly,
    /// so two names that differ only by case (case-sensitive folders) stay separate.
    pub fn upsert(&mut self, parent: u32, name: &str, folded: Option<&str>, raw: Option<&[u16]>, meta: Meta, stamp: u32) -> u32 {
        let name = truncate_name(name);
        let h = key_hash(&self.hasher, parent, name);
        let (entries, names) = (&self.entries, &self.names);
        let found = self.lookup.find(h, |&id| entries[id as usize].parent == parent && name_of(entries, names, id) == name).copied();
        match found {
            Some(id) => {
                self.update_meta(id, meta);
                self.entries[id as usize].stamp = stamp;
                id
            }
            None => {
                let vol = self.entries.get(parent as usize).map_or(0, |p| p.vol);
                self.push_entry(parent, vol, name, folded, raw, meta, stamp, 0)
            }
        }
    }

    /// Adds an entry without a parent yet (bulk loads where children may arrive
    /// before their parents). It stays hidden until [`Index::attach`] is called.
    pub fn push_pending(&mut self, slot: u8, name: &str, folded: Option<&str>, raw: Option<&[u16]>, meta: Meta, stamp: u32) -> u32 {
        self.push_entry(NONE, slot, name, folded, raw, meta, stamp, flags::PENDING)
    }

    /// Gives a pending entry its parent and makes it searchable.
    pub fn attach(&mut self, id: u32, parent: u32) {
        let e = &mut self.entries[id as usize];
        if e.flags & flags::PENDING == 0 || e.is_deleted() {
            return;
        }
        e.parent = parent;
        e.flags &= !flags::PENDING;
        self.insert_lookup(id);
        self.bump(true);
    }

    /// Updates size, date and attribute flags.
    pub fn update_meta(&mut self, id: u32, meta: Meta) {
        let Some(e) = self.entries.get_mut(id as usize) else { return };
        let new_flags = (e.flags & !flags::ATTRS) | (meta.flags & flags::ATTRS);
        if e.size != meta.size || e.mtime != meta.mtime || e.flags != new_flags {
            e.size = meta.size;
            e.mtime = meta.mtime;
            e.flags = new_flags;
            self.bump(false);
        }
    }

    /// Marks an entry as seen by the scan with `stamp`.
    pub fn touch(&mut self, id: u32, stamp: u32) {
        if let Some(e) = self.entries.get_mut(id as usize) {
            e.stamp = stamp;
        }
    }

    /// Records the NTFS file reference number of `id`.
    pub fn set_frn(&mut self, id: u32, frn: u64) {
        let slot = self.entries[id as usize].vol as usize;
        if self.frn_maps.len() <= slot {
            self.frn_maps.resize_with(slot + 1, Default::default);
        }
        if self.frns.len() < self.entries.len() {
            self.frns.resize(self.entries.len(), 0);
        }
        let old = self.frns[id as usize];
        if old != 0 && old != frn {
            self.frn_maps[slot].remove(&old);
        }
        self.frns[id as usize] = frn;
        self.frn_maps[slot].insert(frn, id);
    }

    /// Records `id` as an additional hard-link name of the file `frn`. The primary
    /// entry keeps the FRN lookup; aliases are found with [`Index::aliases_of`].
    pub fn set_frn_alias(&mut self, id: u32, frn: u64) {
        let slot = self.entries[id as usize].vol as usize;
        if self.frn_aliases.len() <= slot {
            self.frn_aliases.resize_with(slot + 1, Default::default);
        }
        if self.frns.len() < self.entries.len() {
            self.frns.resize(self.entries.len(), 0);
        }
        if self.frns[id as usize] == frn {
            return;
        }
        self.drop_frn(id);
        self.frns[id as usize] = frn;
        let list = self.frn_aliases[slot].entry(frn).or_default();
        if !list.contains(&id) {
            list.push(id);
        }
    }

    /// Extra hard-link entries of the file `frn`.
    pub fn aliases_of(&self, slot: u8, frn: u64) -> Vec<u32> {
        self.frn_aliases.get(slot as usize).and_then(|m| m.get(&frn)).cloned().unwrap_or_default()
    }

    fn drop_frn(&mut self, id: u32) {
        if let Some(&frn) = self.frns.get(id as usize) {
            if frn != 0 {
                let slot = self.entries[id as usize].vol as usize;
                if let Some(m) = self.frn_maps.get_mut(slot) {
                    if m.get(&frn) == Some(&id) {
                        m.remove(&frn);
                    }
                }
                if let Some(m) = self.frn_aliases.get_mut(slot) {
                    if let Some(list) = m.get_mut(&frn) {
                        list.retain(|&x| x != id);
                        if list.is_empty() {
                            m.remove(&frn);
                        }
                    }
                }
                self.frns[id as usize] = 0;
            }
        }
    }

    fn force_delete(&mut self, id: u32) {
        let e = self.entries[id as usize];
        if e.is_deleted() {
            return;
        }
        if e.flags & flags::PENDING == 0 {
            self.remove_lookup(id);
        }
        self.drop_frn(id);
        if self.raw_names.remove(&id).is_some() {
            self.entries[id as usize].flags &= !flags::LOSSY;
        }
        self.entries[id as usize].flags |= flags::DELETED;
        self.live -= 1;
        self.deleted += 1;
        self.garbage_bytes += e.name_len as usize + e.fold_len as usize;
        if e.is_dir() {
            self.needs_orphan_sweep = true;
        }
        self.bump(true);
    }

    /// Deletes an entry. Children of a deleted folder are removed by
    /// [`Index::sweep_orphans`], which callers run once per update batch.
    pub fn remove(&mut self, id: u32) {
        match self.entries.get(id as usize) {
            Some(e) if !e.is_deleted() && !e.is_root() => self.force_delete(id),
            _ => {}
        }
    }

    /// Renames and/or moves an entry. Returns false when the move is invalid
    /// (unknown ids, or a folder moved into itself).
    pub fn rename(&mut self, id: u32, new_parent: u32, new_name: &str, raw: Option<&[u16]>) -> bool {
        let new_name = truncate_name(new_name);
        let valid = |i: u32| self.entries.get(i as usize).is_some_and(|e| !e.is_deleted());
        if !valid(id) || !valid(new_parent) || self.entries[id as usize].is_root() {
            return false;
        }
        if self.is_within(new_parent, id) {
            return false;
        }
        if let Some(other) = self.find_child(new_parent, new_name) {
            if other != id && self.name(other) == new_name {
                self.force_delete(other);
            }
        }
        let pending = self.entries[id as usize].flags & flags::PENDING != 0;
        if !pending {
            self.remove_lookup(id);
        }
        if self.name(id) != new_name {
            let e = self.entries[id as usize];
            self.garbage_bytes += e.name_len as usize + e.fold_len as usize;
            let name_off = self.names.len() as u32;
            self.names.push_str(new_name);
            let fold_off = self.folded.len() as u32;
            fold::fold_into(new_name, FoldMode::LOOSE, &mut self.folded);
            let fold_len = (self.folded.len() - fold_off as usize).min(u16::MAX as usize) as u16;
            let e = &mut self.entries[id as usize];
            e.name_off = name_off;
            e.name_len = new_name.len() as u16;
            e.fold_off = fold_off;
            e.fold_len = fold_len;
        }
        match raw {
            Some(r) => {
                self.raw_names.insert(id, r.into());
                self.entries[id as usize].flags |= flags::LOSSY;
            }
            None => {
                if self.raw_names.remove(&id).is_some() {
                    self.entries[id as usize].flags &= !flags::LOSSY;
                }
            }
        }
        let vol = self.entries[new_parent as usize].vol;
        let e = &mut self.entries[id as usize];
        e.parent = new_parent;
        e.vol = vol;
        if !pending {
            self.insert_lookup(id);
        }
        self.bump(true);
        true
    }

    /// Removes every live entry whose parent chain reaches a deleted folder.
    /// Runs in O(n) and only does work after a folder was deleted.
    pub fn sweep_orphans(&mut self) -> usize {
        if !self.needs_orphan_sweep {
            return 0;
        }
        self.needs_orphan_sweep = false;
        let dead = self.classify(|e| {
            if e.is_deleted() {
                Some(false)
            } else if e.parent == NONE {
                Some(true)
            } else {
                None
            }
        });
        let mut count = 0;
        for (i, alive) in dead.iter().enumerate() {
            if !*alive && !self.entries[i].is_deleted() {
                self.force_delete(i as u32);
                count += 1;
            }
        }
        self.needs_orphan_sweep = false;
        count
    }

    /// Deletes entries inside `subtree` (excluding `subtree` itself) whose stamp is
    /// older than `scan_stamp`: they were not seen by a rescan that started at that
    /// stamp and nothing has touched them since.
    pub fn sweep_unstamped(&mut self, subtree: u32, scan_stamp: u32) -> usize {
        let n = self.entries.len();
        let mut state = vec![0u8; n]; // 0 unknown, 1 inside, 2 outside
        if (subtree as usize) < n {
            state[subtree as usize] = 1;
        }
        let mut stack = Vec::new();
        let mut victims = Vec::new();
        for i in 0..n {
            if state[i] != 0 {
                continue;
            }
            let mut cur = i as u32;
            let verdict = loop {
                let ci = cur as usize;
                if state[ci] != 0 {
                    break state[ci];
                }
                let e = &self.entries[ci];
                if e.parent == NONE || stack.len() > MAX_DEPTH {
                    break 2;
                }
                stack.push(cur);
                cur = e.parent;
            };
            for &s in &stack {
                state[s as usize] = verdict;
            }
            stack.clear();
        }
        for (i, &st) in state.iter().enumerate() {
            if st == 1 && i as u32 != subtree {
                let e = &self.entries[i];
                if !e.is_deleted() && e.flags & flags::PENDING == 0 && e.stamp < scan_stamp {
                    victims.push(i as u32);
                }
            }
        }
        for &v in &victims {
            self.force_delete(v);
        }
        self.sweep_orphans();
        victims.len()
    }

    /// Generic memoised ancestor walk. `terminal` returns `Some(alive)` for entries
    /// that decide their own fate; everything else inherits from its parent.
    fn classify(&self, terminal: impl Fn(&Entry) -> Option<bool>) -> Vec<bool> {
        let n = self.entries.len();
        let mut state = vec![0u8; n]; // 0 unknown, 1 alive, 2 dead
        let mut stack: Vec<u32> = Vec::new();
        for i in 0..n {
            if state[i] != 0 {
                continue;
            }
            let mut cur = i as u32;
            let verdict = loop {
                let ci = cur as usize;
                if ci >= n {
                    break 2;
                }
                if state[ci] != 0 {
                    break state[ci];
                }
                if let Some(alive) = terminal(&self.entries[ci]) {
                    let v = if alive { 1 } else { 2 };
                    state[ci] = v;
                    break v;
                }
                if stack.len() > MAX_DEPTH {
                    break 2;
                }
                stack.push(cur);
                cur = self.entries[ci].parent;
            };
            for &s in &stack {
                state[s as usize] = verdict;
            }
            stack.clear();
        }
        state.into_iter().map(|s| s == 1).collect()
    }

    /// True when enough garbage piled up that [`Index::compact`] is worthwhile.
    pub fn should_compact(&self) -> bool {
        let n = self.entries.len().max(1);
        (self.deleted > 50_000 && self.deleted * 4 > n)
            || (self.garbage_bytes > 32 << 20 && self.garbage_bytes * 3 > self.names.len() + self.folded.len())
    }

    /// Drops deleted entries and wasted name bytes. Renumbers ids and bumps the epoch.
    pub fn compact(&mut self) {
        self.needs_orphan_sweep = true;
        self.sweep_orphans();
        let n = self.entries.len();
        let mut remap = vec![NONE; n];
        let mut next = 0u32;
        for (i, e) in self.entries.iter().enumerate() {
            if !e.is_deleted() {
                remap[i] = next;
                next += 1;
            }
        }
        let mut entries = Vec::with_capacity(next as usize);
        let mut names = String::with_capacity(self.names.len().saturating_sub(self.garbage_bytes / 2));
        let mut folded = String::with_capacity(self.folded.len().saturating_sub(self.garbage_bytes / 2));
        let mut frns = Vec::new();
        let keep_frns = !self.frns.is_empty();
        for (i, e) in self.entries.iter().enumerate() {
            if remap[i] == NONE {
                continue;
            }
            let mut ne = *e;
            ne.name_off = names.len() as u32;
            names.push_str(&self.names[e.name_off as usize..e.name_off as usize + e.name_len as usize]);
            ne.fold_off = folded.len() as u32;
            folded.push_str(&self.folded[e.fold_off as usize..e.fold_off as usize + e.fold_len as usize]);
            ne.parent = if e.parent == NONE { NONE } else { remap[e.parent as usize] };
            entries.push(ne);
            if keep_frns {
                frns.push(self.frns.get(i).copied().unwrap_or(0));
            }
        }
        let raw_names = std::mem::take(&mut self.raw_names)
            .into_iter()
            .filter_map(|(id, r)| {
                let nid = remap[id as usize];
                (nid != NONE).then_some((nid, r))
            })
            .collect();
        for v in &mut self.volumes {
            if v.root != NONE {
                v.root = remap[v.root as usize];
            }
        }
        self.entries = entries;
        self.names = names;
        self.folded = folded;
        self.raw_names = raw_names;
        self.frns = frns;
        self.deleted = 0;
        self.garbage_bytes = 0;
        self.rebuild_lookups();
        self.epoch += 1;
        self.bump(true);
    }

    /// Rebuilds the name lookup and FRN maps from the entry array.
    pub(crate) fn rebuild_lookups(&mut self) {
        self.lookup = HashTable::with_capacity(self.entries.len());
        for id in 0..self.entries.len() as u32 {
            let e = &self.entries[id as usize];
            if e.parent != NONE && !e.is_deleted() && e.flags & flags::PENDING == 0 {
                self.insert_lookup(id);
            }
        }
        self.frn_maps = vec![Default::default(); self.volumes.len()];
        self.frn_aliases = vec![Default::default(); self.volumes.len()];
        for (id, &frn) in self.frns.iter().enumerate() {
            if frn != 0 {
                let slot = self.entries[id].vol as usize;
                if slot < self.frn_maps.len() {
                    // The first entry of a file is the primary; later ones are hard links.
                    if self.frn_maps[slot].contains_key(&frn) {
                        self.frn_aliases[slot].entry(frn).or_default().push(id as u32);
                    } else {
                        self.frn_maps[slot].insert(frn, id as u32);
                    }
                }
            }
        }
    }

    /// Assembles an index from parts (used by the snapshot loader).
    pub(crate) fn from_parts(
        entries: Vec<Entry>,
        names: String,
        folded: String,
        volumes: Vec<Volume>,
        raw_names: HashMap<u32, Box<[u16]>>,
        frns: Vec<u64>,
    ) -> Index {
        let live = entries.iter().filter(|e| !e.is_deleted()).count();
        let deleted = entries.len() - live;
        let mut idx = Index { entries, names, folded, raw_names, frns, volumes, live, deleted, ..Index::new() };
        idx.rebuild_lookups();
        idx
    }

    // ----- search accelerators -------------------------------------------------

    /// Length in bytes of every entry's full path (0 for deleted entries). Cached.
    pub fn path_lens(&self) -> Arc<Vec<u32>> {
        let gen = self.structure_gen;
        if let Some((g, v)) = &self.caches.lock().path_lens {
            if *g == gen {
                return v.clone();
            }
        }
        let n = self.entries.len();
        let mut lens = vec![u32::MAX; n];
        let mut stack: Vec<u32> = Vec::new();
        for i in 0..n {
            if lens[i] != u32::MAX {
                continue;
            }
            let mut cur = i as u32;
            loop {
                let ci = cur as usize;
                let e = &self.entries[ci];
                if e.parent == NONE || lens[e.parent as usize] != u32::MAX || stack.len() > MAX_DEPTH {
                    stack.push(cur);
                    break;
                }
                stack.push(cur);
                cur = e.parent;
            }
            while let Some(s) = stack.pop() {
                let e = &self.entries[s as usize];
                let base = if e.parent == NONE { 0 } else { lens[e.parent as usize].saturating_add(1) };
                lens[s as usize] = base.saturating_add(e.name_len as u32);
            }
        }
        let arc = Arc::new(lens);
        self.caches.lock().path_lens = Some((gen, arc.clone()));
        arc
    }

    /// Folded full paths of all live folders. Cached.
    pub fn dir_paths(&self) -> Arc<DirPaths> {
        let gen = self.structure_gen;
        if let Some((g, v)) = &self.caches.lock().dir_paths {
            if *g == gen {
                return v.clone();
            }
        }
        let n = self.entries.len();
        let mut dp = DirPaths { off: vec![NONE; n], len: vec![0; n], pool: Vec::new() };
        let mut stack: Vec<u32> = Vec::new();
        for i in 0..n {
            let e = &self.entries[i];
            if !e.is_dir() || e.is_deleted() || dp.off[i] != NONE {
                continue;
            }
            let mut cur = i as u32;
            loop {
                let ce = &self.entries[cur as usize];
                stack.push(cur);
                if ce.parent == NONE || dp.off[ce.parent as usize] != NONE || stack.len() > MAX_DEPTH {
                    break;
                }
                cur = ce.parent;
            }
            while let Some(s) = stack.pop() {
                let e = &self.entries[s as usize];
                let start = dp.pool.len();
                if e.parent != NONE && dp.off[e.parent as usize] != NONE {
                    let p = e.parent as usize;
                    let (po, pl) = (dp.off[p] as usize, dp.len[p] as usize);
                    dp.pool.extend_from_within(po..po + pl);
                    dp.pool.push(b'\\');
                }
                dp.pool.extend_from_slice(self.folded_bytes(s));
                dp.off[s as usize] = start as u32;
                dp.len[s as usize] = (dp.pool.len() - start) as u32;
            }
        }
        let arc = Arc::new(dp);
        self.caches.lock().dir_paths = Some((gen, arc.clone()));
        arc
    }

    /// All searchable ids sorted by folded name. Cached.
    pub fn name_order(&self) -> Arc<Vec<u32>> {
        let gen = self.structure_gen;
        if let Some((g, v)) = &self.caches.lock().name_order {
            if *g == gen {
                return v.clone();
            }
        }
        let mut ids: Vec<u32> = (0..self.entries.len() as u32).filter(|&i| self.entries[i as usize].is_searchable()).collect();
        ids.par_sort_unstable_by(|&a, &b| self.cmp_names(a, b));
        let arc = Arc::new(ids);
        self.caches.lock().name_order = Some((gen, arc.clone()));
        arc
    }

    /// Total order by folded name, then exact name, then id.
    #[inline]
    pub fn cmp_names(&self, a: u32, b: u32) -> std::cmp::Ordering {
        self.folded_bytes(a).cmp(self.folded_bytes(b)).then_with(|| self.name(a).cmp(self.name(b))).then(a.cmp(&b))
    }

    /// Approximate heap usage in bytes.
    pub fn memory_bytes(&self) -> usize {
        self.entries.capacity() * std::mem::size_of::<Entry>()
            + self.names.capacity()
            + self.folded.capacity()
            + self.lookup.capacity() * 5
            + self.frns.capacity() * 8
            + self.frn_maps.iter().map(|m| m.capacity() * 13).sum::<usize>()
    }
}

/// Convenience constructor for a shared index.
pub fn shared(index: Index) -> SharedIndex {
    Arc::new(RwLock::new(index))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(size: u64) -> Meta {
        Meta { size, mtime: 1_700_000_000, flags: 0 }
    }
    fn dir() -> Meta {
        Meta { size: 0, mtime: 1_700_000_000, flags: flags::DIR }
    }

    fn sample() -> (Index, u8, u32, u32, u32) {
        let mut ix = Index::new();
        let slot = ix.add_volume("C:", VolumeKind::Standard, 1);
        let root = ix.volumes()[slot as usize].root;
        let s = ix.next_stamp();
        let docs = ix.upsert(root, "Docs", None, None, dir(), s);
        let f = ix.upsert(docs, "Invoice_2024.pdf", None, None, file(10), s);
        (ix, slot, root, docs, f)
    }

    #[test]
    fn paths_are_built_from_parents() {
        let (ix, slot, root, docs, f) = sample();
        assert_eq!(ix.path(root), "C:\\");
        assert_eq!(ix.path(docs), "C:\\Docs");
        assert_eq!(ix.path(f), "C:\\Docs\\Invoice_2024.pdf");
        assert_eq!(ix.parent_path(f), "C:\\Docs");
        assert_eq!(String::from_utf16(&ix.wide_path(f)).unwrap(), "C:\\Docs\\Invoice_2024.pdf");
        assert_eq!(ix.resolve(slot, "docs\\INVOICE_2024.PDF"), Some(f));
    }

    #[test]
    fn create_rename_move_delete() {
        let (mut ix, slot, root, docs, f) = sample();
        let s = ix.next_stamp();
        let archive = ix.upsert(root, "Archive", None, None, dir(), s);
        assert!(ix.rename(f, archive, "Invoice_2024_final.pdf", None));
        assert_eq!(ix.path(f), "C:\\Archive\\Invoice_2024_final.pdf");
        assert_eq!(ix.resolve(slot, "Docs\\Invoice_2024.pdf"), None);
        assert_eq!(ix.folded_name(f), "invoice_2024_final.pdf");
        // Moving a folder keeps its children.
        let inner = ix.upsert(docs, "inner.txt", None, None, file(1), s);
        assert!(ix.rename(docs, archive, "Docs", None));
        assert_eq!(ix.path(inner), "C:\\Archive\\Docs\\inner.txt");
        // A folder cannot move into itself.
        assert!(!ix.rename(archive, docs, "loop", None));
        // Deleting a folder removes its subtree after the sweep.
        ix.remove(archive);
        assert_eq!(ix.sweep_orphans(), 3);
        assert!(ix.entry(inner).unwrap().is_deleted());
        assert_eq!(ix.live_count(), 1);
    }

    #[test]
    fn upsert_updates_in_place_and_keeps_case_variants() {
        let (mut ix, _slot, _root, docs, f) = sample();
        let s = ix.next_stamp();
        let again = ix.upsert(docs, "Invoice_2024.pdf", None, None, file(99), s);
        assert_eq!(again, f);
        assert_eq!(ix.entry(f).unwrap().size, 99);
        let upper = ix.upsert(docs, "INVOICE_2024.pdf", None, None, file(1), s);
        assert_ne!(upper, f);
        assert_eq!(ix.find_child(docs, "INVOICE_2024.pdf"), Some(upper));
        assert_eq!(ix.find_child(docs, "Invoice_2024.pdf"), Some(f));
    }

    #[test]
    fn case_only_rename() {
        let (mut ix, _slot, _root, docs, f) = sample();
        assert!(ix.rename(f, docs, "INVOICE_2024.PDF", None));
        assert_eq!(ix.name(f), "INVOICE_2024.PDF");
        assert_eq!(ix.find_child(docs, "invoice_2024.pdf"), Some(f));
    }

    #[test]
    fn rescan_sweep_removes_unseen_entries() {
        let (mut ix, _slot, root, docs, f) = sample();
        let s = ix.next_stamp();
        let other = ix.upsert(root, "other.txt", None, None, file(1), s);
        let scan = ix.next_stamp();
        ix.touch(docs, scan);
        // `f` was not seen by the scan; `other` was not either.
        let removed = ix.sweep_unstamped(root, scan);
        assert_eq!(removed, 2);
        assert!(ix.entry(f).unwrap().is_deleted());
        assert!(ix.entry(other).unwrap().is_deleted());
        assert!(!ix.entry(docs).unwrap().is_deleted());
    }

    #[test]
    fn burst_of_100k_changes() {
        let mut ix = Index::new();
        let slot = ix.add_volume("D:", VolumeKind::Standard, 2);
        let root = ix.volumes()[slot as usize].root;
        let s = ix.next_stamp();
        let d = ix.upsert(root, "build", None, None, dir(), s);
        let ids: Vec<u32> = (0..100_000).map(|i| ix.upsert(d, &format!("obj_{i}.o"), None, None, file(i), s)).collect();
        assert_eq!(ix.live_count(), 100_002);
        for (i, &id) in ids.iter().enumerate().step_by(2) {
            assert!(ix.rename(id, d, &format!("renamed_{i}.o"), None));
        }
        for &id in ids.iter().skip(1).step_by(2) {
            ix.remove(id);
        }
        assert_eq!(ix.live_count(), 50_002);
        assert_eq!(ix.resolve(slot, "build\\renamed_0.o"), Some(ids[0]));
        ix.compact();
        assert_eq!(ix.len(), 50_002);
        assert_eq!(ix.deleted_count(), 0);
        let id = ix.resolve(slot, "build\\renamed_99998.o").unwrap();
        assert_eq!(ix.path(id), "D:\\build\\renamed_99998.o");
    }

    #[test]
    fn frn_tracking_survives_compaction() {
        let (mut ix, slot, _root, docs, f) = sample();
        ix.set_frn(f, 0x0001_0000_0000_0042);
        ix.set_frn(docs, 0x0001_0000_0000_0041);
        ix.remove(docs);
        ix.sweep_orphans();
        assert_eq!(ix.by_frn(slot, 0x0001_0000_0000_0042), None);
        let s = ix.next_stamp();
        let root = ix.volumes()[0].root;
        let g = ix.upsert(root, "g.txt", None, None, file(1), s);
        ix.set_frn(g, 77);
        ix.compact();
        let g2 = ix.by_frn(slot, 77).unwrap();
        assert_eq!(ix.name(g2), "g.txt");
    }

    #[test]
    fn hard_link_aliases() {
        let (mut ix, slot, root, docs, f) = sample();
        ix.set_frn(f, 500);
        let s = ix.next_stamp();
        let link = ix.upsert(root, "Invoice_link.pdf", None, None, file(10), s);
        ix.set_frn_alias(link, 500);
        assert_eq!(ix.by_frn(slot, 500), Some(f));
        assert_eq!(ix.aliases_of(slot, 500), vec![link]);
        ix.compact();
        let f2 = ix.by_frn(slot, 500).unwrap();
        let aliases = ix.aliases_of(slot, 500);
        assert_eq!(aliases.len(), 1);
        assert_ne!(aliases[0], f2);
        ix.remove(aliases[0]);
        assert!(ix.aliases_of(slot, 500).is_empty());
        assert_eq!(ix.by_frn(slot, 500), Some(f2));
        let _ = docs;
    }

    #[test]
    fn pending_entries_attach_later() {
        let mut ix = Index::new();
        let slot = ix.add_volume("E:", VolumeKind::Fast, 3);
        let root = ix.volumes()[0].root;
        let s = ix.next_stamp();
        let child = ix.push_pending(slot, "child.txt", None, None, file(1), s);
        let parent = ix.push_pending(slot, "parent", None, None, dir(), s);
        assert!(!ix.entry(child).unwrap().is_searchable());
        ix.attach(parent, root);
        ix.attach(child, parent);
        assert!(ix.entry(child).unwrap().is_searchable());
        assert_eq!(ix.path(child), "E:\\parent\\child.txt");
        assert_eq!(ix.resolve(slot, "parent\\child.txt"), Some(child));
    }

    #[test]
    fn caches_follow_structure_changes() {
        let (mut ix, _slot, _root, docs, f) = sample();
        let lens = ix.path_lens();
        assert_eq!(lens[f as usize] as usize, "C:\\Docs\\Invoice_2024.pdf".len());
        let dp = ix.dir_paths();
        assert_eq!(dp.get(docs), Some(&b"c:\\docs"[..]));
        assert!(dp.get(f).is_none());
        ix.rename(docs, ix.volumes()[0].root, "Papers", None);
        assert_eq!(ix.dir_paths().get(docs), Some(&b"c:\\papers"[..]));
        let order = ix.name_order();
        assert_eq!(order.len(), 2);
    }

    #[test]
    fn volumes_can_be_removed_and_revived() {
        let (mut ix, slot, _root, _docs, f) = sample();
        ix.remove_volume(slot);
        assert!(ix.entry(f).unwrap().is_deleted());
        assert_eq!(ix.live_count(), 0);
        let again = ix.add_volume("c:", VolumeKind::Standard, 1);
        assert_eq!(again, slot);
        assert!(!ix.volumes()[0].removed);
        assert_eq!(ix.live_count(), 1);
    }

    #[test]
    fn lossy_names_keep_exact_utf16() {
        let mut ix = Index::new();
        let slot = ix.add_volume("C:", VolumeKind::Standard, 1);
        let root = ix.volumes()[slot as usize].root;
        let raw: Vec<u16> = vec![u16::from(b'a'), 0xD800, u16::from(b'b')];
        let lossy = String::from_utf16_lossy(&raw);
        let s = ix.next_stamp();
        let id = ix.upsert(root, &lossy, None, Some(&raw), file(1), s);
        let wide = ix.wide_path(id);
        assert_eq!(&wide[3..], &raw[..]);
    }
}
