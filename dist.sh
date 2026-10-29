#!/usr/bin/env bash
# 打包运行版本到 dist/（dist/ 不提交，见 .gitignore）。
#
# 策略：**优先打本机原生包**（一定成功），再按可用工具链追加交叉编译目标。
#   - Linux 主机 → linux-gnu（原生）+ linux-musl 静态（需 zig cc）
#   - Windows 主机 → windows-msvc（原生）+ linux-musl 静态（需 zig cc）
#   - macOS 主机 → macos（原生）
# 交叉编译目标缺少工具链时**跳过并提示**，不阻塞已成功的产物。
set -euo pipefail
cd "$(dirname "$0")"

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/agentbuffet-target}"
DIST=dist
VER=$(sed -n 's/^version = "\([^"]*\)".*/\1/p' Cargo.toml | head -1)
[ -n "$VER" ] || { echo "无法从 Cargo.toml 读取版本号"; exit 1; }

HOST_OS=$(uname -s)
[ "$HOST_OS" = "MINGW64_NT-"* ] || [ "$HOST_OS" = "MSYS_NT-"* ] || [ "$HOST_OS" = "CYGWIN_NT-"* ] && HOST_OS=windows || true

# 找 zig：PATH → /tmp/opencode/zig14/zig → python ziglang 包
ZIGBIN=""
for cand in "$(command -v zig 2>/dev/null || true)" /tmp/opencode/zig14/zig; do
  [ -n "$cand" ] && [ -x "$cand" ] && { ZIGBIN="$cand"; break; }
done
if [ -z "$ZIGBIN" ]; then
  PYZIG=$(python3 -c "import ziglang,os;print(os.path.join(os.path.dirname(ziglang.__file__),'zig'))" 2>/dev/null || true)
  [ -n "$PYZIG" ] && [ -x "$PYZIG" ] && ZIGBIN="$PYZIG"
fi

rm -rf "$DIST"
mkdir -p "$DIST"
SKIPPED=()

# pack <dir> <产物名> — 复制二进制与文档并压缩
pack() {
  local dir="$1" name="$2" ext="$3"
  mkdir -p "$DIST/$dir"
  cp README.md "$DIST/$dir/" 2>/dev/null || true
  cp README.zh-CN.md "$DIST/$dir/" 2>/dev/null || true
  cp LICENSE "$DIST/$dir/" 2>/dev/null || true
  if [ "$ext" = zip ]; then
    (cd "$DIST" && python3 -m zipfile -c "$name-v$VER.zip" "$dir")
  else
    tar czf "$DIST/$name-v$VER.tar.gz" -C "$DIST" "$dir"
  fi
}

echo "==> 主机: $HOST_OS    版本: $VER    zig: ${ZIGBIN:-（无）}"

# ---------- 本机原生包 ----------
case "$HOST_OS" in
  Linux)
    echo "==> Linux 原生 (x86_64-unknown-linux-gnu)"
    cargo build --release --target x86_64-unknown-linux-gnu
    cp "$CARGO_TARGET_DIR/x86_64-unknown-linux-gnu/release/AgentBuffet" "$DIST/" 2>/dev/null || true
    mkdir -p "$DIST/agentbuffet-linux-x86_64"
    cp "$CARGO_TARGET_DIR/x86_64-unknown-linux-gnu/release/AgentBuffet" "$DIST/agentbuffet-linux-x86_64/"
    pack agentbuffet-linux-x86_64 agentbuffet-linux-x86_64 tar.gz
    ;;
  windows)
    echo "==> Windows 原生 (x86_64-pc-windows-msvc)"
    cargo build --release --target x86_64-pc-windows-msvc
    mkdir -p "$DIST/agentbuffet-windows-x86_64"
    cp "$CARGO_TARGET_DIR/x86_64-pc-windows-msvc/release/AgentBuffet.exe" "$DIST/agentbuffet-windows-x86_64/"
    pack agentbuffet-windows-x86_64 agentbuffet-windows-x86_64 zip
    ;;
  Darwin)
    echo "==> macOS 原生 (x86_64-apple-darwin)"
    cargo build --release --target x86_64-apple-darwin
    mkdir -p "$DIST/agentbuffet-macos-x86_64"
    cp "$CARGO_TARGET_DIR/x86_64-apple-darwin/release/AgentBuffet" "$DIST/agentbuffet-macos-x86_64/"
    pack agentbuffet-macos-x86_64 agentbuffet-macos-x86_64 tar.gz
    ;;
esac

# ---------- Linux 静态版（musl，需要 zig cc 交叉） ----------
if [ -n "$ZIGBIN" ]; then
  echo "==> Linux 静态 (x86_64-unknown-linux-musl, zig cc)"
  ZIGDIR="$(dirname "$ZIGBIN")"
  export PATH="$ZIGDIR:$PATH"
  export CC_x86_64_unknown_linux_musl="$ZIGBIN cc"
  export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER="$ZIGBIN cc"
  export CFLAGS_x86_64_unknown_linux_musl="--target=x86_64-linux-musl"
  if cargo build --release --target x86_64-unknown-linux-musl; then
    mkdir -p "$DIST/agentbuffet-linux-x86_64-static"
    cp "$CARGO_TARGET_DIR/x86_64-unknown-linux-musl/release/AgentBuffet" "$DIST/agentbuffet-linux-x86_64-static/"
    pack agentbuffet-linux-x86_64-static agentbuffet-linux-x86_64-static tar.gz
  else
    SKIPPED+=("linux-musl 静态（交叉编译失败）")
  fi
else
  SKIPPED+=("linux-musl 静态（缺 zig cc）")
fi

# ---------- Windows 交叉版（GNU，msvc 主机上作为可选补充） ----------
if [ "$HOST_OS" != "windows" ] && [ -n "$ZIGBIN" ]; then
  echo "==> Windows 交叉 (x86_64-pc-windows-gnu, zig cc)"
  ZIGDIR="$(dirname "$ZIGBIN")"
  export PATH="$ZIGDIR:$PATH"
  export CC_x86_64_pc_windows_gnu="$ZIGBIN cc"
  export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER="$ZIGBIN cc"
  if cargo build --release --target x86_64-pc-windows-gnu; then
    mkdir -p "$DIST/agentbuffet-windows-x86_64-gnu"
    cp "$CARGO_TARGET_DIR/x86_64-pc-windows-gnu/release/AgentBuffet.exe" "$DIST/agentbuffet-windows-x86_64-gnu/"
    pack agentbuffet-windows-x86_64-gnu agentbuffet-windows-x86_64-gnu zip
  else
    SKIPPED+=("windows-gnu 交叉（交叉编译失败）")
  fi
fi

echo
echo "==> 产物："
ls -lh "$DIST"/*.tar.gz "$DIST"/*.zip 2>/dev/null || ls -l "$DIST"
if [ ${#SKIPPED[@]} -gt 0 ]; then
  echo
  echo "==> 跳过："
  for s in "${SKIPPED[@]}"; do echo "    - $s"; done
fi
