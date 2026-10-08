# Scrubless

Search your footage, photos and sounds by describing them, fully offline. A free alternative to
AI footage-search tools like Jumper, aimed at YouTubers and editors on DaVinci Resolve (free),
CapCut, Final Cut and Premiere.

- **Visual search:** describe a shot in any language ("drone shot over a beach at sunset",
  「猫が窓辺で寝ている」) and get the exact moment, with source timecode.
- **Speech search:** every word is transcribed locally (Whisper, 99 languages). Find exact quotes
  or what was meant ("when she talks about the budget"), including across languages.
- **Find similar:** drop a photo, clip or sound, or click any result.
- **Into your edit:** drag a moment into the NLE, or collect selects and export a timeline:
  FCPXML (Final Cut, Resolve), XML (Premiere), EDL (Avid), CSV, or rendered clips.
- **Watch folders:** new footage is indexed automatically. Works on CPU, fast on any GPU
  (Vulkan / Metal).

Measured quality, including where it is weak, is in [eval/RESULTS.md](eval/RESULTS.md).

## Download

> **First public release: week of October 13.** Watch or star the repo to get notified. The links below go live then.
> Meanwhile, the [benchmark results](eval/RESULTS.md) show exactly how well it works, and where it doesn't.

| Your computer | Download | |
|---|---|---|
| **Windows 10/11** | [**Scrubless-Windows-Setup.exe**](https://github.com/Wiskers790/scrubless/releases/latest/download/Scrubless-Windows-Setup.exe) | Installer (recommended) |
| | [Scrubless-Windows-Portable.zip](https://github.com/Wiskers790/scrubless/releases/latest/download/Scrubless-Windows-Portable.zip) | No install: unzip and run `Scrubless.exe` |
| **Mac (M1/M2/M3/M4)** | [**Scrubless-macOS-AppleSilicon.dmg**](https://github.com/Wiskers790/scrubless/releases/latest/download/Scrubless-macOS-AppleSilicon.dmg) | macOS 12 or newer |
| **Mac (Intel)** | [Scrubless-macOS-Intel.dmg](https://github.com/Wiskers790/scrubless/releases/latest/download/Scrubless-macOS-Intel.dmg) | macOS 12 or newer |
| **Linux** | [**Scrubless-Linux.AppImage**](https://github.com/Wiskers790/scrubless/releases/latest/download/Scrubless-Linux.AppImage) | Any 64-bit distro; or the [.deb](https://github.com/Wiskers790/scrubless/releases/latest/download/Scrubless-Linux.deb) |

All versions are on the [Releases page](https://github.com/Wiskers790/scrubless/releases).

## Setup (2 minutes)

**1. Install**

- **Windows:** run `Scrubless-Windows-Setup.exe`. If Windows shows *"Windows protected your PC"*,
  click **More info → Run anyway**. (The app isn't code-signed yet; that's the only reason for
  the warning.) Prefer no installer? Unzip the portable `.zip` anywhere and double-click
  `Scrubless.exe`.
- **Mac:** open the `.dmg` and drag **Scrubless** into **Applications**. The first time, macOS
  will refuse to open it (not signed yet). Open **System Settings → Privacy & Security**, scroll
  down and click **Open Anyway**. If it still won't start, run this once in Terminal:
  `xattr -dr com.apple.quarantine /Applications/Scrubless.app`
- **Linux:** make the AppImage executable and run it:
  `chmod +x Scrubless-Linux.AppImage && ./Scrubless-Linux.AppImage`
  (or `sudo apt install ./Scrubless-Linux.deb` on Debian/Ubuntu).

**2. First launch:** Scrubless downloads its search and speech models once (~1.4 GB). After
that it works fully offline.

**3. Add your footage:** click **+ Add** (or drag a folder onto the window). Indexing runs in the
background, and you can search as soon as the first files are done. New files in those folders
are picked up automatically.

**4. Search:** describe a shot, quote something someone said, or drop a photo or sound onto the
window. Press **+** on a result to collect it, then open **Selects → Send to** your editor.

**Requirements:** 64-bit Windows 10/11, macOS 12+, or Linux · 8 GB RAM · ~2 GB free disk for
models. A graphics card is optional: indexing is much faster with one (NVIDIA, AMD, Intel or Apple
Silicon), and everything still works on CPU only.

## Layout

| Path | What |
|---|---|
| `app/` | Tauri 2 desktop app: Rust backend (`src-tauri/src`) + Svelte 5 UI (`src/`) |
| `app/src-tauri/src/runtime.rs` | Embedding engine (llama.cpp `llama-server`, EmbeddingGemma 2): supervisor, GPU→CPU fallback, model download |
| `app/src-tauri/src/speech.rs` | Speech engine (whisper.cpp `whisper-server` + Silero VAD) |
| `app/src-tauri/src/index.rs` | Folder watching, shot detection, sound windows, transcription |
| `app/src-tauri/src/store.rs` | SQLite library, in-memory vectors, full-text transcript index |
| `app/src-tauri/src/export.rs` | FCPXML 1.8 / FCP7 XML / EDL / CSV timeline export |
| `app/scripts/fetch-engines.*` | Assemble pinned llama.cpp, whisper.cpp and ffmpeg builds for bundling |
| `eval/` | Benchmarks (MSR-VTT, FLEURS) and results |
| `.github/workflows/release.yml` | Builds installers for Windows, macOS and Linux |

## Develop

```bash
cd app && npm ci
scripts/fetch-engines.sh linux-x64   # or macos-arm64 / macos-x64; Windows: fetch-engines.ps1
npm run tauri dev
```

Models (~1.4 GB) download on first run into the app data folder.

## License

MIT. See [LICENSE](LICENSE). Bundled engines and models keep their own licenses; see
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

## Licenses of bundled parts

- llama.cpp and whisper.cpp: MIT
- EmbeddingGemma 2: Apache 2.0
- Whisper models: MIT
- ffmpeg: LGPL builds on Windows/Linux (BtbN). The macOS builds (martin-riedl.de) are GPL; they
  ship as separate executables, and a source offer is needed in the release notes.
