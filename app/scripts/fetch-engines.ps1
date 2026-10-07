# Windows counterpart of fetch-engines.sh: assembles src-tauri\engines\{llama,whisper,ffmpeg}.
# Requires the Vulkan SDK (VULKAN_SDK env) and Visual Studio build tools for whisper.cpp.
$ErrorActionPreference = "Stop"
$Llama = "b11461"; $Whisper = "v1.9.5"
$Root = Split-Path -Parent $PSScriptRoot
$Out = Join-Path $Root "src-tauri\engines"
$Tmp = Join-Path $env:RUNNER_TEMP "engines"
Remove-Item -Recurse -Force $Out, $Tmp -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force "$Out\llama", "$Out\whisper", "$Out\ffmpeg", $Tmp | Out-Null

Write-Host "== llama.cpp $Llama (Vulkan)"
Invoke-WebRequest "https://github.com/ggml-org/llama.cpp/releases/download/$Llama/llama-$Llama-bin-win-vulkan-x64.zip" -OutFile "$Tmp\llama.zip"
Expand-Archive "$Tmp\llama.zip" "$Tmp\llama"
$dir = (Get-ChildItem -Recurse "$Tmp\llama" -Filter llama-server.exe | Select-Object -First 1).DirectoryName
Copy-Item "$dir\llama-server.exe", "$dir\*.dll" "$Out\llama\"

Write-Host "== whisper.cpp $Whisper (Vulkan, built from source)"
git clone -q --depth 1 --branch $Whisper https://github.com/ggml-org/whisper.cpp.git "$Tmp\whisper"
cmake -S "$Tmp\whisper" -B "$Tmp\whisper\build" -DGGML_VULKAN=ON -DBUILD_SHARED_LIBS=OFF -DGGML_NATIVE=OFF -DWHISPER_BUILD_TESTS=OFF
cmake --build "$Tmp\whisper\build" --config Release --target whisper-server -j 4
Copy-Item (Get-ChildItem -Recurse "$Tmp\whisper\build" -Filter whisper-server.exe | Select-Object -First 1).FullName "$Out\whisper\"

Write-Host "== ffmpeg (LGPL)"
Invoke-WebRequest "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-n9.0-latest-win64-lgpl-9.0.zip" -OutFile "$Tmp\ffmpeg.zip"
Expand-Archive "$Tmp\ffmpeg.zip" "$Tmp\ffmpeg"
$bin = (Get-ChildItem -Recurse "$Tmp\ffmpeg" -Filter ffmpeg.exe | Select-Object -First 1).DirectoryName
Copy-Item "$bin\ffmpeg.exe", "$bin\ffprobe.exe" "$Out\ffmpeg\"
Get-ChildItem $Out -Recurse | Measure-Object -Property Length -Sum | ForEach-Object { "engines: {0:N0} MB" -f ($_.Sum / 1MB) }
