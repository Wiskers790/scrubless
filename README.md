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
