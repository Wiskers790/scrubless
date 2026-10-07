//! Background indexer: scans folders, then turns each pending file into searchable items.

use anyhow::Result;
use crossbeam_channel::{Receiver, Sender};
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use crate::media::{self, Kind};
use crate::runtime::{Runtime, RuntimeState};
use crate::speech::Speech;
use crate::store::{NewItem, Store};

const AUDIO_WIN: f64 = 10.0;
const SILENCE_DB: f32 = -55.0;
const WORKERS: usize = 4; // matches llama-server --parallel

#[derive(Clone, Serialize, Default)]
pub struct Progress {
    pub scanning: Option<String>,
    pub current: Option<String>,
    pub current_done: usize,
    pub current_total: usize,
    pub files_total: i64,
    pub files_done: i64,
    pub files_pending: i64,
    pub files_failed: i64,
    pub items: usize,
    pub paused: bool,
    pub transcribing: Option<String>,
    pub speech_done: i64,
    pub speech_pending: i64,
}

pub enum Msg {
    Scan(i64, String),
}

pub struct Indexer {
    store: Arc<Store>,
    rt: Arc<Runtime>,
    pub speech: Arc<Speech>,
    pub transcribe: AtomicBool,
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
    dirty: Mutex<HashSet<i64>>,
    thumbs: PathBuf,
    tmp: PathBuf,
    pub progress: Mutex<Progress>,
    pub paused: AtomicBool,
    tx: Sender<Msg>,
}

impl Indexer {
    pub fn start(store: Arc<Store>, rt: Arc<Runtime>, speech: Arc<Speech>, data: &Path) -> Arc<Self> {
        let (tx, rx) = crossbeam_channel::unbounded();
        let ix = Arc::new(Self {
            store,
            rt,
            speech,
            transcribe: AtomicBool::new(true),
            watcher: Mutex::new(None),
            dirty: Mutex::new(HashSet::new()),
            thumbs: data.join("thumbs"),
            tmp: data.join("tmp"),
            progress: Mutex::new(Progress::default()),
            paused: AtomicBool::new(false),
            tx,
        });
        let _ = std::fs::create_dir_all(&ix.thumbs);
        let _ = std::fs::remove_dir_all(&ix.tmp);
        let w = ix.clone();
        std::thread::Builder::new().name("indexer".into()).spawn(move || w.run(rx)).unwrap();
        // pick up changes made while the app was closed, then watch for new ones
        for (id, path) in ix.store.folders().unwrap_or_default() {
            ix.send(Msg::Scan(id, path));
        }
        ix.rewatch();
        let w = ix.clone();
        std::thread::Builder::new().name("watch".into()).spawn(move || w.watch_loop()).unwrap();
        ix
    }

    /// (Re)create the filesystem watcher for the current folder list.
    pub fn rewatch(self: &Arc<Self>) {
        use notify::Watcher;
        let folders = self.store.folders().unwrap_or_default();
        let me = Arc::downgrade(self);
        let roots = folders.clone();
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let (Some(ix), Ok(ev)) = (me.upgrade(), res) else { return };
            if matches!(ev.kind, notify::EventKind::Access(_)) {
                return;
            }
            for p in &ev.paths {
                if let Some((id, _)) = roots.iter().find(|(_, r)| p.starts_with(r)) {
                    ix.dirty.lock().insert(*id);
                }
            }
        });
        match watcher {
            Ok(mut w) => {
                for (_, f) in &folders {
                    if let Err(e) = w.watch(Path::new(f), notify::RecursiveMode::Recursive) {
                        eprintln!("watch {f}: {e}");
                    }
                }
                *self.watcher.lock() = Some(w);
            }
            Err(e) => eprintln!("watcher: {e}"),
        }
    }

    /// Debounce filesystem events into folder rescans; also rescan everything every 15 minutes in
    /// case the watcher missed something (network drives, sleep).
    fn watch_loop(&self) {
        let mut ticks = 0u32;
        loop {
            std::thread::sleep(Duration::from_secs(5));
            ticks += 1;
            let dirty: Vec<i64> = self.dirty.lock().drain().collect();
            let folders = self.store.folders().unwrap_or_default();
            for (id, path) in &folders {
                if dirty.contains(id) || ticks % 180 == 0 {
                    self.send(Msg::Scan(*id, path.clone()));
                }
            }
        }
    }

    pub fn send(&self, m: Msg) {
        let _ = self.tx.send(m);
    }

    pub fn snapshot(&self) -> Progress {
        let mut p = self.progress.lock().clone();
        if let Ok((t, d, pe, f)) = self.store.counts() {
            p.files_total = t;
            p.files_done = d;
            p.files_pending = pe;
            p.files_failed = f;
        }
        p.items = self.store.item_count();
        p.paused = self.paused.load(Ordering::Relaxed);
        if let Ok((d, pe)) = self.store.speech_counts() {
            p.speech_done = d;
            p.speech_pending = if self.transcribe.load(Ordering::Relaxed) { pe } else { 0 };
        }
        p
    }

    fn run(self: Arc<Self>, rx: Receiver<Msg>) {
        loop {
            // drain control messages first: scans are cheap and tell us what is pending
            while let Ok(m) = rx.try_recv() {
                let Msg::Scan(id, path) = m;
                self.progress.lock().scanning = Some(path.clone());
                if let Err(e) = self.scan(id, &path) {
                    eprintln!("scan {path}: {e:#}");
                }
                self.progress.lock().scanning = None;
            }
            if !self.rt.ready() || self.paused.load(Ordering::Relaxed) {
                if let Ok(Msg::Scan(id, path)) = rx.recv_timeout(Duration::from_millis(500)) {
                    let _ = self.tx.send(Msg::Scan(id, path));
                }
                continue;
            }
            match self.store.next_pending() {
                Ok(Some((id, path, kind))) => {
                    {
                        let mut p = self.progress.lock();
                        p.current = Some(path.clone());
                        p.current_done = 0;
                        p.current_total = 0;
                    }
                    let res = self.process(id, Path::new(&path), &kind);
                    if let Err(e) = res {
                        if !self.rt.ready() {
                            // the engine went away mid-file: keep it queued, the supervisor restarts it
                            std::thread::sleep(Duration::from_secs(2));
                        } else {
                            eprintln!("index {path}: {e:#}");
                            let _ = self.store.fail_file(id, &format!("{e:#}"));
                        }
                    }
                    self.progress.lock().current = None;
                }
                Ok(None) => {
                    if self.transcribe.load(Ordering::Relaxed) && self.speech_step() {
                        continue;
                    }
                    if let Ok(Msg::Scan(id, path)) = rx.recv_timeout(Duration::from_secs(2)) {
                        let _ = self.tx.send(Msg::Scan(id, path));
                    }
                }
                Err(e) => {
                    eprintln!("db: {e:#}");
                    std::thread::sleep(Duration::from_secs(2));
                }
            }
        }
    }

    /// Transcribe one pending file. Returns false when there is nothing (doable) to transcribe.
    fn speech_step(&self) -> bool {
        let Ok(Some((id, path))) = self.store.next_speech() else { return false };
        let gpu = matches!(&*self.rt.state.lock(), RuntimeState::Ready { device } if device == "gpu");
        if let Err(e) = self.speech.ensure(gpu) {
            eprintln!("speech engine: {e:#}");
            return false; // unavailable: leave files pending, visual search still works
        }
        self.progress.lock().transcribing = Some(path.clone());
        let res = (|| -> Result<()> {
            let pcm = media::audio_pcm(Path::new(&path))?;
            let tr = self.speech.transcribe(&media::wav(&pcm))?;
            self.progress.lock().current_total = tr.segments.len();
            let vecs = self.parallel(&tr.segments, |s| self.rt.embed_text(&format!("title: none | text: {}", s.text)))?;
            let items = tr.segments.iter().zip(vecs)
                .map(|(s, vec)| NewItem { kind: "speech", t0: s.t0, t1: s.t1, thumb: None, text: Some(s.text.clone()), vec })
                .collect();
            self.store.finish_speech(id, &tr.language, items)
        })();
        if let Err(e) = res {
            eprintln!("transcribe {path}: {e:#}");
            let _ = self.store.fail_speech(id);
        }
        self.progress.lock().transcribing = None;
        true
    }

    fn scan(&self, folder_id: i64, root: &str) -> Result<()> {
        let mut seen = HashSet::new();
        for entry in walkdir::WalkDir::new(root).follow_links(true).into_iter().filter_entry(|e| {
            // skip hidden dirs and editor caches
            let n = e.file_name().to_string_lossy();
            e.depth() == 0 || !(n.starts_with('.') || n == "node_modules" || n.ends_with(".fcpbundle") || n == "CacheClip")
        }) {
            let Ok(e) = entry else { continue };
            if !e.file_type().is_file() {
                continue;
            }
            let Some(kind) = Kind::of(e.path()) else { continue };
            let Ok(md) = e.metadata() else { continue };
            if md.len() < 2048 {
                continue;
            }
            let mtime = md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0);
            let p = e.path().to_string_lossy().to_string();
            self.store.upsert_file(folder_id, &p, kind.as_str(), md.len() as i64, mtime)?;
            seen.insert(p);
        }
        if self.store.prune_missing(folder_id, &seen)? > 0 {
            self.store.reload()?;
        }
        Ok(())
    }

    /// Run `f` over `jobs` on WORKERS threads, keeping order and updating progress.
    fn parallel<T: Sync, R: Send>(&self, jobs: &[T], f: impl Fn(&T) -> Result<R> + Sync) -> Result<Vec<R>> {
        let next = AtomicUsize::new(0);
        let results: Mutex<Vec<Option<Result<R>>>> = Mutex::new((0..jobs.len()).map(|_| None).collect());
        std::thread::scope(|s| {
            for _ in 0..WORKERS.min(jobs.len()) {
                s.spawn(|| loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= jobs.len() {
                        break;
                    }
                    let r = f(&jobs[i]);
                    results.lock()[i] = Some(r);
                    self.progress.lock().current_done += 1;
                });
            }
        });
        results.into_inner().into_iter().map(|r| r.unwrap()).collect()
    }

    fn sound_items(&self, path: &Path) -> Result<Vec<NewItem>> {
        let pcm = media::audio_pcm(path)?;
        let win = (AUDIO_WIN * media::SR as f64) as usize;
        let windows: Vec<(f64, &[i16])> = pcm
            .chunks(win)
            .enumerate()
            .filter(|(_, c)| c.len() >= media::SR as usize && media::rms_db(c) > SILENCE_DB)
            .map(|(i, c)| (i as f64 * AUDIO_WIN, c))
            .collect();
        self.progress.lock().current_total += windows.len();
        let vecs = self.parallel(&windows, |(_, c)| self.rt.embed_audio(&media::wav(c)))?;
        Ok(windows
            .iter()
            .zip(vecs)
            .map(|((t, c), vec)| NewItem { kind: "audio", t0: *t, t1: t + c.len() as f64 / media::SR as f64, thumb: None, text: None, vec })
            .collect())
    }

    fn process(&self, file_id: i64, path: &Path, kind: &str) -> Result<()> {
        let thumb_dir = self.thumbs.join(file_id.to_string());
        let _ = std::fs::remove_dir_all(&thumb_dir);
        std::fs::create_dir_all(&thumb_dir)?;
        let mut items = Vec::new();
        let mut duration = 0.0;
        let mut meta = serde_json::json!({});
        let mut has_audio = false;
        match kind {
            "image" => {
                self.progress.lock().current_total = 1;
                meta = media::probe(path).map(|p| p.meta).unwrap_or_default();
                let jpeg = media::image_jpeg(path, 448)?;
                let thumb = thumb_dir.join("0.jpg");
                std::fs::write(&thumb, &jpeg)?;
                let vec = self.rt.embed_image(&jpeg)?;
                items.push(NewItem { kind: "image", t0: 0.0, t1: 0.0, thumb: Some(thumb), text: None, vec });
            }
            "audio" => {
                let pr = media::probe(path)?;
                duration = pr.duration;
                meta = pr.meta;
                has_audio = pr.has_audio;
                items = self.sound_items(path)?;
            }
            _ => {
                let pr = media::probe(path)?;
                duration = pr.duration;
                meta = pr.meta.clone();
                if !pr.has_video {
                    // e.g. a .wmv/.mp4 that only carries a soundtrack
                    items = self.sound_items(path)?;
                    return self.store.finish_file(file_id, &path.to_string_lossy(), duration, meta, pr.has_audio, items);
                }
                let tmp = self.tmp.join(file_id.to_string());
                let _ = std::fs::remove_dir_all(&tmp);
                std::fs::create_dir_all(&tmp)?;
                let shots = media::shots(path, &tmp)?;
                self.progress.lock().current_total += shots.len();
                let mut frames = Vec::with_capacity(shots.len());
                for (n, (t, f)) in shots.iter().enumerate() {
                    let thumb = thumb_dir.join(format!("{n}.jpg"));
                    std::fs::rename(f, &thumb).or_else(|_| std::fs::copy(f, &thumb).map(|_| ()))?;
                    let t1 = shots.get(n + 1).map(|x| x.0).unwrap_or(duration).max(*t);
                    frames.push((*t, t1, thumb));
                }
                let _ = std::fs::remove_dir_all(&tmp);
                let vecs = self.parallel(&frames, |(_, _, thumb)| self.rt.embed_image(&std::fs::read(thumb)?))?;
                for ((t0, t1, thumb), vec) in frames.into_iter().zip(vecs) {
                    items.push(NewItem { kind: "frame", t0, t1, thumb: Some(thumb), text: None, vec });
                }
                if pr.has_audio {
                    has_audio = true;
                    items.extend(self.sound_items(path)?);
                }
            }
        }
        self.store.finish_file(file_id, &path.to_string_lossy(), duration, meta, has_audio, items)
    }
}
