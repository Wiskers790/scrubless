#[cfg(debug_assertions)]
mod dev_bridge;
mod export;
mod index;
mod media;
mod runtime;
mod speech;
mod store;

use index::{Indexer, Msg, Progress};
use media::Kind;
use runtime::{Runtime, RuntimeState};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use speech::{Speech, SpeechState};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;
use store::{FileInfo, ItemMeta, Store};
use tauri::{Manager, State};

struct App {
    rt: Arc<Runtime>,
    store: Arc<Store>,
    ix: Arc<Indexer>,
    clips: PathBuf,
}

type R<T> = Result<T, String>;
fn e<E: std::fmt::Display>(err: E) -> String {
    format!("{err:#}")
}

// ---------------------------------------------------------------- status & library

#[derive(Serialize)]
struct Status {
    runtime: RuntimeState,
    speech: SpeechState,
    index: Progress,
    folders: Vec<String>,
    transcribe: bool,
}

#[tauri::command]
fn status(app: State<App>) -> R<Status> {
    Ok(Status {
        runtime: app.rt.state.lock().clone(),
        speech: app.ix.speech.state.lock().clone(),
        index: app.ix.snapshot(),
        folders: app.store.folders().map_err(e)?.into_iter().map(|f| f.1).collect(),
        transcribe: app.ix.transcribe.load(Ordering::Relaxed),
    })
}

#[tauri::command]
fn add_folder(app: State<App>, path: String) -> R<()> {
    let id = app.store.add_folder(&path).map_err(e)?;
    app.ix.send(Msg::Scan(id, path));
    app.ix.rewatch();
    Ok(())
}

#[tauri::command]
fn remove_folder(app: State<App>, path: String) -> R<()> {
    app.store.remove_folder(&path).map_err(e)?;
    app.ix.rewatch();
    Ok(())
}

#[tauri::command]
fn rescan(app: State<App>) -> R<()> {
    for (id, p) in app.store.folders().map_err(e)? {
        app.ix.send(Msg::Scan(id, p));
    }
    Ok(())
}

#[tauri::command]
fn is_dir(path: String) -> bool {
    Path::new(&path).is_dir()
}

#[tauri::command]
fn set_paused(app: State<App>, paused: bool) {
    app.ix.paused.store(paused, Ordering::Relaxed);
}

#[tauri::command]
fn set_transcribe(app: State<App>, on: bool) -> R<()> {
    let mut s = load_settings(&app.store);
    s.transcribe = on;
    apply_settings(&app, &s, true)
}

// ---------------------------------------------------------------- settings

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
struct Settings {
    transcribe: bool,
    /// "auto" or a language code
    speech_language: String,
    /// "auto" | "accurate" | "fast"
    speech_quality: String,
    /// "auto" | "cpu"
    device: String,
    /// preview/export clip cache cap
    cache_gb: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Self { transcribe: true, speech_language: "auto".into(), speech_quality: "auto".into(), device: "auto".into(), cache_gb: 3.0 }
    }
}

fn load_settings(store: &Store) -> Settings {
    store.setting("settings").and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

/// Persist and apply. Engines restart only when something they depend on changed.
fn apply_settings(app: &App, s: &Settings, persist: bool) -> R<()> {
    let old = load_settings(&app.store);
    if persist {
        app.store.set_setting("settings", &serde_json::to_string(s).map_err(e)?).map_err(e)?;
    }
    app.ix.transcribe.store(s.transcribe, Ordering::Relaxed);
    *app.ix.speech.language.lock() = s.speech_language.clone();
    *app.ix.speech.quality.lock() = s.speech_quality.clone();
    let cpu = s.device == "cpu";
    if app.rt.force_cpu.swap(cpu, Ordering::Relaxed) != cpu {
        app.rt.stop(); // the supervisor restarts it on the new device
    }
    let speech_changed = old.speech_language != s.speech_language || old.speech_quality != s.speech_quality || old.device != s.device;
    if !s.transcribe || speech_changed {
        app.ix.speech.stop(); // restarted on demand with the new options
        *app.ix.speech.state.lock() = SpeechState::Idle;
    }
    Ok(())
}

#[derive(Serialize)]
struct SettingsView {
    settings: Settings,
    data_dir: String,
    cache_bytes: u64,
}

#[tauri::command]
fn get_settings(app: State<App>) -> SettingsView {
    SettingsView {
        settings: load_settings(&app.store),
        data_dir: app.clips.parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default(),
        cache_bytes: dir_size(&app.clips),
    }
}

#[tauri::command]
fn set_settings(app: State<App>, settings: Settings) -> R<()> {
    apply_settings(&app, &settings, true)
}

#[tauri::command]
fn retranscribe_all(app: State<App>) -> R<usize> {
    app.store.retranscribe_all().map_err(e)
}

#[tauri::command]
fn clear_cache(app: State<App>) -> R<()> {
    prune_cache(&app.clips, 0);
    Ok(())
}

fn dir_size(dir: &Path) -> u64 {
    std::fs::read_dir(dir).into_iter().flatten().flatten().filter_map(|e| e.metadata().ok()).filter(|m| m.is_file()).map(|m| m.len()).sum()
}

/// Delete the least recently used preview/export clips until the cache fits in `max_bytes`.
fn prune_cache(dir: &Path, max_bytes: u64) {
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            m.is_file().then(|| (m.accessed().or_else(|_| m.modified()).unwrap_or(std::time::UNIX_EPOCH), m.len(), e.path()))
        })
        .collect();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    files.sort_by_key(|f| f.0);
    for (_, len, path) in files {
        if total <= max_bytes {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total -= len;
        }
    }
}

// ---------------------------------------------------------------- search

/// Optional narrowing of a search.
#[derive(Deserialize, Default, Clone)]
#[serde(default)]
struct Filters {
    /// "video" | "image" | "audio" (file kinds); empty = all
    kinds: Vec<String>,
    /// only files under these folders; empty = all
    folders: Vec<String>,
    /// file modified at/after / before (unix seconds)
    since: Option<i64>,
    until: Option<i64>,
}

impl Filters {
    fn allows(&self, f: &FileInfo) -> bool {
        (self.kinds.is_empty() || self.kinds.iter().any(|k| *k == f.kind))
            && (self.folders.is_empty() || self.folders.iter().any(|d| Path::new(&f.path).starts_with(d)))
            && self.since.map_or(true, |s| f.mtime >= s)
            && self.until.map_or(true, |u| f.mtime < u)
    }
}

#[derive(Serialize, Clone)]
struct Hit {
    item_id: i64,
    file_id: i64,
    path: String,
    name: String,
    file_kind: String,
    t0: f64,
    t1: f64,
    score: f32,
    /// standout vs. the rest of the library for this query (see Rule)
    z: f32,
    strong: bool,
    /// exact words found in the transcript
    exact: bool,
    thumb: Option<String>,
    /// transcript line, for speech hits
    text: Option<String>,
    /// source timecode at t0 (camera TC if present)
    tc: String,
    /// "1920×1080 · 29.97 fps · 2:41"-style summary
    info: String,
}

#[derive(Serialize)]
struct Results {
    moments: Vec<Hit>,
    said: Vec<Hit>,
    sounds: Vec<Hit>,
    photos: Vec<Hit>,
    took_ms: u128,
}

/// When is a hit a real match? Tuned on hand-labelled data (ground.py at the repo root):
/// - `gate`: the best visual score for 11 concepts present in an 861-shot library was 0.716-0.787,
///   for 12 absent concepts 0.637-0.695. Below the gate nothing is a confident match.
/// - `z`: real matches stood out from the library's score distribution by >= 2.1 sigma,
/// - `gap`: and stayed within ~0.06 of the best score.
/// Sound: query-by-example is reliable (ESC-50 P@10 0.65); typed text -> sound has no usable
/// presence signal, so typed queries never mark a sound as a confident match.
struct Rule {
    gate: f32,
    z: f32,
    gap: f32,
}
const VISUAL: Rule = Rule { gate: 0.70, z: 2.0, gap: 0.07 };
const SOUND_BY_EXAMPLE: Rule = Rule { gate: 0.0, z: 3.0, gap: 0.04 };
const SOUND_BY_TEXT: Rule = Rule { gate: f32::INFINITY, z: 3.0, gap: 0.04 };
/// Meaning-based transcript search (calibrated in the speech test, see eval/).
const SAID: Rule = Rule { gate: 0.0, z: 3.0, gap: 0.10 };

fn info_of(f: &FileInfo) -> String {
    let m = &f.meta;
    let mut parts = vec![];
    if let (Some(w), Some(h)) = (m["width"].as_u64(), m["height"].as_u64()) {
        parts.push(format!("{w}×{h}"));
    }
    if let (Some(n), Some(d)) = (m["fps_num"].as_u64(), m["fps_den"].as_u64()) {
        if f.kind == "video" && d > 0 {
            let fps = n as f64 / d as f64;
            parts.push(if (fps - fps.round()).abs() < 0.01 { format!("{fps:.0} fps") } else { format!("{fps:.2} fps") });
        }
    }
    if f.duration > 0.0 {
        parts.push(timecode_short(f.duration));
    }
    parts.join(" · ")
}

fn timecode_short(t: f64) -> String {
    let s = t.max(0.0) as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

fn to_hit(app: &App, m: &ItemMeta, score: f32, z: f32, strong: bool, exact: bool) -> Option<Hit> {
    let files = app.store.files.read();
    let f = files.get(&m.file_id)?;
    let thumb = m.thumb.clone().or_else(|| if f.kind == "video" { app.store.nearest_thumb(m.file_id, m.t0) } else { None });
    Some(Hit {
        item_id: m.id,
        file_id: m.file_id,
        name: Path::new(&f.path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
        file_kind: f.kind.clone(),
        path: f.path.clone(),
        t0: m.t0,
        t1: m.t1,
        score,
        z,
        strong,
        exact,
        thumb,
        text: m.text.clone(),
        tc: if f.kind == "image" { String::new() } else { media::source_tc(&f.meta, m.t0) },
        info: info_of(f),
    })
}

/// Keep the best hit per neighbourhood (one per file and `gap`-second window), apply filters and
/// mark which hits are confident matches.
fn rank(app: &App, hits: Vec<(f32, f32, ItemMeta)>, gap: f64, limit: usize, rule: &Rule, filters: &Filters) -> Vec<Hit> {
    let hits: Vec<_> = {
        let files = app.store.files.read();
        hits.into_iter().filter(|(_, _, m)| files.get(&m.file_id).map_or(false, |f| filters.allows(f))).collect()
    };
    let best = hits.first().map(|h| h.0).unwrap_or(0.0);
    let confident = best >= rule.gate;
    let mut out: Vec<Hit> = Vec::new();
    for (score, z, m) in hits {
        if out.iter().any(|h| h.file_id == m.file_id && (h.t0 - m.t0).abs() < gap) {
            continue;
        }
        let strong = confident && z >= rule.z && score >= best - rule.gap;
        if let Some(h) = to_hit(app, &m, score, z, strong, false) {
            out.push(h);
        }
        if out.len() >= limit {
            break;
        }
    }
    out
}

/// What was said: exact words first (always confident), then lines that mean the same thing.
fn said(app: &App, query: Option<&str>, q: &[f32], filters: &Filters) -> Vec<Hit> {
    let mut out: Vec<Hit> = Vec::new();
    if let Some(text) = query {
        let files = app.store.files.read().clone();
        for id in app.store.keyword(text, 200).unwrap_or_default() {
            let Some(m) = app.store.item(id) else { continue };
            if !files.get(&m.file_id).map_or(false, |f| filters.allows(f)) {
                continue;
            }
            if let Some(h) = to_hit(app, &m, 1.0, 99.0, true, true) {
                out.push(h);
            }
            if out.len() >= 40 {
                break;
            }
        }
    }
    let exact: HashSet<i64> = out.iter().map(|h| h.item_id).collect();
    for h in rank(app, app.store.top(q, "speech", 300), 4.0, 60, &SAID, filters) {
        if !exact.contains(&h.item_id) && !out.iter().any(|x| x.file_id == h.file_id && (x.t0 - h.t0).abs() < 4.0) {
            out.push(h);
        }
    }
    out
}

fn search_vec(app: &App, q: &[f32], text: Option<&str>, filters: &Filters, started: Instant) -> Results {
    let typed = text.is_some();
    Results {
        moments: rank(app, app.store.top(q, "frame", 400), 3.0, 60, &VISUAL, filters),
        said: if typed { said(app, text, q, filters) } else { vec![] },
        sounds: rank(app, app.store.top(q, "audio", 200), 15.0, 30, if typed { &SOUND_BY_TEXT } else { &SOUND_BY_EXAMPLE }, filters),
        photos: rank(app, app.store.top(q, "image", 80), 0.0, 60, &VISUAL, filters),
        took_ms: started.elapsed().as_millis(),
    }
}

#[tauri::command]
async fn search(app: State<'_, App>, query: String, filters: Option<Filters>) -> R<Results> {
    let t = Instant::now();
    if !app.rt.ready() {
        return Err("The search engine is still starting".into());
    }
    let q = app.rt.embed_query(query.trim()).map_err(e)?;
    Ok(search_vec(&app, &q, Some(query.trim()), &filters.unwrap_or_default(), t))
}

/// Search with a file as the query: a photo, a sound, or a frame from a video.
#[tauri::command]
async fn search_by_file(app: State<'_, App>, path: String, filters: Option<Filters>) -> R<Results> {
    let t = Instant::now();
    let p = Path::new(&path);
    let q = match Kind::of(p) {
        Some(Kind::Image) => app.rt.embed_image(&media::image_jpeg(p, 448).map_err(e)?),
        Some(Kind::Audio) => {
            let pcm = media::audio_pcm(p).map_err(e)?;
            let n = (10 * media::SR) as usize;
            // the loudest 10s window represents the sound best
            let best = pcm.chunks(n).max_by(|a, b| media::rms_db(a).total_cmp(&media::rms_db(b))).unwrap_or(&[]);
            app.rt.embed_audio(&media::wav(best))
        }
        Some(Kind::Video) => {
            let d = media::probe(p).map_err(e)?.duration;
            app.rt.embed_image(&media::frame_at(p, d / 2.0, 448).map_err(e)?)
        }
        None => return Err("Drop a photo, video or sound file".into()),
    }
    .map_err(e)?;
    Ok(search_vec(&app, &q, None, &filters.unwrap_or_default(), t))
}

/// "More like this": search with a result's own vector.
#[tauri::command]
async fn search_similar(app: State<'_, App>, item_id: i64, filters: Option<Filters>) -> R<Results> {
    let t = Instant::now();
    let (_, q) = app.store.vector(item_id).ok_or("item not found")?;
    Ok(search_vec(&app, &q, None, &filters.unwrap_or_default(), t))
}

#[derive(Serialize)]
struct Line {
    t0: f64,
    t1: f64,
    tc: String,
    text: String,
}

/// Full transcript of a file (detail panel).
#[tauri::command]
fn transcript(app: State<App>, file_id: i64) -> Vec<Line> {
    let meta = app.store.files.read().get(&file_id).map(|f| f.meta.clone()).unwrap_or_default();
    app.store
        .transcript(file_id)
        .into_iter()
        .map(|m| Line { t0: m.t0, t1: m.t1, tc: media::source_tc(&meta, m.t0), text: m.text.unwrap_or_default() })
        .collect()
}

#[tauri::command]
fn file_info(app: State<App>, file_id: i64) -> Option<FileInfo> {
    app.store.files.read().get(&file_id).cloned()
}

// ---------------------------------------------------------------- clips

fn clip_path(app: &App, path: &str, t0: f64, t1: f64, tag: &str, ext: &str) -> PathBuf {
    let h = Sha256::digest(format!("{path}|{t0:.2}|{t1:.2}|{tag}").as_bytes());
    app.clips.join(format!("{}.{ext}", &hex::encode(h)[..20]))
}

/// A short, browser-playable preview of a hit (H.264 mp4, or wav for audio files).
#[tauri::command]
async fn preview(app: State<'_, App>, path: String, t0: f64, t1: f64) -> R<String> {
    let audio_only = Kind::of(Path::new(&path)) == Some(Kind::Audio);
    let start = (t0 - 0.5).max(0.0);
    let dur = (t1 - start).clamp(3.0, 10.0);
    let out = clip_path(&app, &path, start, start + dur, "preview", if audio_only { "wav" } else { "mp4" });
    if !out.exists() {
        if audio_only {
            media::audio_clip(Path::new(&path), start, dur, &out).map_err(e)?;
        } else {
            media::clip(Path::new(&path), start, dur, &out, 480).map_err(e)?;
        }
    }
    Ok(out.to_string_lossy().to_string())
}

/// Full-quality clip of a hit, written to `dest` (or into the clip cache when empty).
#[tauri::command]
async fn export_clip(app: State<'_, App>, path: String, t0: f64, t1: f64, pad: f64, dest: Option<String>) -> R<String> {
    let audio_only = Kind::of(Path::new(&path)) == Some(Kind::Audio);
    let start = (t0 - pad).max(0.0);
    let dur = (t1 - t0 + 2.0 * pad).max(1.0);
    let out = match dest {
        Some(d) => PathBuf::from(d),
        None => {
            let stem = Path::new(&path).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            let ext = if audio_only { "wav" } else { "mp4" };
            app.clips.join(format!("{stem} @{:02}m{:02}s.{ext}", (start / 60.0) as u32, (start % 60.0) as u32))
        }
    };
    if !out.exists() {
        if audio_only {
            media::audio_clip(Path::new(&path), start, dur, &out).map_err(e)?;
        } else {
            media::clip(Path::new(&path), start, dur, &out, 2160).map_err(e)?;
        }
    }
    Ok(out.to_string_lossy().to_string())
}

// ---------------------------------------------------------------- selects (a bin of moments)

#[derive(Serialize)]
struct Select {
    id: i64,
    file_id: i64,
    path: String,
    name: String,
    file_kind: String,
    t0: f64,
    t1: f64,
    tc: String,
    thumb: Option<String>,
    note: Option<String>,
}

#[tauri::command]
fn add_select(app: State<App>, file_id: i64, t0: f64, t1: f64, note: Option<String>) -> R<i64> {
    app.store.add_select(file_id, t0, t1, note).map_err(e)
}

#[tauri::command]
fn remove_select(app: State<App>, id: i64) -> R<()> {
    app.store.remove_select(id).map_err(e)
}

#[tauri::command]
fn clear_selects(app: State<App>) -> R<()> {
    app.store.clear_selects().map_err(e)
}

#[tauri::command]
fn selects(app: State<App>) -> R<Vec<Select>> {
    let files = app.store.files.read();
    Ok(app
        .store
        .selects()
        .map_err(e)?
        .into_iter()
        .filter_map(|(id, file_id, t0, t1, note)| {
            let f = files.get(&file_id)?;
            Some(Select {
                id,
                file_id,
                path: f.path.clone(),
                name: Path::new(&f.path).file_name()?.to_string_lossy().to_string(),
                file_kind: f.kind.clone(),
                t0,
                t1,
                tc: if f.kind == "image" { String::new() } else { media::source_tc(&f.meta, t0) },
                thumb: app.store.nearest_thumb(file_id, t0),
                note,
            })
        })
        .collect())
}

/// Write the selects as a timeline (fcpxml | xml | edl | csv) or render them as clips ("clips":
/// `dest` is a folder). Returns the written path.
#[tauri::command]
async fn export_selects(app: State<'_, App>, format: String, dest: String, pad: f64) -> R<String> {
    let clips: Vec<export::Clip> = {
        let files = app.store.files.read();
        app.store
            .selects()
            .map_err(e)?
            .into_iter()
            .filter_map(|(_, file_id, t0, t1, note)| {
                let f = files.get(&file_id)?;
                Some(export::Clip {
                    path: f.path.clone(),
                    kind: f.kind.clone(),
                    meta: f.meta.clone(),
                    duration: f.duration,
                    t0: (t0 - pad).max(0.0),
                    t1: if f.duration > 0.0 { (t1 + pad).min(f.duration) } else { t1 + pad },
                    note,
                })
            })
            .collect()
    };
    if clips.is_empty() {
        return Err("No selects to export".into());
    }
    let title = "Scrubless Selects";
    let body = match format.as_str() {
        "fcpxml" => export::fcpxml(&clips, title),
        "xml" => export::xmeml(&clips, title),
        "edl" => export::edl(&clips, title),
        "csv" => export::csv(&clips),
        "clips" => {
            std::fs::create_dir_all(&dest).map_err(e)?;
            for (i, c) in clips.iter().enumerate() {
                let stem = Path::new(&c.path).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                if c.kind == "image" {
                    let name = Path::new(&c.path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    std::fs::copy(&c.path, Path::new(&dest).join(format!("{:02} {name}", i + 1))).map_err(e)?;
                    continue;
                }
                let audio = c.kind == "audio";
                let out = Path::new(&dest).join(format!(
                    "{:02} {stem} @{:02}m{:02}s.{}",
                    i + 1,
                    (c.t0 / 60.0) as u32,
                    (c.t0 % 60.0) as u32,
                    if audio { "wav" } else { "mp4" }
                ));
                if audio {
                    media::audio_clip(Path::new(&c.path), c.t0, c.t1 - c.t0, &out).map_err(e)?;
                } else {
                    media::clip(Path::new(&c.path), c.t0, c.t1 - c.t0, &out, 2160).map_err(e)?;
                }
            }
            return Ok(dest);
        }
        other => return Err(format!("unknown format {other}")),
    };
    std::fs::write(&dest, body).map_err(e)?;
    Ok(dest)
}

pub fn run() {
    tauri::Builder::default()
        // a second launch focuses the running window instead of indexing the same library twice
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_drag::init())
        .setup(|app| {
            if let Ok(res) = app.path().resource_dir() {
                let _ = runtime::RESOURCE_DIR.set(res);
            }
            let data = std::env::var("SCRUBLESS_DATA_DIR").map(PathBuf::from).unwrap_or(app.path().app_data_dir()?);
            std::fs::create_dir_all(&data)?;
            let clips = data.join("clips");
            std::fs::create_dir_all(&clips)?;
            let rt = Runtime::new(&data);
            let speech = Arc::new(Speech::new(&rt.model_dir, &data));
            let store = Arc::new(Store::open(&data.join("library.db"))?);
            let ix = Indexer::start(store.clone(), rt.clone(), speech, &data);
            let sup = rt.clone();
            std::thread::Builder::new().name("engine".into()).spawn(move || sup.supervise())?;
            let state = App { rt, store, ix, clips };
            let _ = apply_settings(&state, &load_settings(&state.store), false);
            let (cache_dir, store_ref) = (state.clips.clone(), state.store.clone());
            std::thread::Builder::new().name("cache".into()).spawn(move || loop {
                let cap = (load_settings(&store_ref).cache_gb.max(0.2) * 1e9) as u64;
                prune_cache(&cache_dir, cap);
                std::thread::sleep(std::time::Duration::from_secs(600));
            })?;
            app.manage(state);
            #[cfg(debug_assertions)]
            if let Some(port) = std::env::var("SCRUBLESS_DEV_BRIDGE").ok().and_then(|p| p.parse().ok()) {
                dev_bridge::start(app.handle().clone(), port);
            }
            Ok(())
        })
        .on_window_event(|w, ev| {
            if let tauri::WindowEvent::Destroyed = ev {
                let app = w.state::<App>();
                app.rt.stop();
                app.ix.speech.stop();
            }
        })
        .invoke_handler(tauri::generate_handler![
            status, add_folder, remove_folder, rescan, is_dir, set_paused, set_transcribe,
            get_settings, set_settings, retranscribe_all, clear_cache,
            search, search_by_file, search_similar, transcript, file_info,
            preview, export_clip,
            add_select, remove_select, clear_selects, selects, export_selects
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
