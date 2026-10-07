//! Export selected moments as a timeline that editors open in their NLE, referencing the original
//! media (no re-encode):
//!   - FCPXML 1.8  → Final Cut Pro (10.4+), DaVinci Resolve
//!   - FCP7 XML    → Premiere Pro, DaVinci Resolve
//!   - EDL (CMX3600) → Avid, Premiere, Resolve (conform by clip name / source file comment)
//!   - CSV         → spreadsheets, logging
//! Times are frame-aligned to each clip's own frame rate; the timeline uses the first clip's rate.

use serde_json::Value;
use std::fmt::Write as _;
use std::path::Path;

pub struct Clip {
    pub path: String,
    pub kind: String, // video | audio | image
    pub meta: Value,
    pub duration: f64, // whole file
    pub t0: f64,       // source in (seconds)
    pub t1: f64,       // source out (seconds)
    pub note: Option<String>,
}

#[derive(Clone, Copy)]
struct Rate {
    num: u64, // frames per `den` seconds, e.g. 30000/1001
    den: u64,
}

impl Rate {
    fn of(meta: &Value) -> Rate {
        let num = meta["fps_num"].as_u64().unwrap_or(25).max(1);
        let den = meta["fps_den"].as_u64().unwrap_or(1).max(1);
        let (num, den) = crate::media::snap_rate(num as f64 / den as f64);
        Rate { num: num as u64, den: den as u64 }
    }
    fn fps(&self) -> f64 {
        self.num as f64 / self.den as f64
    }
    fn timebase(&self) -> u64 {
        self.fps().round() as u64
    }
    fn ntsc(&self) -> bool {
        (self.fps() - self.fps().round()).abs() > 0.001
    }
    fn frames(&self, secs: f64) -> u64 {
        (secs * self.fps()).round().max(0.0) as u64
    }
    /// FCPXML rational time for a frame count: frames * den / num seconds.
    fn rt(&self, frames: u64) -> String {
        if frames == 0 {
            return "0s".into();
        }
        format!("{}/{}s", frames * self.den, self.num)
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

fn file_url(path: &str) -> String {
    let p = path.replace('\\', "/");
    let encoded: String = p
        .split('/')
        .map(|seg| urlencoding::encode(seg).into_owned())
        .collect::<Vec<_>>()
        .join("/");
    if p.starts_with('/') {
        format!("file://{encoded}")
    } else {
        format!("file:///{encoded}") // Windows drive path: C:/...
    }
}

fn name(path: &str) -> String {
    Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

/// Start timecode of the media in frames (0 when the file carries none).
fn start_frames(meta: &Value, r: Rate) -> u64 {
    let Some(tc) = meta["start_tc"].as_str() else { return 0 };
    let p: Vec<u64> = tc.split(|c| c == ':' || c == ';' || c == '.').filter_map(|x| x.parse().ok()).collect();
    if p.len() != 4 {
        return 0;
    }
    ((p[0] * 60 + p[1]) * 60 + p[2]) * r.timebase() + p[3]
}

fn tc(frames: u64, tb: u64) -> String {
    let (f, s) = (frames % tb, frames / tb);
    format!("{:02}:{:02}:{:02}:{:02}", (s / 3600) % 24, (s / 60) % 60, s % 60, f)
}

fn timeline_clips(clips: &[Clip]) -> Vec<&Clip> {
    clips.iter().filter(|c| c.kind != "image" && c.t1 > c.t0).collect()
}

pub fn fcpxml(clips: &[Clip], title: &str) -> String {
    let clips = timeline_clips(clips);
    let seq_rate = clips.iter().find(|c| c.kind == "video").map(|c| Rate::of(&c.meta)).unwrap_or(Rate { num: 25, den: 1 });
    let (w, h) = clips
        .iter()
        .find(|c| c.kind == "video")
        .map(|c| (c.meta["width"].as_u64().unwrap_or(1920), c.meta["height"].as_u64().unwrap_or(1080)))
        .unwrap_or((1920, 1080));
    let mut res = String::new();
    let mut spine = String::new();
    let mut formats: Vec<(u64, u64, u64, u64)> = vec![(seq_rate.num, seq_rate.den, w, h)];
    let fmt_id = |formats: &mut Vec<(u64, u64, u64, u64)>, key: (u64, u64, u64, u64)| -> usize {
        if let Some(i) = formats.iter().position(|f| *f == key) {
            i
        } else {
            formats.push(key);
            formats.len() - 1
        }
    };
    let mut assets: Vec<String> = Vec::new();
    let mut offset = 0u64; // in sequence frames
    for c in &clips {
        let r = if c.kind == "video" { Rate::of(&c.meta) } else { seq_rate };
        let asset_idx = match assets.iter().position(|p| p == &c.path) {
            Some(i) => i,
            None => {
                assets.push(c.path.clone());
                let fi = if c.kind == "video" {
                    fmt_id(&mut formats, (r.num, r.den, c.meta["width"].as_u64().unwrap_or(w), c.meta["height"].as_u64().unwrap_or(h)))
                } else {
                    0
                };
                let has_audio = c.meta.get("acodec").is_some() || c.kind == "audio";
                let _ = write!(
                    res,
                    r#"    <asset id="a{}" name="{}" start="{}" duration="{}" hasVideo="{}" format="f{}" hasAudio="{}" audioSources="1" audioChannels="{}" audioRate="{}" src="{}"/>
"#,
                    assets.len(),
                    esc(&name(&c.path)),
                    r.rt(start_frames(&c.meta, r)),
                    r.rt(r.frames(c.duration)),
                    (c.kind == "video") as u8,
                    fi + 1,
                    has_audio as u8,
                    c.meta["channels"].as_u64().unwrap_or(2),
                    c.meta["sample_rate"].as_u64().unwrap_or(48000),
                    esc(&file_url(&c.path))
                );
                assets.len() - 1
            }
        };
        let (in_f, out_f) = (r.frames(c.t0), r.frames(c.t1).max(r.frames(c.t0) + 1));
        let dur_seq = seq_rate.frames((out_f - in_f) as f64 / r.fps());
        let _ = write!(
            spine,
            r#"          <asset-clip ref="a{}" offset="{}" name="{}" start="{}" duration="{}" tcFormat="NDF">{}</asset-clip>
"#,
            asset_idx + 1,
            seq_rate.rt(offset),
            esc(&name(&c.path)),
            r.rt(start_frames(&c.meta, r) + in_f),
            seq_rate.rt(dur_seq),
            c.note.as_ref().map(|n| format!(r#"<note>{}</note>"#, esc(n))).unwrap_or_default()
        );
        offset += dur_seq;
    }
    let mut fmts = String::new();
    for (i, (num, den, fw, fh)) in formats.iter().enumerate() {
        let _ = writeln!(fmts, r#"    <format id="f{}" frameDuration="{}/{}s" width="{}" height="{}"/>"#, i + 1, den, num, fw, fh);
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE fcpxml>
<fcpxml version="1.8">
  <resources>
{fmts}{res}  </resources>
  <library>
    <event name="{t}">
      <project name="{t}">
        <sequence format="f1" duration="{d}" tcStart="0s" tcFormat="NDF" audioLayout="stereo" audioRate="48k">
          <spine>
{spine}          </spine>
        </sequence>
      </project>
    </event>
  </library>
</fcpxml>
"#,
        t = esc(title),
        d = seq_rate.rt(offset)
    )
}

/// Final Cut Pro 7 XML (xmeml v4), the interchange format Premiere Pro imports.
pub fn xmeml(clips: &[Clip], title: &str) -> String {
    let clips = timeline_clips(clips);
    let seq = clips.iter().find(|c| c.kind == "video").map(|c| Rate::of(&c.meta)).unwrap_or(Rate { num: 25, den: 1 });
    let rate = |r: Rate| format!("<rate><timebase>{}</timebase><ntsc>{}</ntsc></rate>", r.timebase(), if r.ntsc() { "TRUE" } else { "FALSE" });
    let (mut vtrack, mut atrack, mut pos) = (String::new(), String::new(), 0u64);
    let mut files_done: Vec<String> = Vec::new();
    for (i, c) in clips.iter().enumerate() {
        let r = if c.kind == "video" { Rate::of(&c.meta) } else { seq };
        let (in_f, out_f) = (r.frames(c.t0), r.frames(c.t1).max(r.frames(c.t0) + 1));
        let len_seq = seq.frames((out_f - in_f) as f64 / r.fps());
        let fid = files_done.iter().position(|p| p == &c.path).map(|j| j + 1);
        let file_el = match fid {
            Some(j) => format!(r#"<file id="file-{j}"/>"#),
            None => {
                files_done.push(c.path.clone());
                let j = files_done.len();
                let mut media = String::new();
                if c.kind == "video" {
                    let _ = write!(media, "<video><samplecharacteristics><width>{}</width><height>{}</height></samplecharacteristics></video>",
                                   c.meta["width"].as_u64().unwrap_or(1920), c.meta["height"].as_u64().unwrap_or(1080));
                }
                if c.meta.get("acodec").is_some() || c.kind == "audio" {
                    let _ = write!(media, "<audio><channelcount>{}</channelcount></audio>", c.meta["channels"].as_u64().unwrap_or(2));
                }
                format!(
                    r#"<file id="file-{j}"><name>{}</name><pathurl>{}</pathurl>{}<duration>{}</duration><timecode>{}<string>{}</string><frame>{}</frame></timecode><media>{media}</media></file>"#,
                    esc(&name(&c.path)),
                    esc(&file_url(&c.path)),
                    rate(r),
                    r.frames(c.duration),
                    rate(r),
                    tc(start_frames(&c.meta, r), r.timebase()),
                    start_frames(&c.meta, r)
                )
            }
        };
        let item = |id: &str, file: &str, extra: &str| {
            format!(
                r#"<clipitem id="{id}"><name>{}</name><duration>{}</duration>{}<start>{}</start><end>{}</end><in>{}</in><out>{}</out>{file}{extra}</clipitem>
"#,
                esc(&name(&c.path)), r.frames(c.duration), rate(r), pos, pos + len_seq, in_f, out_f
            )
        };
        if c.kind == "video" {
            vtrack.push_str(&item(&format!("clipitem-v{}", i + 1), &file_el, ""));
            if c.meta.get("acodec").is_some() {
                atrack.push_str(&item(&format!("clipitem-a{}", i + 1), &format!(r#"<file id="file-{}"/>"#, files_done.iter().position(|p| p == &c.path).unwrap() + 1),
                                      "<sourcetrack><mediatype>audio</mediatype><trackindex>1</trackindex></sourcetrack>"));
            }
        } else {
            atrack.push_str(&item(&format!("clipitem-a{}", i + 1), &file_el, "<sourcetrack><mediatype>audio</mediatype><trackindex>1</trackindex></sourcetrack>"));
        }
        pos += len_seq;
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE xmeml>
<xmeml version="4">
<sequence id="sequence-1"><name>{}</name><duration>{pos}</duration>{}
<timecode>{}<string>00:00:00:00</string><frame>0</frame><displayformat>NDF</displayformat></timecode>
<media>
<video><track>
{vtrack}</track></video>
<audio><track>
{atrack}</track></audio>
</media>
</sequence>
</xmeml>
"#,
        esc(title),
        rate(seq),
        rate(seq)
    )
}

/// CMX3600 EDL. Reel = "AX" with a SOURCE FILE comment, which Resolve/Premiere use to relink.
pub fn edl(clips: &[Clip], title: &str) -> String {
    let clips = timeline_clips(clips);
    let seq = clips.iter().find(|c| c.kind == "video").map(|c| Rate::of(&c.meta)).unwrap_or(Rate { num: 25, den: 1 });
    let tb = seq.timebase();
    let mut out = format!("TITLE: {}\nFCM: NON-DROP FRAME\n\n", title.to_uppercase());
    let mut rec = 0u64;
    for (i, c) in clips.iter().enumerate() {
        let r = if c.kind == "video" { Rate::of(&c.meta) } else { seq };
        let src_in = start_frames(&c.meta, r) + r.frames(c.t0);
        let src_out = start_frames(&c.meta, r) + r.frames(c.t1).max(r.frames(c.t0) + 1);
        let len = seq.frames((src_out - src_in) as f64 / r.fps());
        let tracks = if c.kind == "video" { if c.meta.get("acodec").is_some() { "AA/V" } else { "V" } } else { "AA" };
        let _ = write!(
            out,
            "{:03}  AX       {:<5} C        {} {} {} {}\n* FROM CLIP NAME: {}\n* SOURCE FILE: {}\n",
            i + 1, tracks,
            tc(src_in, r.timebase()), tc(src_out, r.timebase()), tc(rec, tb), tc(rec + len, tb),
            name(&c.path), c.path
        );
        if let Some(n) = &c.note {
            let _ = writeln!(out, "* COMMENT: {}", n.replace('\n', " "));
        }
        out.push('\n');
        rec += len;
    }
    out
}

pub fn csv(clips: &[Clip]) -> String {
    let q = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    let mut out = String::from("file,path,in_seconds,out_seconds,source_tc_in,source_tc_out,note\n");
    for c in clips {
        let _ = writeln!(
            out,
            "{},{},{:.3},{:.3},{},{},{}",
            q(&name(&c.path)), q(&c.path), c.t0, c.t1,
            crate::media::source_tc(&c.meta, c.t0), crate::media::source_tc(&c.meta, c.t1),
            q(c.note.as_deref().unwrap_or(""))
        );
    }
    out
}
