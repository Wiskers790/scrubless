//! Speech-to-text: a whisper.cpp server (whisper-server) started on demand, used to transcribe the
//! sound of every video and audio file so people can search for what was said.

use anyhow::{anyhow, bail, Context, Result};
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::runtime::{child_hygiene, download_file, find_bin, free_port, kill_stale, log_file};

const BASE: &str = "https://huggingface.co";
/// (file, url): accurate multilingual model for GPUs, small one for CPU-only machines.
pub const ACCURATE: (&str, &str) = ("ggml-large-v3-turbo-q5_0.bin", "/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q5_0.bin");
pub const FAST: (&str, &str) = ("ggml-small-q5_1.bin", "/ggerganov/whisper.cpp/resolve/main/ggml-small-q5_1.bin");
const VAD: (&str, &str) = ("ggml-silero-v6.2.0.bin", "/ggml-org/whisper-vad/resolve/main/ggml-silero-v6.2.0.bin");

#[derive(Clone, Serialize, Debug, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SpeechState {
    Idle,
    Downloading { file: String, done: u64, total: u64 },
    Starting,
    Ready { model: String, device: String },
    Unavailable { error: String },
}

pub struct Segment {
    pub t0: f64,
    pub t1: f64,
    pub text: String,
}

pub struct Transcript {
    pub language: String,
    pub segments: Vec<Segment>,
}

pub struct Speech {
    /// "auto" or a whisper language code ("en", "hi", ...): set it when all footage is in one
    /// language (auto-detect writes some Hindi as Urdu, for example)
    pub language: Mutex<String>,
    /// "auto" (accurate on GPU, fast on CPU) | "accurate" | "fast"
    pub quality: Mutex<String>,
    model_dir: PathBuf,
    pidfile: PathBuf,
    bin: Option<PathBuf>,
    child: Mutex<Option<Child>>,
    port: Mutex<u16>,
    pub state: Mutex<SpeechState>,
    agent: ureq::Agent,
}

impl Speech {
    pub fn new(model_dir: &Path, data_dir: &Path) -> Self {
        let pidfile = data_dir.join("speech.pid");
        kill_stale(&pidfile, "whisper-server");
        Self {
            language: Mutex::new("auto".into()),
            quality: Mutex::new("auto".into()),
            model_dir: model_dir.to_path_buf(),
            pidfile,
            bin: find_bin("whisper-server", "SCRUBLESS_WHISPER_SERVER", "whisper"),
            child: Mutex::new(None),
            port: Mutex::new(0),
            state: Mutex::new(SpeechState::Idle),
            agent: ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(3600))).http_status_as_error(false).build().into(),
        }
    }

    fn alive(&self) -> bool {
        match self.child.lock().as_mut() {
            Some(c) => matches!(c.try_wait(), Ok(None)),
            None => false,
        }
    }

    /// Start the server if needed (downloading models first). `gpu` follows the embedding engine.
    pub fn ensure(&self, gpu: bool) -> Result<()> {
        if self.alive() && matches!(*self.state.lock(), SpeechState::Ready { .. }) {
            return Ok(());
        }
        let bin = self.bin.clone().ok_or_else(|| anyhow!("speech engine not installed"))?;
        let (model, url) = match self.quality.lock().as_str() {
            "accurate" => ACCURATE,
            "fast" => FAST,
            _ if gpu => ACCURATE,
            _ => FAST,
        };
        let language = self.language.lock().clone();
        for (file, path) in [(model, url), VAD] {
            download_file(&self.agent, &format!("{BASE}{path}"), &self.model_dir.join(file), |done, total| {
                *self.state.lock() = SpeechState::Downloading { file: file.into(), done, total };
            })?;
        }
        *self.state.lock() = SpeechState::Starting;
        for use_gpu in [gpu, false] {
            self.stop();
            let port = free_port();
            *self.port.lock() = port;
            let threads = std::thread::available_parallelism().map(|n| n.get().min(8)).unwrap_or(4);
            let mut cmd = Command::new(&bin);
            cmd.arg("-m").arg(self.model_dir.join(model))
                .arg("--vad").arg("-vm").arg(self.model_dir.join(VAD.0))
                // 30 ms default padding clipped first and last words (FLEURS eval); 300 ms keeps them
                .args(["-vp", "300"])
                .args(["-l", &language, "-t", &threads.to_string(), "--host", "127.0.0.1", "--port", &port.to_string()])
                .stdin(Stdio::null()).stdout(Stdio::null()).stderr(log_file(&self.pidfile.with_file_name("speech.log")))
                .env_remove("LD_PRELOAD");
            if !use_gpu {
                cmd.arg("-ng");
            }
            if let Some(dir) = bin.parent() {
                cmd.current_dir(dir);
            }
            child_hygiene(&mut cmd);
            let child = cmd.spawn().with_context(|| format!("failed to start {}", bin.display()))?;
            let _ = std::fs::write(&self.pidfile, child.id().to_string());
            *self.child.lock() = Some(child);
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(120) && self.alive() {
                if self.agent.get(&format!("http://127.0.0.1:{port}/")).call().map(|r| r.status() == 200).unwrap_or(false) {
                    *self.state.lock() = SpeechState::Ready {
                        model: model.trim_start_matches("ggml-").trim_end_matches(".bin").into(),
                        device: if use_gpu { "gpu".into() } else { "cpu".into() },
                    };
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(400));
            }
        }
        self.stop();
        let e = "the speech engine failed to start".to_string();
        *self.state.lock() = SpeechState::Unavailable { error: e.clone() };
        bail!(e)
    }

    pub fn stop(&self) {
        if let Some(mut c) = self.child.lock().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        let _ = std::fs::remove_file(&self.pidfile);
    }

    /// Transcribe a 16 kHz mono WAV. Segments shorter than a word or flagged as non-speech are dropped.
    pub fn transcribe(&self, wav: &[u8]) -> Result<Transcript> {
        let boundary = "----scrubless-boundary-7d1f";
        let mut body = Vec::with_capacity(wav.len() + 512);
        let field = |b: &mut Vec<u8>, name: &str, value: &str| {
            b.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes());
        };
        field(&mut body, "response_format", "verbose_json");
        field(&mut body, "temperature", "0.0");
        body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.wav\"\r\nContent-Type: audio/wav\r\n\r\n").as_bytes());
        body.extend_from_slice(wav);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let url = format!("http://127.0.0.1:{}/inference", *self.port.lock());
        let mut resp = self.agent.post(&url)
            .header("Content-Type", &format!("multipart/form-data; boundary={boundary}"))
            .send(&body[..])?;
        let status = resp.status();
        let v: Value = resp.body_mut().read_json().context("bad speech response")?;
        if status != 200 {
            bail!("speech request failed ({status}): {v}");
        }
        let segments = v["segments"].as_array().map(|a| a.as_slice()).unwrap_or(&[]).iter()
            .filter(|s| s["no_speech_prob"].as_f64().unwrap_or(0.0) < 0.8)
            .filter_map(|s| {
                let text = s["text"].as_str()?.trim().to_string();
                (text.chars().filter(|c| c.is_alphanumeric()).count() >= 2).then(|| Segment {
                    t0: s["start"].as_f64().unwrap_or(0.0),
                    t1: s["end"].as_f64().unwrap_or(0.0),
                    text,
                })
            })
            .collect();
        Ok(Transcript { language: v["language"].as_str().unwrap_or("").to_string(), segments })
    }
}

impl Drop for Speech {
    fn drop(&mut self) {
        self.stop();
    }
}
