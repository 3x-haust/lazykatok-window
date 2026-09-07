#!/usr/bin/env bash
set -euo pipefail

LAZYKATOK_BIN="${LAZYKATOK_BIN:-lazykatok}"

if ! command -v "$LAZYKATOK_BIN" >/dev/null 2>&1; then
  if [ -x "target/debug/lazykatok" ]; then
    LAZYKATOK_BIN="target/debug/lazykatok"
  else
    echo "lazykatok binary not found. Run cargo install --git https://github.com/changeroa/lazykatok, or set LAZYKATOK_BIN=/path/to/lazykatok." >&2
    exit 127
  fi
fi

echo "Opening macOS permission settings..."
"$LAZYKATOK_BIN" permissions macos --accessibility
echo "Enable your terminal app for Full Disk Access. Enable Accessibility too if you plan to use KakaoTalk UI automation, then press Enter."
read -r _

echo "Checking KakaoTalk readiness..."
"$LAZYKATOK_BIN" doctor --macos-probe --json

echo "Syncing live macOS KakaoTalk archive..."
"$LAZYKATOK_BIN" sync --source macos --json

echo "Building local semantic index with EmbeddingGemma..."
"$LAZYKATOK_BIN" index --json

echo "Running semantic smoke search..."
"$LAZYKATOK_BIN" search semantic "최근 대화" --json
