//! Applies watcher events to the index (standard mode).
//!
//! File system calls (stat) happen before the write lock is taken, so searches are
//! blocked only for the in-memory update. Renames keep the entry id, so a moved
//! folder keeps its whole subtree without rescanning.

use crate::walk::Exclusions;
use crate::watch::{FsAction, FsEvent};
use crate::win::{join, split_last, stat, FindItem};
use findit_core::{flags, Index, SharedIndex};

/// What the caller still has to do after applying a batch.
#[derive(Debug, Default)]
pub struct ApplyOutcome {
    /// Folders to scan: new folders (their contents produced no events) and folders
    /// whose state is uncertain. `(id, display path)`.
    pub rescan: Vec<(u32, Vec<u16>)>,
    /// The kernel buffer overflowed; the whole volume needs a rescan.
    pub overflow: bool,
    /// The watch failed with this Win32 error.
    pub failed: Option<u32>,
    /// The index changed.
    pub changed: bool,
}

enum Op {
    Upsert { rel: Vec<u16>, item: FindItem, new: bool },
    Remove { rel: Vec<u16> },
    Rename { old: Vec<u16>, new: Vec<u16>, item: Option<FindItem> },
}

fn rel_string(rel: &[u16]) -> String {
    String::from_utf16_lossy(rel)
}

fn excluded(excl: &Exclusions, root: &[u16], rel: &[u16]) -> bool {
    let rs = rel_string(rel);
    excl.excludes_any_component(&rs) || (excl.has_prefixes() && excl.excludes_path(&rel_string(&join(root, rel))))
}

/// Resolves a relative path; on failure returns the deepest existing ancestor.
fn resolve(ix: &Index, slot: u8, rel: &[u16]) -> Result<u32, (u32, usize)> {
    let root = ix.volume(slot).map_or(findit_core::NONE, |v| v.root);
    let mut cur = root;
    let mut consumed = 0usize;
    for comp in rel.split(|&c| c == u16::from(b'\\')).filter(|c| !c.is_empty()) {
        let name = String::from_utf16_lossy(comp);
        match ix.find_child(cur, &name) {
            Some(id) => {
                cur = id;
                consumed += comp.len() + 1;
            }
            None => return Err((cur, consumed.saturating_sub(1))),
        }
    }
    Ok(cur)
}

fn schedule(ix: &Index, id: u32, out: &mut ApplyOutcome) {
    let mut path: Vec<u16> = ix.wide_path(id);
    if path.last() == Some(&u16::from(b'\\')) {
        path.pop();
    }
    out.rescan.push((id, path));
}

/// Applies one batch of events for the volume in `slot` whose root path is `root`
/// (for example `C:`).
pub fn apply_events(index: &SharedIndex, slot: u8, root: &[u16], events: &[FsEvent], excl: &Exclusions) -> ApplyOutcome {
    let mut out = ApplyOutcome::default();
    let mut ops = Vec::new();
    let mut i = 0;
    while i < events.len() {
        match &events[i] {
            FsEvent::Overflow => out.overflow = true,
            FsEvent::Failed(code) => out.failed = Some(*code),
            FsEvent::Change { action, path } => {
                if excluded(excl, root, path) {
                    // A rename out of an excluded folder still has to be handled below.
                    let next_is_new = matches!(events.get(i + 1), Some(FsEvent::Change { action: FsAction::RenamedNew, .. }));
                    if !(*action == FsAction::RenamedOld && next_is_new) {
                        i += 1;
                        continue;
                    }
                }
                match action {
                    FsAction::Added | FsAction::Modified | FsAction::RenamedNew => {
                        let new = *action != FsAction::Modified;
                        match stat(&join(root, path)) {
                            Some(item) => ops.push(Op::Upsert { rel: path.clone(), item, new }),
                            None => ops.push(Op::Remove { rel: path.clone() }),
                        }
                    }
                    FsAction::Removed => ops.push(Op::Remove { rel: path.clone() }),
                    FsAction::RenamedOld => {
                        if let Some(FsEvent::Change { action: FsAction::RenamedNew, path: new }) = events.get(i + 1) {
                            i += 1;
                            if excluded(excl, root, new) {
                                ops.push(Op::Remove { rel: path.clone() });
                            } else {
                                let item = stat(&join(root, new));
                                ops.push(Op::Rename { old: path.clone(), new: new.clone(), item });
                            }
                        } else {
                            ops.push(Op::Remove { rel: path.clone() });
                        }
                    }
                }
            }
        }
        i += 1;
    }
    if ops.is_empty() {
        return out;
    }

    let mut ix = index.write();
    if ix.volume(slot).is_none_or(|v| v.removed) {
        return out;
    }
    let stamp = ix.next_stamp();
    let gen_before = ix.generation();
    for op in ops {
        match op {
            Op::Upsert { rel, item, new } => {
                let (parent_rel, _) = split_last(&rel);
                match resolve(&ix, slot, parent_rel) {
                    Ok(parent) => {
                        let id = ix.upsert(parent, &item.name, None, item.raw.as_deref(), item.meta, stamp);
                        if new && item.meta.flags & flags::DIR != 0 && item.meta.flags & flags::LINK == 0 {
                            schedule(&ix, id, &mut out);
                        }
                    }
                    Err((ancestor, _)) => schedule(&ix, ancestor, &mut out),
                }
            }
            Op::Remove { rel } => {
                if let Ok(id) = resolve(&ix, slot, &rel) {
                    ix.remove(id);
                }
            }
            Op::Rename { old, new, item } => {
                let (new_parent_rel, _) = split_last(&new);
                let old_id = resolve(&ix, slot, &old).ok();
                let new_parent = resolve(&ix, slot, new_parent_rel);
                match (old_id, new_parent, item) {
                    (Some(id), Ok(parent), Some(item)) => {
                        if ix.rename(id, parent, &item.name, item.raw.as_deref()) {
                            ix.update_meta(id, item.meta);
                            ix.touch(id, stamp);
                        } else {
                            ix.remove(id);
                            let nid = ix.upsert(parent, &item.name, None, item.raw.as_deref(), item.meta, stamp);
                            if item.meta.flags & flags::DIR != 0 {
                                schedule(&ix, nid, &mut out);
                            }
                        }
                    }
                    (Some(id), _, None) => ix.remove(id),
                    (None, Ok(parent), Some(item)) => {
                        let nid = ix.upsert(parent, &item.name, None, item.raw.as_deref(), item.meta, stamp);
                        if item.meta.flags & flags::DIR != 0 && item.meta.flags & flags::LINK == 0 {
                            schedule(&ix, nid, &mut out);
                        }
                    }
                    (old_id, Err((ancestor, _)), _) => {
                        if let Some(id) = old_id {
                            ix.remove(id);
                        }
                        schedule(&ix, ancestor, &mut out);
                    }
                    (None, Ok(_), None) => {}
                }
            }
        }
    }
    ix.sweep_orphans();
    out.changed = ix.generation() != gen_before;
    out.rescan.sort_by_key(|r| r.0);
    out.rescan.dedup_by_key(|r| r.0);
    out
}
