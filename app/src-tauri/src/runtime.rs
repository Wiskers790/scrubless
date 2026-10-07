//! The embedding runtime: a llama-server child process serving EmbeddingGemma 2.
//!
//! Lookup order for the binary: $SCRUBLESS_LLAMA_SERVER, a sidecar next to our executable, then PATH.
//! Models live in <data>/models (override with $SCRUBLESS_MODEL_DIR) and are downloaded on first run.
//! GPU is tried first; if the server dies or never becomes healthy we restart it on CPU.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const MODEL_FILES: [(&str, &str); 2] = [
    ("embeddinggemma-2-Q8_0.gguf", "https://huggingface.co/ggml-org/embeddinggemma-2-GGUF/resolve/main/embeddinggemma-2-Q8_0.gguf"),
    ("mmproj-embeddinggemma-2-Q8_0.gguf", "https://huggingface.co/ggml-org/embeddinggemma-2-GGUF/resolve/main/mmproj-embeddinggemma-2-Q8_0.gguf"),
];
pub const DIM: usize = 768;

#[derive(Clone, Serialize, Debug, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RuntimeState {
    NeedsModel,
    Downloading { file: String, done: u64, total: u64 },
    Starting { device: String },
    Ready { device: String },
    Failed { error: String },
}

pub struct Runtime {
    pub model_dir: PathBuf,
    /// user setting: never use the GPU (shared GPUs, driver trouble)
    pub force_cpu: std::sync::atomic::AtomicBool,
    pidfile: PathBuf,
    server_bin: Option<PathBuf>,
    child: Mutex<Option<Child>>,
    port: Mutex<u16>,
    pub state: Mutex<RuntimeState>,
    agent: ureq::Agent,
}

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(Path::to_path_buf)
}

/// Locate a bundled helper binary: $<env>, then `<exe dir>/<subdir>/<name>` (each engine has its
/// own folder because llama.cpp and whisper.cpp ship different builds of the same ggml libraries),
/// then a Tauri sidecar next to the executable, then PATH.
/// The bundle's resource directory (set once at startup from Tauri's path resolver).
pub static RESOURCE_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

pub fn find_bin(name: &str, env: &str, subdir: &str) -> Option<PathBuf> {
    if let Ok(p) = std::env::var(env) {
        return Some(PathBuf::from(p));
    }
    let exe = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
    if let Some(res) = RESOURCE_DIR.get() {
        let p = res.join("engines").join(subdir).join(&exe);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Some(dir) = exe_dir() {
        for base in [dir.join(subdir), dir.join("resources").join(subdir), dir.join("../Resources").join(subdir)] {
            if base.join(&exe).is_file() {
                return Some(base.join(&exe));
            }
        }
        // Tauri sidecars are renamed `<name>-<target triple>` in dev and `<name>` when bundled.
        for cand in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let f = cand.file_name().to_string_lossy().to_string();
            if f == exe || (f.starts_with(&format!("{name}-")) && !f.ends_with(".d")) {
                return Some(cand.path());
            }
        }
    }
    which(&exe)
}

pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| p.is_file())
}

pub fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").and_then(|l| l.local_addr()).map(|a| a.port()).unwrap_or(8095)
}

impl Runtime {
    pub fn new(data_dir: &Path) -> Arc<Self> {
        let model_dir = std::env::var("SCRUBLESS_MODEL_DIR").map(PathBuf::from).unwrap_or_else(|_| data_dir.join("models"));
        let _ = std::fs::create_dir_all(&model_dir);
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(600)))
            .http_status_as_error(false)
            .build()
            .into();
        let pidfile = data_dir.join("engine.pid");
        kill_stale(&pidfile, "llama-server");
        let rt = Arc::new(Self {
            model_dir,
            force_cpu: std::sync::atomic::AtomicBool::new(std::env::var("SCRUBLESS_CPU").is_ok()),
            pidfile,
            server_bin: find_bin("llama-server", "SCRUBLESS_LLAMA_SERVER", "llama"),
            child: Mutex::new(None),
            port: Mutex::new(0),
            state: Mutex::new(RuntimeState::NeedsModel),
            agent,
        });
        if rt.models_present() {
            *rt.state.lock() = RuntimeState::Starting { device: "gpu".into() };
        }
        rt
    }

    pub fn models_present(&self) -> bool {
        MODEL_FILES.iter().all(|(f, _)| self.model_dir.join(f).metadata().map(|m| m.len() > 1_000_000).unwrap_or(false))
    }

    /// Download any missing model file, reporting progress through `state`.
    pub fn download_models(&self) -> Result<()> {
        for (file, url) in MODEL_FILES {
            download_file(&self.agent, url, &self.model_dir.join(file), |done, total| {
                *self.state.lock() = RuntimeState::Downloading { file: file.into(), done, total };
            })?;
        }
        Ok(())
    }

    fn spawn(&self, gpu: bool) -> Result<Child> {
        let bin = self.server_bin.clone().ok_or_else(|| anyhow!("llama-server not found (set SCRUBLESS_LLAMA_SERVER)"))?;
        let port = free_port();
        *self.port.lock() = port;
        let mut cmd = Command::new(&bin);
        cmd.arg("--model").arg(self.model_dir.join(MODEL_FILES[0].0))
            .arg("--mmproj").arg(self.model_dir.join(MODEL_FILES[1].0))
            .args(["--embeddings", "--ctx-size", "8192", "--batch-size", "8192", "--ubatch-size", "8192"])
            .args(["--parallel", "4", "--host", "127.0.0.1", "--port", &port.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log_file(&self.pidfile.with_file_name("engine.log")));
        if gpu {
            cmd.args(["-ngl", "99"]);
        } else {
            cmd.args(["-ngl", "0", "--no-mmproj-offload", "--device", "none"]);
        }
        // Some desktops preload libraries that break GPU drivers inside child processes.
        cmd.env_remove("LD_PRELOAD");
        if let Some(dir) = bin.parent() {
            cmd.current_dir(dir);
        }
        child_hygiene(&mut cmd);
        let child = cmd.spawn().with_context(|| format!("failed to start {}", bin.display()))?;
        let _ = std::fs::write(&self.pidfile, child.id().to_string());
        Ok(child)
    }

    fn wait_healthy(&self, timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if let Some(c) = self.child.lock().as_mut() {
                if let Ok(Some(_)) = c.try_wait() {
                    return false; // exited
                }
            }
            let url = format!("http://127.0.0.1:{}/health", *self.port.lock());
            if let Ok(r) = self.agent.get(&url).call() {
                if r.status() == 200 {
                    return true;
                }
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        false
    }

    /// Start the server (GPU first, then CPU) and warm it up. Blocking.
    pub fn start(&self) -> Result<()> {
        if !self.models_present() {
            self.download_models()?;
        }
        let force_cpu = self.force_cpu.load(std::sync::atomic::Ordering::Relaxed);
        for gpu in [true, false] {
            if gpu && force_cpu {
                continue;
            }
            let device = if gpu { "gpu" } else { "cpu" };
            *self.state.lock() = RuntimeState::Starting { device: device.into() };
            self.stop();
            *self.child.lock() = Some(self.spawn(gpu)?);
            if self.wait_healthy(Duration::from_secs(90)) && self.warm_up().is_ok() {
                *self.state.lock() = RuntimeState::Ready { device: device.into() };
                return Ok(());
            }
        }
        self.stop();
        let err = "the embedding engine failed to start on GPU and CPU".to_string();
        *self.state.lock() = RuntimeState::Failed { error: err.clone() };
        bail!(err)
    }

    /// First requests on a GPU compile kernels; do it before the user's first search.
    fn warm_up(&self) -> Result<()> {
        self.embed_text("warm up")?;
        // best effort: a failure here must not push us onto the CPU path
        if let Ok(jpeg) = base64::engine::general_purpose::STANDARD.decode(TINY_JPEG_B64) {
            let _ = self.embed_image(&jpeg);
        }
        Ok(())
    }

    /// Own the engine for the life of the app: start it, then restart it if it ever dies. Must run
    /// on a thread that never exits, because Linux PDEATHSIG fires when the *spawning thread* ends.
    pub fn supervise(self: Arc<Self>) {
        let mut failures = 0u32;
        loop {
            if !self.ready() || !self.alive() {
                match self.start() {
                    Ok(()) => failures = 0,
                    Err(err) => {
                        failures += 1;
                        eprintln!("engine: {err:#}");
                        *self.state.lock() = RuntimeState::Failed { error: format!("{err:#}") };
                        std::thread::sleep(Duration::from_secs((5 * failures as u64).min(60)));
                        continue;
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    }

    fn alive(&self) -> bool {
        match self.child.lock().as_mut() {
            Some(c) => matches!(c.try_wait(), Ok(None)),
            None => false,
        }
    }

    pub fn stop(&self) {
        if let Some(mut c) = self.child.lock().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        let _ = std::fs::remove_file(&self.pidfile);
    }

    pub fn ready(&self) -> bool {
        matches!(*self.state.lock(), RuntimeState::Ready { .. }) && self.alive()
    }

    fn post(&self, inputs: Vec<Value>) -> Result<Vec<Vec<f32>>> {
        let url = format!("http://127.0.0.1:{}/v1/embeddings", *self.port.lock());
        let mut resp = self.agent.post(&url).send_json(json!({ "input": inputs }))?;
        let status = resp.status();
        let body: Value = resp.body_mut().read_json()?;
        if status != 200 {
            bail!("embedding request failed ({status}): {body}");
        }
        let data = body["data"].as_array().ok_or_else(|| anyhow!("bad response"))?;
        data.iter()
            .map(|d| {
                let v: Vec<f32> = d["embedding"].as_array().ok_or_else(|| anyhow!("no embedding"))?
                    .iter().map(|x| x.as_f64().unwrap_or(0.0) as f32).collect();
                Ok(normalize(v))
            })
            .collect()
    }

    /// Queries use the model's retrieval prompt; documents (media) are embedded as-is.
    pub fn embed_query(&self, q: &str) -> Result<Vec<f32>> {
        Ok(self.post(vec![json!(format!("task: search result | query: {q}"))])?.remove(0))
    }

    pub fn embed_text(&self, t: &str) -> Result<Vec<f32>> {
        Ok(self.post(vec![json!(t)])?.remove(0))
    }

    pub fn embed_image(&self, jpeg: &[u8]) -> Result<Vec<f32>> {
        let url = format!("data:image/jpeg;base64,{}", base64::engine::general_purpose::STANDARD.encode(jpeg));
        Ok(self.post(vec![json!({"content": [{"type": "image_url", "image_url": {"url": url}}]})])?.remove(0))
    }

    pub fn embed_audio(&self, wav: &[u8]) -> Result<Vec<f32>> {
        let data = base64::engine::general_purpose::STANDARD.encode(wav);
        Ok(self.post(vec![json!({"content": [{"type": "input_audio", "input_audio": {"data": data, "format": "wav"}}]})])?.remove(0))
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Download `url` to `dest` (via a .part file) unless it already exists, reporting progress.
pub fn download_file(agent: &ureq::Agent, url: &str, dest: &Path, progress: impl Fn(u64, u64)) -> Result<()> {
    if dest.exists() {
        return Ok(());
    }
    let part = dest.with_extension("part");
    let resp = agent.get(url).call().context("download request failed")?;
    if resp.status() != 200 {
        bail!("download of {} failed: HTTP {}", dest.display(), resp.status());
    }
    let total: u64 = resp.headers().get("content-length").and_then(|v| v.to_str().ok()?.parse().ok()).unwrap_or(0);
    let mut reader = resp.into_body().into_reader();
    let mut out = std::fs::File::create(&part)?;
    let mut buf = vec![0u8; 1 << 20];
    let (mut done, mut last) = (0u64, Instant::now());
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        done += n as u64;
        if last.elapsed() > Duration::from_millis(200) {
            progress(done, total);
            last = Instant::now();
        }
    }
    out.sync_all()?;
    std::fs::rename(&part, dest)?;
    Ok(())
}

/// Helper output goes to a log in the data dir (overwritten each start) for diagnosing failures.
pub fn log_file(path: &Path) -> Stdio {
    std::fs::File::create(path).map(Stdio::from).unwrap_or_else(|_| Stdio::null())
}

/// Make a helper process die with us. On Linux the kernel enforces it (PDEATHSIG, so spawn only
/// from threads that live as long as the app); elsewhere the pidfile cleanup in `kill_stale`
/// catches servers orphaned by a crash or force-quit.
pub fn child_hygiene(cmd: &mut Command) {
    #[cfg(target_os = "linux")]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            Ok(())
        });
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
}

/// Kill an engine left behind by a previous run (crash, force-quit), if it is still ours.
pub fn kill_stale(pidfile: &Path, exe_name: &str) {
    let Some(pid) = std::fs::read_to_string(pidfile).ok().and_then(|s| s.trim().parse::<u32>().ok()) else { return };
    #[cfg(unix)]
    {
        let name = Command::new("ps").args(["-p", &pid.to_string(), "-o", "comm="]).output().ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
        if name.contains(exe_name) {
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        }
    }
    #[cfg(windows)]
    {
        let out = Command::new("tasklist").args(["/FI", &format!("PID eq {pid}"), "/NH"]).output();
        if out.map(|o| String::from_utf8_lossy(&o.stdout).contains(exe_name)).unwrap_or(false) {
            let _ = Command::new("taskkill").args(["/F", "/PID", &pid.to_string()]).status();
        }
    }
    let _ = std::fs::remove_file(pidfile);
}

pub fn normalize(mut v: Vec<f32>) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
    v.iter_mut().for_each(|x| *x /= n);
    v
}

// 8x8 grey JPEG, used to warm up the vision path.
const TINY_JPEG_B64: &str = "/9j/4AAQSkZJRgABAQEASABIAAD/2wBDAP//////////////////////////////////////////////////////////////////////////////////////wgALCAAIAAgBAREA/8QAFAABAAAAAAAAAAAAAAAAAAAAAv/aAAgBAQAAAAE//8QAFBABAAAAAAAAAAAAAAAAAAAAAP/aAAgBAQABBQJ//8QAFBABAAAAAAAAAAAAAAAAAAAAAP/aAAgBAQAGPwJ//8QAFBABAAAAAAAAAAAAAAAAAAAAAP/aAAgBAQABPyF//9oACAEBAAAAEH//xAAUEAEAAAAAAAAAAAAAAAAAAAAA/9oACAEBAAE/EH//2Q==";
