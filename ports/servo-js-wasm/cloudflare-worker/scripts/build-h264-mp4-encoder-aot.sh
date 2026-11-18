#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
work_dir="$(mktemp -d)"
trap 'rm -rf "$work_dir"' EXIT
source_dir="$work_dir/h264-mp4-encoder"
build_dir="$work_dir/build"

git clone --depth 1 --branch v1.0.12 --recurse-submodules \
  https://github.com/TrevorSundberg/h264-mp4-encoder.git "$source_dir"

python3 - "$source_dir/CMakeLists.txt" <<'PY'
from pathlib import Path
import sys

cmake = Path(sys.argv[1])
source = cmake.read_text()
source = source.replace("set(CMAKE_CXX_STANDARD 14)", "set(CMAKE_CXX_STANDARD 17)")
anchor = "        -s SINGLE_FILE=0\\\n"
replacement = (
    "        -s ENVIRONMENT=web,worker\\\n"
    "        -s SINGLE_FILE=0\\\n"
)
if anchor not in source:
    raise SystemExit("Upstream CMakeLists.txt changed; review the encoder build settings.")
source = source.replace(anchor, replacement, 1)
anchor = "        -s EXPORT_NAME=H264MP4Module\\\n"
replacement = (
    "        -s EXPORT_NAME=H264MP4Module\\\n"
    "        -s DYNAMIC_EXECUTION=0\\\n"
    "        -s EMBIND_AOT=1\\\n"
)
if anchor not in source:
    raise SystemExit("Upstream CMakeLists.txt changed; review the encoder build settings.")
source = source.replace(anchor, replacement, 1)
cmake.write_text(source)
PY

emcmake cmake -S "$source_dir" -B "$build_dir"
cmake --build "$build_dir" --target h264-mp4-encoder --parallel "${JOBS:-2}"

if rg -q 'new Function\s*\(|eval\s*\(' "$build_dir/h264-mp4-encoder.js"; then
  echo "Generated encoder contains forbidden dynamic code generation." >&2
  exit 1
fi

cp "$build_dir/h264-mp4-encoder.js" "$repo_root/src/vendor/h264-mp4-encoder-aot.js"
cp "$build_dir/h264-mp4-encoder.wasm" "$repo_root/src/vendor/h264-mp4-encoder.wasm"
cp "$source_dir/LICENSE.md" "$repo_root/src/vendor/H264_MP4_ENCODER_LICENSE.md"
cp "$source_dir/generated/COPYING" "$repo_root/src/vendor/LIBMP4V2_MPL-1.1_LICENSE.txt"
cp "$source_dir/minih264/LICENSE" "$repo_root/src/vendor/MINIH264_CC0-1.0_LICENSE.txt"
