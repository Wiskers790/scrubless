//! ffmpeg/ffprobe wrappers. Everything media-specific (decoding, shot detection, clips) lives here.

use anyhow::{bail, Context, Result};
use regex::Regex;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Video,
    Image,
    Audio,
}

impl Kind {
    pub fn of(path: &Path) -> Option<Kind> {
        let ext = path.extension()?.to_string_lossy().to_lowercase();
        Some(match ext.as_str() {
            "mp4" | "mov" | "mkv" | "webm" | "avi" | "m4v" | "mts" | "m2ts" | "mxf" | "wmv" | "flv" | "3gp" | "mpg"
            | "mpeg" | "ts" => Kind::Video,
            "jpg" | "jpeg" | "png" | "webp" | "bmp" | "gif" | "tif" | "tiff" | "heic" | "avif" => Kind::Image,
            "wav" | "mp3" | "flac" | "m4a" | "aac" | "ogg" | "opus" | "aif" | "aiff" | "wma" => Kind::Audio,
            _ => return None,
        })
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Video => "video",
            Kind::Image => "image",
            Kind::Audio => "audio",
        }
    }
}

fn tool(name: &str, env: &str) -> PathBuf {
    crate::runtime::find_bin(name, env, "ffmpeg").unwrap_or_else(|| PathBuf::from(name))
}

fn ffmpeg() -> &'static PathBuf {
    static P: OnceLock<PathBuf> = OnceLock::new();
    P.get_or_init(|| tool("ffmpeg", "SCRUBLESS_FFMPEG"))
}

fn ffprobe() -> &'static PathBuf {
    static P: OnceLock<PathBuf> = OnceLock::new();
    P.get_or_init(|| tool("ffprobe", "SCRUBLESS_FFPROBE"))
}

fn cmd(bin: &Path) -> Command {
    let mut c = Command::new(bin);
    c.stdin(Stdio::null()).env_remove("LD_PRELOAD");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000);
    }
    c
}

pub struct Probe {
    pub duration: f64,
    pub has_video: bool,
    pub has_audio: bool,
    /// width, height, fps (num/den), codec, start timecode, creation time; shown in the UI and used
    /// for source timecodes and NLE export
    pub meta: serde_json::Value,
}

pub fn probe(path: &Path) -> Result<Probe> {
    let out = cmd(ffprobe())
        .args(["-v", "error", "-print_format", "json", "-show_format", "-show_streams"])
        .arg(path)
        .output()
        .context("ffprobe failed to run")?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    let streams = v["streams"].as_array().cloned().unwrap_or_default();
    let video = streams.iter().find(|s| s["codec_type"] == "video" && s["disposition"]["attached_pic"] != 1);
    let audio = streams.iter().find(|s| s["codec_type"] == "audio");
    let tag = |k: &str| -> Option<String> {
        v["format"]["tags"][k].as_str().map(String::from)
            .or_else(|| streams.iter().find_map(|s| s["tags"][k].as_str().map(String::from)))
    };
    let duration = v["format"]["duration"].as_str().and_then(|d| d.parse().ok()).unwrap_or(0.0);
    let mut meta = serde_json::json!({});
    if let Some(s) = video {
        let parse = |k: &str| s[k].as_str().and_then(|r| {
            let (a, b) = r.split_once('/')?;
            let (n, d) = (a.parse::<u32>().ok()?, b.parse::<u32>().ok()?);
            (n > 0 && d > 0).then(|| n as f64 / d as f64)
        });
        let (num, den) = snap_rate(parse("avg_frame_rate").or_else(|| parse("r_frame_rate")).unwrap_or(25.0));
        meta["width"] = s["width"].clone();
        meta["height"] = s["height"].clone();
        meta["fps_num"] = num.into();
        meta["fps_den"] = den.into();
        meta["vcodec"] = s["codec_name"].clone();
    }
    if let Some(s) = audio {
        meta["acodec"] = s["codec_name"].clone();
        meta["channels"] = s["channels"].clone();
        meta["sample_rate"] = s["sample_rate"].as_str().and_then(|r| r.parse::<u32>().ok()).into();
    }
    if let Some(tc) = tag("timecode") {
        meta["start_tc"] = tc.into();
    }
    if let Some(c) = tag("creation_time").or_else(|| tag("com.apple.quicktime.creationdate")) {
        meta["created"] = c.into();
    }
    if let Some(r) = tag("reel_name").or_else(|| tag("com.apple.proapps.reel")) {
        meta["reel"] = r.into();
    }
    Ok(Probe { duration, has_video: video.is_some(), has_audio: audio.is_some(), meta })
}

/// Containers like WebM/MKV often store odd rates (19001/317). Editors expect standard ones, so snap
/// to the nearest broadcast/web rate within 1%, else to the nearest whole number.
pub fn snap_rate(fps: f64) -> (u32, u32) {
    const STD: [(u32, u32); 12] = [(24000, 1001), (24, 1), (25, 1), (30000, 1001), (30, 1), (48, 1), (50, 1),
                                   (60000, 1001), (60, 1), (100, 1), (120000, 1001), (120, 1)];
    STD.iter()
        .map(|&(n, d)| ((n as f64 / d as f64 - fps).abs() / fps, (n, d)))
        .filter(|(err, _)| *err < 0.01)
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, r)| r)
        .unwrap_or(((fps.round() as u32).max(1), 1))
}

/// Source timecode at `t` seconds into a clip: the camera's start timecode (if any) plus `t`,
/// in HH:MM:SS:FF (";" before frames for drop-frame).
pub fn source_tc(meta: &serde_json::Value, t: f64) -> String {
    if meta.get("fps_num").is_none() {
        // sound files have no frames: plain clock time
        let s = t.max(0.0) as u64;
        return format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60);
    }
    let num = meta["fps_num"].as_u64().unwrap_or(25) as f64;
    let den = meta["fps_den"].as_u64().unwrap_or(1) as f64;
    let fps = num / den;
    let nominal = fps.round().max(1.0) as i64;
    let drop = (fps - fps.round()).abs() > 0.001 && (nominal == 30 || nominal == 60);
    let start = meta["start_tc"].as_str().and_then(|s| tc_to_frames(s, nominal, drop)).unwrap_or(0);
    frames_to_tc(start + (t * fps).floor() as i64, nominal, drop)
}

fn tc_to_frames(tc: &str, fps: i64, drop: bool) -> Option<i64> {
    let parts: Vec<i64> = tc.split(|c| c == ':' || c == ';' || c == '.').map(|p| p.parse().ok()).collect::<Option<_>>()?;
    let [h, m, s, f] = parts[..] else { return None };
    let mut frames = ((h * 60 + m) * 60 + s) * fps + f;
    if drop {
        let d = if fps == 60 { 4 } else { 2 };
        let total_min = h * 60 + m;
        frames -= d * (total_min - total_min / 10);
    }
    Some(frames)
}

fn frames_to_tc(mut frames: i64, fps: i64, drop: bool) -> String {
    let sep = if drop { ';' } else { ':' };
    if drop {
        let d = if fps == 60 { 4 } else { 2 };
        let per10 = fps * 600 - d * 9;
        let (tens, rem) = (frames / per10, frames % per10);
        frames += d * 9 * tens + if rem > d { d * ((rem - d) / (fps * 60 - d)) } else { 0 };
    }
    let f = frames % fps;
    let s = frames / fps;
    format!("{:02}:{:02}:{:02}{sep}{:02}", (s / 3600) % 24, (s / 60) % 60, s % 60, f)
}

/// One frame per shot: a frame at every scene change (but no closer than 1.5s), and at least one
/// every 8s inside long shots. Returns (time, jpeg path) sorted by time; frames are written to `dir`.
pub fn shots(path: &Path, dir: &Path) -> Result<Vec<(f64, PathBuf)>> {
    let sel = "select='isnan(prev_selected_t)+gte(t-prev_selected_t\\,8)+gt(scene\\,0.3)*gte(t-prev_selected_t\\,1.5)'";
    let out = cmd(ffmpeg())
        .args(["-hide_banner", "-v", "info", "-hwaccel", "auto", "-i"])
        .arg(path)
        .args(["-an", "-sn", "-vf", &format!("scale=448:-2,{sel},showinfo"), "-fps_mode", "vfr", "-q:v", "4"])
        .arg(dir.join("f%06d.jpg"))
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .output()?;
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"pts_time:([\d.]+)").unwrap());
    let err = String::from_utf8_lossy(&out.stderr);
    let times: Vec<f64> = re.captures_iter(&err).filter_map(|c| c[1].parse().ok()).collect();
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "jpg").unwrap_or(false))
        .collect();
    files.sort();
    if files.is_empty() && !out.status.success() {
        bail!("could not decode video: {}", err.lines().last().unwrap_or(""));
    }
    Ok(times.into_iter().zip(files).collect())
}

/// A still image as a JPEG at most `w` px wide (also handles PNG/WebP/GIF/etc).
pub fn image_jpeg(path: &Path, w: u32) -> Result<Vec<u8>> {
    let out = cmd(ffmpeg())
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-frames:v", "1", "-vf", &format!("scale='min({w},iw)':-2"), "-f", "image2", "-c:v", "mjpeg", "-q:v", "4", "-"])
        .stderr(Stdio::piped())
        .output()?;
    if out.stdout.is_empty() {
        bail!("could not read image: {}", String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or(""));
    }
    Ok(out.stdout)
}

/// Single frame from a video at `t`, as JPEG.
pub fn frame_at(path: &Path, t: f64, w: u32) -> Result<Vec<u8>> {
    let out = cmd(ffmpeg())
        .args(["-v", "error", "-ss", &format!("{t:.3}"), "-i"])
        .arg(path)
        .args(["-frames:v", "1", "-vf", &format!("scale={w}:-2"), "-f", "image2", "-c:v", "mjpeg", "-q:v", "4", "-"])
        .output()?;
    if out.stdout.is_empty() {
        bail!("could not extract frame");
    }
    Ok(out.stdout)
}

pub const SR: u32 = 16_000;

/// Whole audio track decoded once to 16 kHz mono s16 samples.
pub fn audio_pcm(path: &Path) -> Result<Vec<i16>> {
    let out = cmd(ffmpeg())
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-vn", "-ac", "1", "-ar", &SR.to_string(), "-f", "s16le", "-"])
        .stderr(Stdio::null())
        .output()?;
    Ok(out.stdout.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect())
}

/// Wrap PCM samples in a WAV header.
pub fn wav(samples: &[i16]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut v = Vec::with_capacity(44 + data_len as usize);
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data_len).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes()); // PCM
    v.extend_from_slice(&1u16.to_le_bytes()); // mono
    v.extend_from_slice(&SR.to_le_bytes());
    v.extend_from_slice(&(SR * 2).to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        v.extend_from_slice(&s.to_le_bytes());
    }
    v
}

/// RMS level in dBFS, used to skip silent windows.
pub fn rms_db(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return -120.0;
    }
    let ms = samples.iter().map(|&s| (s as f64) * (s as f64)).sum::<f64>() / samples.len() as f64;
    (20.0 * (ms.sqrt() / 32768.0).max(1e-6).log10()) as f32
}

/// An H.264 encoder that works on this machine. The bundled ffmpeg is an LGPL build without x264,
/// so we try (in order) x264 if present, the platform's hardware encoders, then OpenH264, which
/// every build includes. Each candidate is test-encoded once, because hardware encoders are
/// compiled in even on machines without the hardware.
struct Encoder {
    name: &'static str,
    pix_fmt: &'static str,
}

impl Encoder {
    /// Quality settings scaled to the output height (≈ visually clean, not archival).
    fn args(&self, height: u32) -> Vec<String> {
        let mbps = match height {
            0..=540 => 2,
            541..=1080 => 12,
            1081..=1440 => 20,
            _ => 40,
        };
        let v: Vec<&str> = match self.name {
            "libx264" => vec!["-preset", "veryfast", "-crf", "20"],
            "h264_nvenc" => vec!["-preset", "p4", "-rc", "vbr", "-cq", "21"],
            "h264_qsv" => vec!["-global_quality", "22"],
            "h264_amf" => vec!["-rc", "cqp", "-qp_i", "20", "-qp_p", "22"],
            _ => vec![], // bitrate-driven: videotoolbox, openh264
        };
        let mut out: Vec<String> = vec!["-c:v".into(), self.name.into()];
        out.extend(v.into_iter().map(String::from));
        if matches!(self.name, "h264_videotoolbox" | "libopenh264") {
            out.extend(["-b:v".to_string(), format!("{mbps}M"), "-maxrate".into(), format!("{}M", mbps * 2), "-bufsize".into(), format!("{}M", mbps * 2)]);
        }
        out.extend(["-pix_fmt".to_string(), self.pix_fmt.to_string()]);
        out
    }
}

fn h264() -> &'static Encoder {
    static E: OnceLock<Encoder> = OnceLock::new();
    E.get_or_init(|| {
        let candidates: &[(&'static str, &'static str)] = &[
            ("libx264", "yuv420p"),
            #[cfg(target_os = "macos")]
            ("h264_videotoolbox", "yuv420p"),
            ("h264_nvenc", "yuv420p"),
            ("h264_qsv", "nv12"),
            #[cfg(windows)]
            ("h264_amf", "yuv420p"),
            ("libopenh264", "yuv420p"),
        ];
        for &(name, pix_fmt) in candidates {
            let ok = cmd(ffmpeg())
                .args(["-v", "error", "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=25", "-frames:v", "8",
                       "-c:v", name, "-pix_fmt", pix_fmt, "-f", "null", "-"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if ok {
                eprintln!("video encoder: {name}");
                return Encoder { name, pix_fmt };
            }
        }
        Encoder { name: "libopenh264", pix_fmt: "yuv420p" }
    })
}

/// H.264/AAC MP4 of [t0, t0+dur): plays in any webview and drops into any editor.
pub fn clip(path: &Path, t0: f64, dur: f64, out: &Path, height: u32) -> Result<()> {
    let tmp = out.with_extension("part.mp4");
    let st = cmd(ffmpeg())
        .args(["-v", "error", "-y", "-ss", &format!("{:.3}", t0.max(0.0)), "-i"])
        .arg(path)
        .args(["-t", &format!("{dur:.3}"), "-map", "0:v:0?", "-map", "0:a:0?",
               // even dimensions: every H.264 encoder needs them
               "-vf", &format!("scale=-2:'min({height},ih)':flags=bicubic,scale=trunc(iw/2)*2:trunc(ih/2)*2")])
        .args(h264().args(height))
        .args(["-c:a", "aac", "-b:a", "160k", "-movflags", "+faststart"])
        .arg(&tmp)
        .stderr(Stdio::piped())
        .output()?;
    if !st.status.success() {
        let _ = std::fs::remove_file(&tmp);
        bail!("clip export failed ({}): {}", h264().name, String::from_utf8_lossy(&st.stderr).lines().last().unwrap_or(""));
    }
    std::fs::rename(&tmp, out)?;
    Ok(())
}

/// Audio snippet as WAV (for previews / export of sound hits from audio files).
pub fn audio_clip(path: &Path, t0: f64, dur: f64, out: &Path) -> Result<()> {
    let st = cmd(ffmpeg())
        .args(["-v", "error", "-y", "-ss", &format!("{:.3}", t0.max(0.0)), "-i"])
        .arg(path)
        .args(["-t", &format!("{dur:.3}"), "-vn", "-c:a", "pcm_s16le"])
        .arg(out)
        .output()?;
    if !st.status.success() {
        bail!("audio export failed");
    }
    Ok(())
}
