# Third-party components

Scrubless bundles or downloads the following. Each keeps its own license.

| Component | Use | License | Source |
|---|---|---|---|
| llama.cpp (`llama-server`) | Embedding engine | MIT | https://github.com/ggml-org/llama.cpp |
| whisper.cpp (`whisper-server`) | Speech-to-text engine | MIT | https://github.com/ggml-org/whisper.cpp |
| EmbeddingGemma 2 (GGUF, downloaded on first run) | Search model | Apache 2.0 | https://huggingface.co/google/embeddinggemma-2 |
| Whisper large-v3-turbo / small (GGML, downloaded on first run) | Speech model | MIT | https://github.com/openai/whisper |
| Silero VAD (GGML, downloaded on first run) | Voice activity detection | MIT | https://github.com/snakers4/silero-vad |
| FFmpeg (Windows, Linux: BtbN LGPL builds) | Decoding, clips | LGPL 2.1+ | https://github.com/BtbN/FFmpeg-Builds, source: https://ffmpeg.org/download.html |
| FFmpeg (macOS: martin-riedl.de builds) | Decoding, clips | GPL 2+ | https://ffmpeg.martin-riedl.de, source: https://ffmpeg.org/download.html |
| OpenH264 (inside FFmpeg) | H.264 encoding | BSD 2-Clause | https://github.com/cisco/openh264 |
| Tauri and its plugins | App shell | MIT / Apache 2.0 | https://tauri.app |

FFmpeg runs as a separate program. Its source code, matching the bundled version, is available
from the links above. Scrubless itself does not link against FFmpeg.
