#!/usr/bin/env bash
# Assemble the helper engines bundled with the app into src-tauri/engines/{llama,whisper,ffmpeg}.
#   scripts/fetch-engines.sh linux-x64 | macos-arm64 | macos-x64
# Windows uses scripts/fetch-engines.ps1. Versions are pinned so builds are reproducible.
set -euo pipefail
PLATFORM="${1:?platform}"
LLAMA=b11461          # llama.cpp: EmbeddingGemma 2 (text + vision + audio) support landed in b11457
WHISPER=v1.9.5        # whisper.cpp: server + Silero VAD
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/src-tauri/engines"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
rm -rf "$OUT" && mkdir -p "$OUT"/{llama,whisper,ffmpeg}

case "$PLATFORM" in
  linux-x64)   LLAMA_ASSET="llama-$LLAMA-bin-ubuntu-vulkan-x64.tar.gz"; WHISPER_GPU="-DGGML_VULKAN=ON" ;;
  macos-arm64) LLAMA_ASSET="llama-$LLAMA-bin-macos-arm64.tar.gz";       WHISPER_GPU="-DGGML_METAL=ON -DGGML_METAL_EMBED_LIBRARY=ON" ;;
  macos-x64)   LLAMA_ASSET="llama-$LLAMA-bin-macos-x64.tar.gz";         WHISPER_GPU="-DGGML_METAL=OFF" ;;
  *) echo "unknown platform $PLATFORM" >&2; exit 1 ;;
esac

echo "== llama.cpp $LLAMA ($LLAMA_ASSET)"
curl -fsSL "https://github.com/ggml-org/llama.cpp/releases/download/$LLAMA/$LLAMA_ASSET" | tar xz -C "$TMP"
dir="$(dirname "$(find "$TMP" -name llama-server -type f | head -1)")"
cp -a "$dir"/llama-server "$dir"/*.so* "$dir"/*.dylib "$OUT/llama/" 2>/dev/null || true
cp -a "$dir"/*.metal "$OUT/llama/" 2>/dev/null || true
rm -f "$OUT"/llama/libllama-{batched-bench,bench,cli,completion,fit-params,perplexity,quantize}-impl.* "$OUT"/llama/libggml-rpc.*

echo "== whisper.cpp $WHISPER (built from source)"
PREFIX="$TMP/prefix"
if [ "$PLATFORM" = linux-x64 ]; then
  # ggml's Vulkan backend needs SPIRV-Headers and Vulkan-Headers as CMake packages; distro and
  # CI images rarely ship them, so build them into a private prefix
  for repo in SPIRV-Headers Vulkan-Headers; do
    git clone -q --depth 1 "https://github.com/KhronosGroup/$repo.git" "$TMP/$repo"
    cmake -S "$TMP/$repo" -B "$TMP/$repo/build" -DCMAKE_INSTALL_PREFIX="$PREFIX" -DSPIRV_HEADERS_ENABLE_TESTS=OFF > /dev/null
    cmake --install "$TMP/$repo/build" > /dev/null
  done
fi
git clone -q --depth 1 --branch "$WHISPER" https://github.com/ggml-org/whisper.cpp.git "$TMP/whisper"
# shellcheck disable=SC2086
cmake -S "$TMP/whisper" -B "$TMP/whisper/build" -DCMAKE_BUILD_TYPE=Release -DWHISPER_BUILD_TESTS=OFF \
      -DCMAKE_PREFIX_PATH="$PREFIX" -DBUILD_SHARED_LIBS=OFF -DGGML_NATIVE=OFF $WHISPER_GPU > /dev/null
cmake --build "$TMP/whisper/build" -j"$(getconf _NPROCESSORS_ONLN)" --target whisper-server > /dev/null
cp "$TMP/whisper/build/bin/whisper-server" "$OUT/whisper/"

echo "== ffmpeg"
case "$PLATFORM" in
  linux-x64)
    curl -fsSL https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-n9.0-latest-linux64-lgpl-9.0.tar.xz | tar xJ -C "$TMP"
    cp "$TMP"/ffmpeg-n9.0-*/bin/ffmpeg "$TMP"/ffmpeg-n9.0-*/bin/ffprobe "$OUT/ffmpeg/" ;;
  macos-*)
    arch="${PLATFORM#macos-}"; [ "$arch" = x64 ] && arch=amd64
    for t in ffmpeg ffprobe; do
      curl -fsSL -o "$TMP/$t.zip" "https://ffmpeg.martin-riedl.de/redirect/latest/macos/$arch/release/$t.zip"
      unzip -oq "$TMP/$t.zip" -d "$OUT/ffmpeg/"
    done ;;
esac
chmod +x "$OUT"/*/* 2>/dev/null || true
du -sh "$OUT"/*
