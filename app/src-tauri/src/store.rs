//! Persistence (SQLite) plus the in-memory vector matrix used for search.
//!
//! Items are the searchable units: a shot of a video ("frame"), a 10 s window of sound ("audio",
//! from a video or an audio file) or a still image ("image"). Vectors are stored as f16.

use anyhow::Result;
use half::f16;
use parking_lot::{Mutex, RwLock};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::runtime::DIM;

const SCHEMA: &str = r#"
pragma journal_mode = wal;
create table if not exists folders (id integer primary key, path text unique not null);
create table if not exists files (
    id integer primary key,
    path text unique not null,
    folder_id integer not null,
    kind text not null,
    size integer not null,
    mtime integer not null,
    duration real,
    status text not null default 'pending',   -- pending | done | error
    error text
);
create index if not exists files_status on files(status);
create table if not exists items (
    id integer primary key,
    file_id integer not null references files(id) on delete cascade,
    kind text not null,                       -- frame | audio | image
    t0 real not null default 0,
    t1 real not null default 0,
    thumb text,
    vec blob not null
);
create index if not exists items_file on items(file_id);
create table if not exists selects (
    id integer primary key,
    file_id integer not null references files(id) on delete cascade,
    t0 real not null,
    t1 real not null,
    note text,
    added integer not null
);
create table if not exists settings (key text primary key, value text not null);
-- what was said: trigram tokens give substring matching in every script (CJK has no spaces)
create virtual table if not exists speech_fts using fts5(text, tokenize = 'trigram');
"#;

/// Columns added after the first release; applied idempotently on open.
const MIGRATIONS: [(&str, &str, &str); 3] = [
    ("files", "meta", "alter table files add column meta text"),
    ("files", "speech", "alter table files add column speech text"),
    ("items", "text", "alter table items add column text text"),
];

#[derive(Clone, Serialize, Debug)]
pub struct ItemMeta {
    pub id: i64,
    pub file_id: i64,
    pub kind: String,
    pub t0: f64,
    pub t1: f64,
    pub thumb: Option<String>,
    pub text: Option<String>,
}

#[derive(Clone, Serialize, Debug, Default)]
pub struct FileInfo {
    pub path: String,
    pub kind: String,
    pub mtime: i64,
    pub size: i64,
    pub duration: f64,
    pub meta: serde_json::Value,
}

pub struct NewItem {
    pub kind: &'static str,
    pub t0: f64,
    pub t1: f64,
    pub thumb: Option<PathBuf>,
    pub text: Option<String>,
    pub vec: Vec<f32>,
}

#[derive(Default)]
struct Matrix {
    vecs: Vec<f32>, // row-major, DIM per item
    metas: Vec<ItemMeta>,
}

pub struct Store {
    db: Mutex<Connection>,
    mat: RwLock<Matrix>,
    pub files: RwLock<HashMap<i64, FileInfo>>, // for results and filters
}

/// Text as it is indexed for exact search, and queries are normalized the same way: NFKC,
/// lowercase, punctuation as spaces, no stray spaces before combining marks (whisper splits
/// Devanagari vowel signs off their letters), and no spaces between CJK characters (which whisper
/// inserts or not at segment joins).
pub fn norm_text(s: &str) -> String {
    use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};
    let cjk = |c: char| matches!(c as u32, 0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF);
    let chars: Vec<char> = s.nfkc().flat_map(char::to_lowercase).map(|c| if c.is_alphanumeric() || is_combining_mark(c) { c } else { ' ' }).collect();
    let mut out = String::with_capacity(chars.len());
    for (i, &c) in chars.iter().enumerate() {
        if c == ' ' {
            let prev = out.chars().last();
            let next = chars[i + 1..].iter().copied().find(|&n| n != ' ');
            let drop = prev.map_or(true, |p| p == ' ')
                || next.map_or(true, |n| is_combining_mark(n) || (cjk(n) && prev.map_or(false, cjk)));
            if drop {
                continue;
            }
        }
        out.push(c);
    }
    out
}

fn to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| f16::from_f32(*x).to_le_bytes()).collect()
}

fn from_blob(b: &[u8], out: &mut Vec<f32>) {
    out.extend(b.chunks_exact(2).map(|c| f16::from_le_bytes([c[0], c[1]]).to_f32()));
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path)?;
        db.execute_batch(SCHEMA)?;
        for (table, col, sql) in MIGRATIONS {
            let has: bool = db.prepare(&format!("select 1 from pragma_table_info('{table}') where name='{col}'"))?.exists([])?;
            if !has {
                db.execute_batch(sql)?;
            }
        }
        if db.query_row("pragma user_version", [], |r| r.get::<_, i64>(0))? < 1 {
            // v1: the exact-search index stores normalized text
            let rows: Vec<(i64, String)> = db.prepare("select id, text from items where kind='speech' and text is not null")?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
            db.execute("delete from speech_fts", [])?;
            for (id, t) in rows {
                db.execute("insert into speech_fts(rowid, text) values (?, ?)", params![id, norm_text(&t)])?;
            }
            db.execute_batch("pragma user_version = 1")?;
        }
        db.execute_batch("pragma foreign_keys = on;")?;
        let s = Self { db: Mutex::new(db), mat: RwLock::new(Matrix::default()), files: RwLock::new(HashMap::new()) };
        s.reload()?;
        Ok(s)
    }

    pub fn reload(&self) -> Result<()> {
        let db = self.db.lock();
        let mut m = Matrix::default();
        let mut st = db.prepare("select i.id, i.file_id, i.kind, i.t0, i.t1, i.thumb, i.vec, i.text from items i order by i.id")?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let blob: Vec<u8> = r.get(6)?;
            if blob.len() != DIM * 2 {
                continue;
            }
            from_blob(&blob, &mut m.vecs);
            m.metas.push(ItemMeta { id: r.get(0)?, file_id: r.get(1)?, kind: r.get(2)?, t0: r.get(3)?, t1: r.get(4)?, thumb: r.get(5)?, text: r.get(7)? });
        }
        let mut files = HashMap::new();
        let mut st = db.prepare("select id, path, kind, mtime, size, coalesce(duration, 0), meta from files")?;
        for r in st.query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, FileInfo {
                path: r.get(1)?, kind: r.get(2)?, mtime: r.get(3)?, size: r.get(4)?, duration: r.get(5)?,
                meta: r.get::<_, Option<String>>(6)?.and_then(|m| serde_json::from_str(&m).ok()).unwrap_or_default(),
            }))
        })? {
            let (id, f) = r?;
            files.insert(id, f);
        }
        *self.mat.write() = m;
        *self.files.write() = files;
        Ok(())
    }

    // ---------------------------------------------------------------- folders & files

    pub fn add_folder(&self, path: &str) -> Result<i64> {
        let db = self.db.lock();
        db.execute("insert or ignore into folders(path) values (?)", [path])?;
        Ok(db.query_row("select id from folders where path=?", [path], |r| r.get(0))?)
    }

    pub fn remove_folder(&self, path: &str) -> Result<()> {
        {
            let db = self.db.lock();
            if let Some(id) = db.query_row("select id from folders where path=?", [path], |r| r.get::<_, i64>(0)).optional()? {
                db.execute("delete from speech_fts where rowid in (select i.id from items i join files f on f.id=i.file_id where f.folder_id=?)", [id])?;
                db.execute("delete from items where file_id in (select id from files where folder_id=?)", [id])?;
                db.execute("delete from files where folder_id=?", [id])?;
                db.execute("delete from folders where id=?", [id])?;
            }
        }
        self.reload()
    }

    pub fn folders(&self) -> Result<Vec<(i64, String)>> {
        let db = self.db.lock();
        let mut st = db.prepare("select id, path from folders order by path")?;
        let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    /// Insert a new file, or mark a changed one (size/mtime differ) for re-indexing.
    /// Returns true if the file needs (re)indexing.
    pub fn upsert_file(&self, folder_id: i64, path: &str, kind: &str, size: i64, mtime: i64) -> Result<bool> {
        let db = self.db.lock();
        let existing: Option<(i64, i64, i64, String)> = db
            .query_row("select id, size, mtime, status from files where path=?", [path], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .optional()?;
        match existing {
            None => {
                db.execute("insert into files(path, folder_id, kind, size, mtime) values (?,?,?,?,?)", params![path, folder_id, kind, size, mtime])?;
                Ok(true)
            }
            Some((id, s, m, _)) if s != size || m != mtime => {
                db.execute("delete from speech_fts where rowid in (select id from items where file_id=?)", [id])?;
                db.execute("delete from items where file_id=?", [id])?;
                db.execute("update files set size=?, mtime=?, status='pending', error=null where id=?", params![size, mtime, id])?;
                Ok(true)
            }
            Some((_, _, _, status)) => Ok(status == "pending"),
        }
    }

    /// Forget files under `folder_id` that are no longer on disk.
    pub fn prune_missing(&self, folder_id: i64, seen: &std::collections::HashSet<String>) -> Result<usize> {
        let db = self.db.lock();
        let mut st = db.prepare("select id, path from files where folder_id=?")?;
        let gone: Vec<i64> = st
            .query_map([folder_id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
            .flatten()
            .filter(|(_, p)| !seen.contains(p))
            .map(|(id, _)| id)
            .collect();
        for id in &gone {
            db.execute("delete from speech_fts where rowid in (select id from items where file_id=?)", [id])?;
            db.execute("delete from items where file_id=?", [id])?;
            db.execute("delete from files where id=?", [id])?;
        }
        Ok(gone.len())
    }

    pub fn next_pending(&self) -> Result<Option<(i64, String, String)>> {
        let db = self.db.lock();
        // images first (fast, gives the user results quickly), then audio, then video
        Ok(db
            .query_row(
                "select id, path, kind from files where status='pending'
                 order by case kind when 'image' then 0 when 'audio' then 1 else 2 end, size limit 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?)
    }

    pub fn counts(&self) -> Result<(i64, i64, i64, i64)> {
        let db = self.db.lock();
        Ok(db.query_row(
            "select count(*), coalesce(sum(status='done'),0), coalesce(sum(status='pending'),0), coalesce(sum(status='error'),0) from files",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?)
    }

    /// `has_audio`: the file has a sound track, so it gets transcribed (independent of whether any
    /// window was loud enough to become a sound-search item).
    pub fn finish_file(&self, file_id: i64, path: &str, duration: f64, meta: serde_json::Value, has_audio: bool, items: Vec<NewItem>) -> Result<()> {
        let mut new_metas = Vec::with_capacity(items.len());
        let mut new_vecs = Vec::with_capacity(items.len() * DIM);
        {
            let mut db = self.db.lock();
            let tx = db.transaction()?;
            tx.execute("delete from speech_fts where rowid in (select id from items where file_id=?)", [file_id])?;
            tx.execute("delete from items where file_id=?", [file_id])?;
            for it in &items {
                let thumb = it.thumb.as_ref().map(|p| p.to_string_lossy().to_string());
                tx.execute(
                    "insert into items(file_id, kind, t0, t1, thumb, vec, text) values (?,?,?,?,?,?,?)",
                    params![file_id, it.kind, it.t0, it.t1, thumb, to_blob(&it.vec), it.text],
                )?;
                let id = tx.last_insert_rowid();
                new_metas.push(ItemMeta { id, file_id, kind: it.kind.into(), t0: it.t0, t1: it.t1, thumb, text: it.text.clone() });
                from_blob(&to_blob(&it.vec), &mut new_vecs); // keep memory identical to what is stored
            }
            let speech = if has_audio { "pending" } else { "none" };
            tx.execute("update files set status='done', duration=?, meta=?, speech=?, error=null where id=?",
                       params![duration, meta.to_string(), speech, file_id])?;
            tx.commit()?;
        }
        // files indexed after startup must enter the in-memory table too, or search drops them
        let row = self.db.lock().query_row("select kind, mtime, size from files where id=?", [file_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
        });
        if let Ok((kind, mtime, size)) = row {
            self.files.write().insert(file_id, FileInfo { path: path.to_string(), kind, mtime, size, duration, meta });
        }
        let reindexed = self.mat.read().metas.iter().any(|x| x.file_id == file_id);
        if reindexed {
            return self.reload(); // rare: simplest way to drop the old rows from the matrix
        }
        let mut m = self.mat.write();
        m.vecs.extend(new_vecs);
        m.metas.extend(new_metas);
        Ok(())
    }

    /// Next file whose sound still needs transcribing (visual indexing of everything comes first).
    pub fn next_speech(&self) -> Result<Option<(i64, String)>> {
        let db = self.db.lock();
        Ok(db
            .query_row("select id, path from files where status='done' and speech='pending' order by size limit 1", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?)
    }

    pub fn speech_counts(&self) -> Result<(i64, i64)> {
        let db = self.db.lock();
        Ok(db.query_row(
            "select coalesce(sum(speech='done'),0), coalesce(sum(speech='pending'),0) from files where status='done'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }

    /// Store a file's transcript: one item per segment (vector = text embedding), plus full-text rows.
    pub fn finish_speech(&self, file_id: i64, language: &str, items: Vec<NewItem>) -> Result<()> {
        let mut new_metas = Vec::new();
        let mut new_vecs = Vec::new();
        {
            let mut db = self.db.lock();
            let tx = db.transaction()?;
            tx.execute("delete from speech_fts where rowid in (select id from items where file_id=? and kind='speech')", [file_id])?;
            tx.execute("delete from items where file_id=? and kind='speech'", [file_id])?;
            for it in &items {
                tx.execute("insert into items(file_id, kind, t0, t1, vec, text) values (?,?,?,?,?,?)",
                           params![file_id, "speech", it.t0, it.t1, to_blob(&it.vec), it.text])?;
                let id = tx.last_insert_rowid();
                tx.execute("insert into speech_fts(rowid, text) values (?, ?)", params![id, it.text.as_deref().map(norm_text)])?;
                new_metas.push(ItemMeta { id, file_id, kind: "speech".into(), t0: it.t0, t1: it.t1, thumb: None, text: it.text.clone() });
                from_blob(&to_blob(&it.vec), &mut new_vecs);
            }
            tx.execute("update files set speech='done', meta=json_set(coalesce(meta,'{}'), '$.language', ?) where id=?", params![language, file_id])?;
            tx.commit()?;
        }
        let mut m = self.mat.write();
        m.metas.retain(|x| !(x.file_id == file_id && x.kind == "speech"));
        if m.metas.len() * DIM != m.vecs.len() {
            drop(m);
            return self.reload();
        }
        m.vecs.extend(new_vecs);
        m.metas.extend(new_metas);
        drop(m);
        if let Some(f) = self.files.write().get_mut(&file_id) {
            f.meta["language"] = serde_json::json!(language);
        }
        Ok(())
    }

    pub fn fail_speech(&self, file_id: i64) -> Result<()> {
        self.db.lock().execute("update files set speech='error' where id=?", [file_id])?;
        Ok(())
    }

    /// Exact words in transcripts. Returns item ids, best first.
    pub fn keyword(&self, q: &str, limit: usize) -> Result<Vec<i64>> {
        let q = norm_text(q);
        let q = q.trim();
        if q.is_empty() {
            return Ok(vec![]);
        }
        let db = self.db.lock();
        if q.chars().count() >= 3 {
            let phrase = format!("\"{}\"", q.replace('"', "\"\""));
            let mut st = db.prepare("select rowid from speech_fts where speech_fts match ? order by rank limit ?")?;
            let ids = st.query_map(params![phrase, limit as i64], |r| r.get(0))?.collect::<Result<Vec<i64>, _>>()?;
            return Ok(ids);
        }
        // too short for trigrams: scan the normalized index
        let mut st = db.prepare("select rowid from speech_fts where text like ? limit ?")?;
        let ids = st.query_map(params![format!("%{q}%"), limit as i64], |r| r.get(0))?.collect::<Result<Vec<i64>, _>>()?;
        Ok(ids)
    }

    /// Thumbnail of the shot that contains `t` (for transcript hits and selects).
    pub fn nearest_thumb(&self, file_id: i64, t: f64) -> Option<String> {
        let m = self.mat.read();
        m.metas
            .iter()
            .filter(|x| x.file_id == file_id && x.thumb.is_some())
            .min_by(|a, b| {
                let da = if a.t0 <= t { t - a.t0 } else { (a.t0 - t) * 4.0 }; // prefer the shot already running
                let db = if b.t0 <= t { t - b.t0 } else { (b.t0 - t) * 4.0 };
                da.total_cmp(&db)
            })
            .and_then(|x| x.thumb.clone())
    }

    pub fn item(&self, id: i64) -> Option<ItemMeta> {
        self.mat.read().metas.iter().find(|m| m.id == id).cloned()
    }

    /// All transcript lines of a file, in order (for the detail panel).
    pub fn transcript(&self, file_id: i64) -> Vec<ItemMeta> {
        let mut v: Vec<ItemMeta> = self.mat.read().metas.iter().filter(|m| m.file_id == file_id && m.kind == "speech").cloned().collect();
        v.sort_by(|a, b| a.t0.total_cmp(&b.t0));
        v
    }

    // ---------------------------------------------------------------- settings

    pub fn setting(&self, key: &str) -> Option<String> {
        self.db.lock().query_row("select value from settings where key=?", [key], |r| r.get(0)).optional().ok().flatten()
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.db.lock().execute("insert into settings(key, value) values (?, ?) on conflict(key) do update set value=excluded.value", [key, value])?;
        Ok(())
    }

    /// Queue every transcribed file again (after changing the speech language or quality).
    pub fn retranscribe_all(&self) -> Result<usize> {
        Ok(self.db.lock().execute("update files set speech='pending' where speech in ('done', 'error')", [])?)
    }

    // ---------------------------------------------------------------- selects

    pub fn add_select(&self, file_id: i64, t0: f64, t1: f64, note: Option<String>) -> Result<i64> {
        let db = self.db.lock();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        db.execute("insert into selects(file_id, t0, t1, note, added) values (?,?,?,?,?)", params![file_id, t0, t1, note, now])?;
        Ok(db.last_insert_rowid())
    }

    pub fn remove_select(&self, id: i64) -> Result<()> {
        self.db.lock().execute("delete from selects where id=?", [id])?;
        Ok(())
    }

    pub fn clear_selects(&self) -> Result<()> {
        self.db.lock().execute("delete from selects", [])?;
        Ok(())
    }

    pub fn selects(&self) -> Result<Vec<(i64, i64, f64, f64, Option<String>)>> {
        let db = self.db.lock();
        let mut st = db.prepare("select id, file_id, t0, t1, note from selects order by id")?;
        let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn fail_file(&self, file_id: i64, err: &str) -> Result<()> {
        self.db.lock().execute("update files set status='error', error=? where id=?", params![err, file_id])?;
        Ok(())
    }

    pub fn item_count(&self) -> usize {
        self.mat.read().metas.len()
    }

    // ---------------------------------------------------------------- search

    /// The stored vector of one item ("find similar").
    pub fn vector(&self, item_id: i64) -> Option<(String, Vec<f32>)> {
        let m = self.mat.read();
        let i = m.metas.iter().position(|x| x.id == item_id)?;
        Some((m.metas[i].kind.clone(), m.vecs[i * DIM..(i + 1) * DIM].to_vec()))
    }

    /// Score every item of `kind` against `q` and return (score, z, meta), best first, up to `k`.
    /// `z` is how far the score stands out from this query's scores over the whole library: real
    /// matches sit at z >= ~2, while the tail is "least bad" filler (see ground.py at repo root).
    pub fn top(&self, q: &[f32], kind: &str, k: usize) -> Vec<(f32, f32, ItemMeta)> {
        let m = self.mat.read();
        let mut scored: Vec<(f32, usize)> = m
            .metas
            .iter()
            .enumerate()
            .filter(|(_, meta)| meta.kind == kind)
            .map(|(i, _)| {
                let row = &m.vecs[i * DIM..(i + 1) * DIM];
                (row.iter().zip(q).map(|(a, b)| a * b).sum::<f32>(), i)
            })
            .collect();
        let n = scored.len();
        if n == 0 {
            return vec![];
        }
        let mean = scored.iter().map(|x| x.0).sum::<f32>() / n as f32;
        let std = (scored.iter().map(|x| (x.0 - mean).powi(2)).sum::<f32>() / n as f32).sqrt().max(1e-6);
        let k = k.min(n);
        scored.select_nth_unstable_by(k - 1, |a, b| b.0.total_cmp(&a.0));
        scored.truncate(k);
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));
        // with a tiny library the distribution says nothing; treat everything as a match
        let z = |s: f32| if n < 40 { 9.0 } else { (s - mean) / std };
        scored.into_iter().map(|(s, i)| (s, z(s), m.metas[i].clone())).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::norm_text;

    #[test]
    fn normalizes_for_exact_search() {
        assert_eq!(norm_text("Riot gear, entered the YARD, and"), "riot gear entered the yard and");
        assert_eq!(norm_text("ग्लोबल वॉर्म िंग से"), "ग्लोबल वॉर्मिंग से");
        assert_eq!(norm_text("正解 です。 お腹すいた"), "正解ですお腹すいた");
        assert_eq!(norm_text("  ¿Qué   tal?  "), "qué tal");
    }
}
