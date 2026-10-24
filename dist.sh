#!/usr/bin/env bash
# 打包 Linux + Windows 运行版本到 dist/（dist/ 不提交，见 .gitignore）。
# 依赖：rustup 目标 x86_64-unknown-linux-gnu / x86_64-pc-windows-gnu；
#       Windows 交叉编译需 zig（cargo-zigbuild），缺失时跳过并提示。
set -euo pipefail
cd "$(dirname "$0")"

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/agentlink-target}"
LINUX_T=x86_64-unknown-linux-gnu
WIN_T=x86_64-pc-windows-gnu
DIST=dist
VER=$(sed -n 's/^version = "\([^"]*\)".*/\1/p' Cargo.toml | head -1)
[ -n "$VER" ] || { echo "无法从 Cargo.toml 读取版本号"; exit 1; }
rm -rf "$DIST"
mkdir -p "$DIST"

echo "==> Linux ($LINUX_T)"
cargo build --release --target "$LINUX_T"
mkdir -p "$DIST/agentbuffet-linux-x86_64"
cp "$CARGO_TARGET_DIR/$LINUX_T/release/AgentBuffet" "$DIST/agentbuffet-linux-x86_64/"
cp README.md "$DIST/agentbuffet-linux-x86_64/" 2>/dev/null || true
tar czf "$DIST/agentbuffet-linux-x86_64-v$VER.tar.gz" -C "$DIST" agentbuffet-linux-x86_64

echo "==> Windows ($WIN_T)"
if command -v zig >/dev/null 2>&1 || [ -x /tmp/opencode/zig14/zig ]; then
  ZIGBIN="$(command -v zig || echo /tmp/opencode/zig14/zig)"
  PATH="$(dirname "$ZIGBIN"):$PATH" cargo zigbuild --release --target "$WIN_T"
  mkdir -p "$DIST/agentbuffet-windows-x86_64"
  cp "$CARGO_TARGET_DIR/$WIN_T/release/AgentBuffet.exe" "$DIST/agentbuffet-windows-x86_64/"
  cp README.md "$DIST/agentbuffet-windows-x86_64/" 2>/dev/null || true
  (cd "$DIST" && python3 -m zipfile -c "agentbuffet-windows-x86_64-v$VER.zip" agentbuffet-windows-x86_64)
else
  echo "跳过：未找到 zig（cargo-zigbuild 依赖）。安装 zig 后重跑本脚本即可产出 Windows 版。"
fi

echo "==> 产物："
ls -lh "$DIST"/*.tar.gz "$DIST"/*.zip 2>/dev/null || ls -l "$DIST"
