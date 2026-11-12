#!/usr/bin/env bash
# 从 SVG 正本重导整套位图图标：Chrome 无头渲染 1024px 源图，再由 Pillow 下采样。
# 用法：bash assets/logo/export-icons.sh   （可用 CHROME=... 覆盖浏览器路径）
set -e
CHROME="${CHROME:-/c/Program Files/Google/Chrome/Application/chrome.exe}"
ROOT="$(cd "$(dirname "$0")" && pwd)"
DST="$ROOT/favicon"
CACHE="$HOME/.cache/agentbuffet-logo"
PROFILE="$HOME/.cache/agentbuffet-logo-profile"
mkdir -p "$CACHE" "$PROFILE" "$DST"
# Chrome 的 --screenshot 只认 Windows 风格路径
WINCACHE="$(cygpath -w "$CACHE" | tr '\\' '/')"
WINPROFILE="$(cygpath -w "$PROFILE" | tr '\\' '/')"

render() {
  local out="$1" svg="$2" win
  win="file:///$(cygpath -w "$ROOT/$svg" | tr '\\' '/')"
  "$CHROME" --headless=new --disable-gpu --no-first-run --hide-scrollbars \
    --force-device-scale-factor=1 --default-background-color=00000000 \
    --user-data-dir="$WINPROFILE" --window-size=1024,1024 \
    --screenshot="$WINCACHE/$out" "$win" >/dev/null 2>&1
  mv -f "$CACHE/$out" "$DST/$out"
  echo "rendered $out"
}

render _src-tile.png favicon/favicon.svg
render _src-full.png agentbuffet-appicon-fullbleed.svg
render _src-mark.png agentbuffet-mark.svg
render _src-glow.png agentbuffet-mark-glow.svg
render _src-mono.png agentbuffet-mark-mono.svg

python "$DST/export-icons.py"
