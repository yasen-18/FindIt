//! Keeps the index complete and current.
//!
//! Threads owned by the engine:
//! * startup: loads the snapshot, then starts indexing;
//! * one scan thread per volume being walked;
//! * one watcher thread per standard-mode volume (`ReadDirectoryChangesW`);
//! * the applier: batches watcher events for 100 ms and applies them;
//! * the notifier: tells the UI about changes at most 10 times per second;
//! * the saver: writes the snapshot every 10 minutes when something changed;
//! * the drive poller: notices drives being plugged in or removed;
//! * the fast-mode session (talks to the elevated helper).

use crate::paths::DataPaths;
use crate::settings::Settings;
use findit_core::snapshot::{self, SnapshotError};
use findit_core::{Index, SharedIndex, VolumeKind};
use findit_indexing::apply::apply_events;
use findit_indexing::drives::{self, DriveInfo, DriveKind};
use findit_indexing::fast::{run_fast, FastApplier, FastEvent, FastRequest};
use findit_indexing::pipe;
use findit_indexing::proto::read_msg;
use findit_indexing::walk::{walk, Exclusions, ScanControl, ScanStats};
use findit_indexing::watch::{FsEvent, Watcher};
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

const SAVE_EVERY: Duration = Duration::from_secs(600);
const NOTIFY_EVERY: Duration = Duration::from_millis(100);
const DRIVE_POLL: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Phase {
    Waiting,
    Scanning,
    Enumerating,
    Filling,
    Ready,
    Error,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VolumeStatus {
    pub label: String,
    pub fast: bool,
    pub phase: Phase,
    pub count: u64,
    pub denied: u64,
    pub message: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub loading: bool,
    pub indexing: bool,
    pub paused: bool,
    pub total: usize,
    pub volumes: Vec<VolumeStatus>,
    pub denied: u64,
    /// `off`, `starting`, `on`, `declined`, `failed` or `unsupported`.
    pub fast_mode: String,
    pub message: Option<String>,
    pub generation: u64,
    pub epoch: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexEvent {
    pub changed: bool,
    pub status: Status,
}

struct VolRuntime {
    fast: bool,
    phase: Phase,
    stats: Arc<ScanStats>,
    fast_count: u64,
    message: Option<String>,
}

impl VolRuntime {
    fn new(fast: bool) -> VolRuntime {
        VolRuntime { fast, phase: Phase::Waiting, stats: Arc::new(ScanStats::default()), fast_count: 0, message: None }
    }
}

struct FastSession {
    stop: Arc<AtomicBool>,
}

pub struct Engine {
    pub index: SharedIndex,
    pub paths: DataPaths,
    pub settings: Arc<RwLock<Settings>>,
    app: AppHandle,
    ctl: Arc<ScanControl>,
    /// One control per drive, so switching a drive to fast mode can stop its walk.
    scan_ctl: Mutex<HashMap<char, Arc<ScanControl>>>,
    vols: Mutex<BTreeMap<char, VolRuntime>>,
    watchers: Mutex<HashMap<char, Watcher>>,
    events: mpsc::Sender<(char, Vec<FsEvent>)>,
    events_rx: Mutex<Option<mpsc::Receiver<(char, Vec<FsEvent>)>>>,
    scanning: Mutex<HashSet<char>>,
    rescan_wanted: Mutex<HashSet<char>>,
    fast: Mutex<Option<FastSession>>,
    fast_state: Mutex<String>,
    excl: RwLock<Exclusions>,
    loading: AtomicBool,
    indexing_started: AtomicBool,
    shutting_down: AtomicBool,
    saved_gen: AtomicU64,
    message: Mutex<Option<String>>,
}

fn label(letter: char) -> String {
    format!("{letter}:")
}

fn wide_root(letter: char) -> Vec<u16> {
    label(letter).encode_utf16().collect()
}

impl Engine {
    pub fn new(app: AppHandle, paths: DataPaths, settings: Arc<RwLock<Settings>>) -> Arc<Engine> {
        let (tx, rx) = mpsc::channel();
        let excl = Self::make_exclusions(&settings.read(), &paths);
        Arc::new(Engine {
            index: findit_core::shared(Index::new()),
            paths,
            settings,
            app,
            ctl: Arc::new(ScanControl::default()),
            scan_ctl: Mutex::new(HashMap::new()),
            vols: Mutex::new(BTreeMap::new()),
            watchers: Mutex::new(HashMap::new()),
            events: tx,
            events_rx: Mutex::new(Some(rx)),
            scanning: Mutex::new(HashSet::new()),
            rescan_wanted: Mutex::new(HashSet::new()),
            fast: Mutex::new(None),
            fast_state: Mutex::new("off".into()),
            excl: RwLock::new(excl),
            loading: AtomicBool::new(true),
            indexing_started: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
            saved_gen: AtomicU64::new(0),
            message: Mutex::new(None),
        })
    }

    fn make_exclusions(s: &Settings, paths: &DataPaths) -> Exclusions {
        let mut e = Exclusions::new(&s.exclusions);
        // FindIt's own files change all the time; indexing them would only add churn.
        e.add_prefix(&paths.root.to_string_lossy());
        e
    }

    fn stopping(&self) -> bool {
        self.shutting_down.load(Ordering::Relaxed)
    }

    /// The scan control of one drive (created on first use, inheriting pause).
    fn volume_ctl(&self, letter: char) -> Arc<ScanControl> {
        let mut map = self.scan_ctl.lock();
        map.entry(letter)
            .or_insert_with(|| {
                let c = ScanControl::default();
                c.paused.store(self.ctl.paused.load(Ordering::Relaxed), Ordering::Relaxed);
                Arc::new(c)
            })
            .clone()
    }

    fn cancel_scan(&self, letter: char) {
        if let Some(c) = self.scan_ctl.lock().get(&letter) {
            c.cancel.store(true, Ordering::Relaxed);
        }
    }

    fn cancel_all_scans(&self) {
        self.ctl.cancel.store(true, Ordering::Relaxed);
        for c in self.scan_ctl.lock().values() {
            c.cancel.store(true, Ordering::Relaxed);
        }
    }

    // ----- startup ------------------------------------------------------------------

    /// Loads the snapshot and starts the background threads. Indexing itself
    /// starts right away unless the first-run wizard has not been completed.
    pub fn start(self: &Arc<Self>) {
        let me = self.clone();
        spawn("findit-startup", move || {
            me.load_snapshot();
            me.loading.store(false, Ordering::Relaxed);
            if me.settings.read().first_run_done {
                me.begin_indexing(false);
            }
        });
        if let Some(rx) = self.events_rx.lock().take() {
            let me = self.clone();
            spawn("findit-apply", move || me.applier(rx));
        }
        let me = self.clone();
        spawn("findit-notify", move || me.notifier());
        let me = self.clone();
        spawn("findit-save", move || me.saver());
        let me = self.clone();
        spawn("findit-drives", move || me.drive_poller());
    }

    fn load_snapshot(&self) {
        let t = Instant::now();
        match snapshot::load(&self.paths.snapshot) {
            Ok(mut ix) => {
                ix.advance_epoch_past(self.index.read().epoch());
                let gen = ix.generation();
                log::info!(target: "findit::engine", "snapshot loaded: {} entries in {:?}", ix.live_count(), t.elapsed());
                *self.index.write() = ix;
                self.saved_gen.store(gen, Ordering::Relaxed);
            }
            Err(SnapshotError::Missing) => log::info!(target: "findit::engine", "no snapshot yet"),
            Err(e) => {
                log::warn!(target: "findit::engine", "snapshot rejected ({e}); rebuilding in the background");
                *self.message.lock() = Some(crate::strings::t("status.rebuildingCorrupt"));
                let _ = std::fs::remove_file(&self.paths.snapshot);
            }
        }
    }

    /// Drives the user wants indexed and that are present now.
    pub fn selected_drives(&self) -> Vec<DriveInfo> {
        let s = self.settings.read();
        drives::list_drives()
            .into_iter()
            .filter(|d| d.kind != DriveKind::Other && s.drive_enabled(&d.label, d.kind == DriveKind::Fixed))
            .collect()
    }

    /// Starts (or restarts) indexing of all selected drives. `explicit` is true when
    /// the user just chose fast mode (declining the prompt then turns it off).
    pub fn begin_indexing(self: &Arc<Self>, explicit: bool) {
        self.indexing_started.store(true, Ordering::Relaxed);
        let selected = self.selected_drives();
        let wanted: HashSet<char> = selected.iter().map(|d| d.letter).collect();
        // Forget volumes that are no longer selected or present.
        {
            let mut ix = self.index.write();
            let stale: Vec<u8> = ix
                .volumes()
                .iter()
                .enumerate()
                .filter(|(_, v)| !v.removed && v.label.chars().next().is_none_or(|c| !wanted.contains(&c)))
                .map(|(i, _)| i as u8)
                .collect();
            for slot in stale {
                ix.remove_volume(slot);
            }
        }
        let fast_mode = self.settings.read().fast_mode;
        let (fast, standard): (Vec<&DriveInfo>, Vec<&DriveInfo>) =
            selected.iter().partition(|d| fast_mode && d.kind == DriveKind::Fixed && d.is_ntfs());
        if fast_mode && fast.is_empty() {
            *self.fast_state.lock() = "unsupported".into();
            if explicit {
                let mut s = self.settings.write();
                s.fast_mode = false;
                let _ = s.save(&self.paths.settings);
            }
        }
        for d in &standard {
            self.scan_volume(d.letter);
        }
        if !fast.is_empty() {
            self.start_fast(fast.iter().map(|d| d.letter).collect(), explicit);
        }
    }

    // ----- standard mode ------------------------------------------------------------

    fn ensure_watcher(self: &Arc<Self>, letter: char, network: bool) {
        let mut watchers = self.watchers.lock();
        if watchers.contains_key(&letter) {
            return;
        }
        let tx = self.events.clone();
        let buffer = if network { 64 * 1024 } else { 1 << 20 };
        match Watcher::start(&wide_root(letter), buffer, move |ev| {
            let _ = tx.send((letter, ev));
        }) {
            Ok(w) => {
                watchers.insert(letter, w);
            }
            Err(e) => log::warn!(target: "findit::engine", "cannot watch {letter}: ({e})"),
        }
    }

    fn stop_watcher(&self, letter: char) {
        let w = self.watchers.lock().remove(&letter);
        if let Some(w) = w {
            w.stop();
        }
    }

    /// Walks a whole volume (merging with what is already indexed) and keeps it
    /// current with a watcher.
    pub fn scan_volume(self: &Arc<Self>, letter: char) {
        if self.stopping() {
            return;
        }
        if !self.scanning.lock().insert(letter) {
            self.rescan_wanted.lock().insert(letter);
            return;
        }
        let info = drives::drive_info(letter);
        let network = info.as_ref().is_some_and(|d| d.kind == DriveKind::Network);
        let serial = info.as_ref().map_or(0, |d| d.serial);
        let stats = Arc::new(ScanStats::default());
        let ctl = self.volume_ctl(letter);
        ctl.cancel.store(false, Ordering::Relaxed);
        {
            let mut vols = self.vols.lock();
            let v = vols.entry(letter).or_insert_with(|| VolRuntime::new(false));
            v.fast = false;
            v.phase = Phase::Scanning;
            v.stats = stats.clone();
            v.message = None;
        }
        let me = self.clone();
        spawn("findit-scan", move || {
            let (root, stamp, complete) = {
                let mut ix = me.index.write();
                let slot = ix.add_volume(&label(letter), VolumeKind::Standard, serial);
                let v = ix.volume_mut(slot).expect("volume just added");
                v.kind = VolumeKind::Standard;
                if v.serial != serial && serial != 0 {
                    // A reformatted or different disk with the same letter.
                    v.serial = serial;
                    v.complete = false;
                }
                let (root, complete) = (v.root, v.complete);
                (root, ix.next_stamp(), complete)
            };
            me.ensure_watcher(letter, network);
            let threads = if complete { 2 } else { std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(2, 8) };
            let t = Instant::now();
            let excl = me.excl.read().clone();
            walk(&me.index, root, &wide_root(letter), stamp, &excl, &ctl, &stats, threads);
            let cancelled = ctl.cancel.load(Ordering::Relaxed) || me.stopping();
            if !cancelled {
                let mut ix = me.index.write();
                if let Some(slot) = ix.volume_slot(&label(letter)) {
                    if ix.volume(slot).is_some_and(|v| v.root == root && !v.removed) {
                        let removed = ix.sweep_unstamped(root, stamp);
                        if let Some(v) = ix.volume_mut(slot) {
                            v.complete = true;
                        }
                        log::info!(
                            target: "findit::engine",
                            "{letter}: scanned {} entries in {:?} ({} stale removed, {} folders denied)",
                            stats.entries.load(Ordering::Relaxed),
                            t.elapsed(),
                            removed,
                            stats.denied.load(Ordering::Relaxed)
                        );
                    }
                }
            }
            if let Some(v) = me.vols.lock().get_mut(&letter) {
                if !v.fast {
                    v.phase = if cancelled { Phase::Waiting } else { Phase::Ready };
                }
            }
            me.scanning.lock().remove(&letter);
            if !cancelled && me.rescan_wanted.lock().remove(&letter) {
                me.scan_volume(letter);
            }
        });
    }

    fn applier(self: Arc<Self>, rx: mpsc::Receiver<(char, Vec<FsEvent>)>) {
        while let Ok(first) = rx.recv() {
            let mut batch: BTreeMap<char, Vec<FsEvent>> = BTreeMap::new();
            batch.entry(first.0).or_default().extend(first.1);
            // Coalesce bursts (git checkout, unzip, builds) into one update.
            let until = Instant::now() + Duration::from_millis(100);
            while let Some(left) = until.checked_duration_since(Instant::now()) {
                match rx.recv_timeout(left) {
                    Ok((l, ev)) => batch.entry(l).or_default().extend(ev),
                    Err(_) => break,
                }
            }
            while self.ctl.paused.load(Ordering::Relaxed) && !self.stopping() {
                std::thread::sleep(Duration::from_millis(250));
                while let Ok((l, ev)) = rx.try_recv() {
                    batch.entry(l).or_default().extend(ev);
                }
            }
            if self.stopping() {
                return;
            }
            for (letter, events) in batch {
                let slot = {
                    let ix = self.index.read();
                    ix.volume_slot(&label(letter)).filter(|&s| ix.volume(s).is_some_and(|v| !v.removed && v.kind == VolumeKind::Standard))
                };
                let Some(slot) = slot else { continue };
                let excl = self.excl.read().clone();
                let out = apply_events(&self.index, slot, &wide_root(letter), &events, &excl);
                if out.overflow {
                    log::info!(target: "findit::engine", "{letter}: change buffer overflowed, rescanning");
                    self.scan_volume(letter);
                }
                if let Some(code) = out.failed {
                    log::warn!(target: "findit::engine", "{letter}: watcher stopped (error {code})");
                    self.stop_watcher(letter);
                }
                for (id, path) in out.rescan {
                    let stamp = self.index.write().next_stamp();
                    let stats = ScanStats::default();
                    walk(&self.index, id, &path, stamp, &excl, &self.ctl, &stats, 2);
                    let mut ix = self.index.write();
                    if ix.entry(id).is_some_and(|e| !e.is_deleted()) {
                        ix.sweep_unstamped(id, stamp);
                    }
                }
            }
            let compact = self.index.read().should_compact();
            if compact {
                self.index.write().compact();
            }
        }
    }

    // ----- fast mode ----------------------------------------------------------------

    /// Starts the NTFS fast mode for `letters`. Without administrator rights this
    /// launches the helper, which shows a UAC prompt. `explicit` is true when the
    /// user just pressed "Enable fast mode": declining then turns the setting off.
    pub fn start_fast(self: &Arc<Self>, letters: Vec<char>, explicit: bool) {
        self.stop_fast();
        for &l in &letters {
            self.cancel_scan(l);
            self.stop_watcher(l);
            let mut vols = self.vols.lock();
            let v = vols.entry(l).or_insert_with(|| VolRuntime::new(true));
            v.fast = true;
            v.phase = Phase::Waiting;
            v.message = None;
        }
        *self.fast_state.lock() = "starting".into();
        let reqs: Vec<FastRequest> = {
            let ix = self.index.read();
            letters
                .iter()
                .map(|&l| {
                    let resume = ix
                        .volume_slot(&label(l))
                        .and_then(|s| ix.volume(s))
                        .filter(|v| v.kind == VolumeKind::Fast && v.complete && !v.removed && v.journal_id != 0)
                        .map(|v| (v.journal_id, v.next_usn));
                    FastRequest { letter: l, resume }
                })
                .collect()
        };
        let stop = Arc::new(AtomicBool::new(false));
        *self.fast.lock() = Some(FastSession { stop: stop.clone() });
        let me = self.clone();
        spawn("findit-fast", move || {
            let mut applier = FastApplier::new(me.index.clone(), me.excl.read().clone());
            let mut handle = |msg| -> bool {
                let ev = applier.apply(msg);
                me.on_fast_event(ev);
                !stop.load(Ordering::Relaxed) && !me.stopping()
            };
            if pipe::is_elevated() {
                *me.fast_state.lock() = "on".into();
                run_fast(&reqs, &mut handle, &stop, &mut || {});
            } else {
                me.run_helper(&reqs, &mut handle, explicit);
            }
            if !stop.load(Ordering::Relaxed) && !me.stopping() {
                // The helper went away: keep the drives indexed the standard way.
                let still_fast: Vec<char> = me.vols.lock().iter().filter(|(_, v)| v.fast).map(|(l, _)| *l).collect();
                for l in still_fast {
                    me.fallback_standard(l, crate::strings::t("status.fastStopped"));
                }
            }
        });
    }

    fn run_helper(
        self: &Arc<Self>,
        reqs: &[FastRequest],
        handle: &mut dyn FnMut(findit_indexing::proto::HelperMsg) -> bool,
        explicit: bool,
    ) {
        let name = match pipe::random_pipe_name() {
            Ok(n) => n,
            Err(e) => {
                self.fast_failed(&format!("{e}"));
                return;
            }
        };
        let vols: Vec<String> = reqs.iter().map(FastRequest::to_arg).collect();
        let args = format!("--findit-helper --pipe {name} --parent {} --volumes {}", std::process::id(), vols.join(","));
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                self.fast_failed(&format!("{e}"));
                return;
            }
        };
        let owner = self.app.get_webview_window("main").and_then(|w| w.hwnd().ok()).map_or(std::ptr::null_mut(), |h| h.0);
        let child = match pipe::launch_elevated(&exe, &args, owner) {
            Ok(c) => c,
            Err(pipe::LaunchError::Declined) => {
                log::info!(target: "findit::engine", "fast mode: administrator prompt declined");
                *self.fast_state.lock() = "declined".into();
                if explicit {
                    let mut s = self.settings.write();
                    s.fast_mode = false;
                    let _ = s.save(&self.paths.settings);
                }
                self.fallback_all(crate::strings::t("status.fastDeclined"));
                return;
            }
            Err(pipe::LaunchError::Failed(e)) => {
                self.fast_failed(&format!("{e}"));
                return;
            }
        };
        let mut stream = match pipe::connect(&name, &child, Duration::from_secs(20)) {
            Ok(s) => s,
            Err(e) => {
                child.terminate();
                self.fast_failed(&format!("{e}"));
                return;
            }
        };
        *self.fast_state.lock() = "on".into();
        log::info!(target: "findit::engine", "fast mode: helper connected (pid {})", child.pid);
        loop {
            match read_msg(&mut stream) {
                Ok(msg) => {
                    if !handle(msg) {
                        break;
                    }
                }
                Err(e) => {
                    if !self.stopping() {
                        log::warn!(target: "findit::engine", "fast mode: helper connection ended ({e})");
                    }
                    break;
                }
            }
        }
        drop(stream);
    }

    fn fast_failed(self: &Arc<Self>, why: &str) {
        log::warn!(target: "findit::engine", "fast mode failed: {why}");
        *self.fast_state.lock() = "failed".into();
        self.fallback_all(crate::strings::t("status.fastFailed"));
    }

    fn fallback_all(self: &Arc<Self>, message: String) {
        let letters: Vec<char> = self.vols.lock().iter().filter(|(_, v)| v.fast).map(|(l, _)| *l).collect();
        for l in letters {
            self.fallback_standard(l, message.clone());
        }
    }

    fn fallback_standard(self: &Arc<Self>, letter: char, message: String) {
        {
            let mut vols = self.vols.lock();
            let v = vols.entry(letter).or_insert_with(|| VolRuntime::new(false));
            v.fast = false;
            v.message = Some(message);
        }
        {
            let mut ix = self.index.write();
            if let Some(slot) = ix.volume_slot(&label(letter)) {
                if let Some(v) = ix.volume_mut(slot) {
                    v.kind = VolumeKind::Standard;
                }
            }
        }
        if !self.stopping() {
            self.scan_volume(letter);
        }
    }

    fn on_fast_event(self: &Arc<Self>, ev: FastEvent) {
        let mut vols = self.vols.lock();
        match ev {
            FastEvent::Progress { vol, count } => {
                let v = vols.entry(vol).or_insert_with(|| VolRuntime::new(true));
                v.phase = Phase::Enumerating;
                v.fast_count = count as u64;
            }
            FastEvent::NamesReady { vol } => {
                if let Some(v) = vols.get_mut(&vol) {
                    v.phase = Phase::Filling;
                }
            }
            FastEvent::Ready { vol } => {
                if let Some(v) = vols.get_mut(&vol) {
                    v.phase = Phase::Ready;
                }
            }
            FastEvent::Failed { vol, message } => {
                drop(vols);
                log::warn!(target: "findit::engine", "fast mode: {vol}: {message}");
                self.fallback_standard(vol, crate::strings::t("status.fastFailed"));
            }
            FastEvent::Fatal(message) => {
                drop(vols);
                self.fast_failed(&message);
            }
            FastEvent::Changed | FastEvent::Nothing => {}
        }
    }

    fn stop_fast(&self) {
        if let Some(s) = self.fast.lock().take() {
            s.stop.store(true, Ordering::Relaxed);
        }
    }

    /// Turns fast mode off and goes back to standard scanning.
    pub fn disable_fast(self: &Arc<Self>) {
        self.stop_fast();
        *self.fast_state.lock() = "off".into();
        let letters: Vec<char> = self.vols.lock().iter().filter(|(_, v)| v.fast).map(|(l, _)| *l).collect();
        for l in letters {
            self.fallback_standard(l, String::new());
            if let Some(v) = self.vols.lock().get_mut(&l) {
                v.message = None;
            }
        }
    }

    /// Fixed NTFS drives that fast mode can serve.
    pub fn fast_capable(&self) -> Vec<char> {
        self.selected_drives().into_iter().filter(|d| d.kind == DriveKind::Fixed && d.is_ntfs()).map(|d| d.letter).collect()
    }

    // ----- settings, rebuild, pause -------------------------------------------------

    /// Applies changed settings that affect indexing.
    pub fn settings_changed(self: &Arc<Self>, old: &Settings, new: &Settings) {
        if old.exclusions != new.exclusions {
            *self.excl.write() = Self::make_exclusions(new, &self.paths);
            let removed_some = old.exclusions.iter().any(|e| !new.exclusions.contains(e));
            if removed_some {
                // Folders that are no longer excluded need a fresh scan.
                self.rebuild();
                return;
            }
            let excl = self.excl.read().clone();
            let mut ix = self.index.write();
            let victims: Vec<u32> = (0..ix.len() as u32)
                .filter(|&id| ix.entry(id).is_some_and(|e| e.is_searchable()) && excl.excludes_name(ix.name(id)))
                .collect();
            for id in victims {
                ix.remove(id);
            }
            ix.sweep_orphans();
        }
        if old.drives != new.drives && self.indexing_started.load(Ordering::Relaxed) {
            self.begin_indexing_delta();
        }
    }

    /// Adds newly selected drives and drops deselected ones.
    fn begin_indexing_delta(self: &Arc<Self>) {
        let selected = self.selected_drives();
        let wanted: HashSet<char> = selected.iter().map(|d| d.letter).collect();
        let present: Vec<char> = self.vols.lock().keys().copied().collect();
        for l in present.iter().filter(|l| !wanted.contains(l)) {
            self.drop_volume(*l);
        }
        let fast_mode = self.settings.read().fast_mode;
        let mut new_fast = Vec::new();
        for d in selected.iter().filter(|d| !present.contains(&d.letter)) {
            if fast_mode && d.kind == DriveKind::Fixed && d.is_ntfs() {
                new_fast.push(d.letter);
            } else {
                self.scan_volume(d.letter);
            }
        }
        if !new_fast.is_empty() {
            let mut all: Vec<char> = self.vols.lock().iter().filter(|(_, v)| v.fast).map(|(l, _)| *l).collect();
            all.extend(new_fast);
            all.sort_unstable();
            all.dedup();
            self.start_fast(all, false);
        }
    }

    fn drop_volume(self: &Arc<Self>, letter: char) {
        self.stop_watcher(letter);
        self.vols.lock().remove(&letter);
        let mut ix = self.index.write();
        if let Some(slot) = ix.volume_slot(&label(letter)) {
            ix.remove_volume(slot);
        }
    }

    /// Throws the index away and indexes everything again.
    pub fn rebuild(self: &Arc<Self>) {
        let me = self.clone();
        spawn("findit-rebuild", move || {
            log::info!(target: "findit::engine", "rebuilding the index");
            me.stop_fast();
            let letters: Vec<char> = me.watchers.lock().keys().copied().collect();
            for l in letters {
                me.stop_watcher(l);
            }
            me.cancel_all_scans();
            let t = Instant::now();
            while !me.scanning.lock().is_empty() && t.elapsed() < Duration::from_secs(30) {
                std::thread::sleep(Duration::from_millis(50));
            }
            me.ctl.cancel.store(false, Ordering::Relaxed);
            me.rescan_wanted.lock().clear();
            me.vols.lock().clear();
            {
                let mut ix = me.index.write();
                let mut fresh = Index::new();
                fresh.advance_epoch_past(ix.epoch());
                *ix = fresh;
            }
            *me.message.lock() = None;
            let _ = std::fs::remove_file(&me.paths.snapshot);
            me.begin_indexing(false);
        });
    }

    pub fn set_paused(&self, paused: bool) {
        self.ctl.paused.store(paused, Ordering::Relaxed);
        for c in self.scan_ctl.lock().values() {
            c.paused.store(paused, Ordering::Relaxed);
        }
        log::info!(target: "findit::engine", "indexing {}", if paused { "paused" } else { "resumed" });
    }

    pub fn is_paused(&self) -> bool {
        self.ctl.paused.load(Ordering::Relaxed)
    }

    // ----- status, notification, persistence ---------------------------------------

    pub fn status(&self) -> Status {
        let (total, generation, epoch) = match self.index.try_read_for(Duration::from_millis(20)) {
            Some(ix) => (ix.live_count().saturating_sub(ix.volumes().iter().filter(|v| !v.removed).count()), ix.generation(), ix.epoch()),
            None => (0, 0, 0),
        };
        let vols = self.vols.lock();
        let volumes: Vec<VolumeStatus> = vols
            .iter()
            .map(|(l, v)| VolumeStatus {
                label: label(*l),
                fast: v.fast,
                phase: v.phase,
                count: if v.fast { v.fast_count } else { v.stats.entries.load(Ordering::Relaxed) },
                denied: v.stats.denied.load(Ordering::Relaxed),
                message: v.message.clone(),
            })
            .collect();
        let indexing = volumes.iter().any(|v| matches!(v.phase, Phase::Scanning | Phase::Enumerating | Phase::Filling | Phase::Waiting))
            || (!self.indexing_started.load(Ordering::Relaxed) && self.settings.read().first_run_done);
        Status {
            loading: self.loading.load(Ordering::Relaxed),
            indexing,
            paused: self.is_paused(),
            total,
            denied: volumes.iter().map(|v| v.denied).sum(),
            volumes,
            fast_mode: self.fast_state.lock().clone(),
            message: self.message.lock().clone(),
            generation,
            epoch,
        }
    }

    fn notifier(self: Arc<Self>) {
        let mut last_gen = 0u64;
        let mut last_emit = Instant::now();
        let mut last_json = String::new();
        while !self.stopping() {
            std::thread::sleep(NOTIFY_EVERY);
            let status = self.status();
            if status.generation == 0 && !status.loading {
                continue; // index busy; try again next tick
            }
            let changed = status.generation != last_gen;
            let json = serde_json::to_string(&(
                &status.volumes,
                status.loading,
                status.indexing,
                status.paused,
                &status.fast_mode,
                &status.message,
            ))
            .unwrap_or_default();
            let periodic = status.indexing && last_emit.elapsed() > Duration::from_millis(500);
            if changed || json != last_json || periodic {
                last_gen = status.generation;
                last_json = json;
                last_emit = Instant::now();
                let _ = self.app.emit("index-status", IndexEvent { changed, status });
            }
        }
    }

    fn saver(self: Arc<Self>) {
        let mut last = Instant::now();
        while !self.stopping() {
            std::thread::sleep(Duration::from_secs(5));
            if last.elapsed() >= SAVE_EVERY && !self.loading.load(Ordering::Relaxed) {
                last = Instant::now();
                if let Err(e) = self.save_snapshot() {
                    log::warn!(target: "findit::engine", "snapshot save failed: {e}");
                }
            }
        }
    }

    /// Writes the snapshot if the index changed since the last save.
    pub fn save_snapshot(&self) -> std::io::Result<()> {
        if self.loading.load(Ordering::Relaxed) {
            return Ok(());
        }
        let ix = self.index.read();
        let gen = ix.generation();
        if gen == self.saved_gen.load(Ordering::Relaxed) {
            return Ok(());
        }
        let t = Instant::now();
        let size = snapshot::save(&ix, &self.paths.snapshot)?;
        self.saved_gen.store(gen, Ordering::Relaxed);
        log::info!(target: "findit::engine", "snapshot saved: {} entries, {} MB in {:?}", ix.live_count(), size >> 20, t.elapsed());
        Ok(())
    }

    fn drive_poller(self: Arc<Self>) {
        let mut last = drives::drive_mask();
        while !self.stopping() {
            std::thread::sleep(DRIVE_POLL);
            if !self.indexing_started.load(Ordering::Relaxed) {
                last = drives::drive_mask();
                continue;
            }
            let mask = drives::drive_mask();
            if mask != last {
                let gone: Vec<char> =
                    (0..26u8).filter(|i| last & (1 << i) != 0 && mask & (1 << i) == 0).map(|i| char::from(b'A' + i)).collect();
                last = mask;
                for l in gone {
                    if self.vols.lock().contains_key(&l) {
                        log::info!(target: "findit::engine", "drive {l}: removed");
                        self.drop_volume(l);
                    }
                }
                self.begin_indexing_delta();
            }
            // Restart watchers that stopped (for example after resume from sleep).
            let standard_ready: Vec<char> =
                self.vols.lock().iter().filter(|(_, v)| !v.fast && v.phase == Phase::Ready).map(|(l, _)| *l).collect();
            for l in standard_ready {
                if !self.watchers.lock().contains_key(&l) && drives::drive_info(l).is_some() {
                    log::info!(target: "findit::engine", "drive {l}: watcher restarted, rescanning");
                    self.scan_volume(l);
                }
            }
        }
    }

    /// Stops everything and saves the snapshot. Called once on exit.
    pub fn shutdown(&self) {
        if self.shutting_down.swap(true, Ordering::Relaxed) {
            return;
        }
        self.stop_fast();
        self.cancel_all_scans();
        let letters: Vec<char> = self.watchers.lock().keys().copied().collect();
        for l in letters {
            self.stop_watcher(l);
        }
        let t = Instant::now();
        while !self.scanning.lock().is_empty() && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        if let Err(e) = self.save_snapshot() {
            log::warn!(target: "findit::engine", "snapshot save on exit failed: {e}");
        }
        log::logger().flush();
    }
}

fn spawn<F: FnOnce() + Send + 'static>(name: &str, f: F) {
    if let Err(e) = std::thread::Builder::new().name(name.into()).spawn(f) {
        log::error!(target: "findit::engine", "cannot start thread {name}: {e}");
    }
}
