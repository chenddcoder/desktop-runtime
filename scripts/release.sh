#!/usr/bin/env bash
#
# release.sh —— 投屏显象 desktop-runtime 一键构建+发布
#
# 构建链：
#   1. web-runtime dist（quicktvui monorepo）→ 部署到 runtime.chenddcoder.cn
#   2. desktop universal 双架构包（rustup cargo）→ 部署到官网 mac 下载
#
# 用法：
#   ./release.sh                  # 全量：bump 版本 + 构建 web + 桌面，并发布到服务器
#   ./release.sh --no-upload     # 只本地构建，不上传（自测用）
#   ./release.sh --web-only      # 只构建+发布 web-runtime
#   ./release.sh --desktop-only  # 只构建+发布桌面包
#   ./release.sh --prod          # web-runtime 用生产构建（去 console），默认 build:dev（保留日志）
#   ./release.sh --skip-web      # 桌面构建跳过 web-runtime 重建（dist 已是最新时用）
#   ./release.sh --no-bump       # 跳过版本号自动更新
#   ./release.sh --version 0.2.0 # 显式指定新版本号（默认 patch 段 +1）
#
# 环境变量（可选覆盖）：
#   WEB_RUNTIME_DIR    web-runtime 源码目录（默认 ../quicktvui/packages/web-runtime）
#   SERVER             服务器（默认 root@chenddcoder.cn）
#   NODE               node/npx 路径（默认 npx，需 PATH 含 node20）
#   CARGO_TARGET_DIR   cargo 构建输出目录（默认 src-tauri/target；
#                      项目卷沙箱可能拦截 target 内文件操作，可指到 ~/ 下绕开）
#
set -euo pipefail

# ============ 路径与常量 ============
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DESKTOP_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
SRC_TAURI_DIR="$DESKTOP_DIR/src-tauri"
WEB_RUNTIME_DIR="${WEB_RUNTIME_DIR:-$DESKTOP_DIR/../quicktvui/packages/web-runtime}"

APP_NAME="投屏显象"
SERVER="${SERVER:-root@chenddcoder.cn}"
WEB_REMOTE_DIR="/opt/www/web-runtime"
DESKTOP_REMOTE_DIR="/opt/www/vue-ai/downloads"
DESKTOP_ZIP="QuickAppDesktop-macos-universal.zip"

# ⚠️ 必须用 rustup 的 cargo（系统 /usr/local/bin 的 cargo 缺 x86_64 std，universal 编不过）
export PATH="$HOME/.cargo/bin:$PATH"
RUSTUP_CARGO="$HOME/.cargo/bin/cargo"
NODE_BIN="${NODE:-npx}"
# cargo 构建输出目录（支持外部覆盖；tauri build 会透传给 cargo，bundle 输出在同目录下）
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$SRC_TAURI_DIR/target}"

# ============ 参数解析 ============
DO_UPLOAD=1
DO_WEB=1
DO_DESKTOP=1
WEB_MODE="build:dev"   # 默认保留日志，便于线上排障
SKIP_WEB=0
NO_BUMP=0
VERSION_OVERRIDE=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --no-upload)   DO_UPLOAD=0 ;;
    --web-only)    DO_DESKTOP=0 ;;
    --desktop-only) DO_WEB=0 ;;
    --skip-web)    SKIP_WEB=1 ;;
    --prod)        WEB_MODE="build" ;;
    --no-bump)     NO_BUMP=1 ;;
    --version)     VERSION_OVERRIDE="$2"; shift ;;
    -h|--help)     sed -n '3,24p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "未知参数: $1"; exit 1 ;;
  esac
  shift
done

log() { echo "==> $*"; }
die() { echo "✗ $*" >&2; exit 1; }

# ============ 0. 版本号自动更新 ============
# 同步更新 tauri.conf.json / Cargo.toml / package.json 三处版本（scripts/bump_version.mjs）。
# 默认 patch 段 +1；--version x.y.z 显式指定；--no-bump 跳过。
# Cargo.lock 的本地包版本由 cargo build 自动同步，无需手动处理。
bump_version() {
  [[ "$NO_BUMP" == 1 ]] && { log "跳过版本号更新（--no-bump）"; return; }
  log "更新版本号"
  NEW_VERSION="$(node "$SCRIPT_DIR/bump_version.mjs" "$DESKTOP_DIR" "$VERSION_OVERRIDE")" || die "版本号更新失败"
  log "版本号: $NEW_VERSION"
}

# ============ 1. web-runtime 构建 ============
build_web() {
  [[ "$DO_WEB" != 1 || "$SKIP_WEB" == 1 ]] && return
  log "构建 web-runtime dist（mode=$WEB_MODE）"
  [[ -d "$WEB_RUNTIME_DIR" ]] || die "web-runtime 目录不存在: $WEB_RUNTIME_DIR"
  cd "$WEB_RUNTIME_DIR"
  npm run "$WEB_MODE" >/dev/null
  # build:dev 不会自动拷贝 wasm，兜底补一份（build 模式自带）
  cp ../wasm-crypto/dist/wasm_crypto_bg.wasm dist/ 2>/dev/null || true
  [[ -f dist/index.html ]] || die "web-runtime 构建失败：dist/index.html 缺失"
  WEB_HASH="$(grep -o 'web-runtime\.[a-f0-9]*\.js' dist/index.html | head -1 | sed 's/web-runtime\.//;s/\.js//')"
  log "web-runtime 新主入口: web-runtime.$WEB_HASH.js"
}

# ============ 2. desktop universal 构建 ============
build_desktop() {
  [[ "$DO_DESKTOP" != 1 ]] && return
  log "检查 rustup 工具链与 x86_64 target"
  command -v "$RUSTUP_CARGO" >/dev/null 2>&1 || die "找不到 rustup cargo: $RUSTUP_CARGO（请用 rustup 安装）"
  if ! rustup target list --installed 2>/dev/null | grep -q 'x86_64-apple-darwin'; then
    log "安装 x86_64-apple-darwin target"
    rustup target add x86_64-apple-darwin
  fi

  log "touch build.rs 强制重嵌最新 dist"
  touch "$SRC_TAURI_DIR/build.rs"

  log "tauri build --target universal-apple-darwin --bundles app"
  # ⚠️ 只打 app（官网发布走 zip）；dmg 用 bundle_dmg.sh 偶发失败会阻断流程，故跳过。
  #    如需 dmg 手动：PATH="$HOME/.cargo/bin:$PATH" npx tauri build --target universal-apple-darwin --bundles dmg
  cd "$SRC_TAURI_DIR"
  PATH="$HOME/.cargo/bin:$PATH" "$NODE_BIN" tauri build --target universal-apple-darwin --bundles app

  APP_PATH="$CARGO_TARGET_DIR/universal-apple-darwin/release/bundle/macos/$APP_NAME.app"
  [[ -d "$APP_PATH" ]] || die "universal 包未生成: $APP_PATH"
  log "校验双架构: $(file "$APP_PATH/Contents/MacOS/quickapp-desktop" | head -1)"
}

# ============ 3. 打包 zip ============
pack_zip() {
  [[ "$DO_DESKTOP" != 1 ]] && return
  MACOS_DIR="$CARGO_TARGET_DIR/universal-apple-darwin/release/bundle/macos"
  log "打包 $DESKTOP_ZIP"
  cd "$MACOS_DIR"
  rm -f "/tmp/$DESKTOP_ZIP"
  zip -r -y -q "/tmp/$DESKTOP_ZIP" "$APP_NAME.app"
  log "zip 大小: $(du -h "/tmp/$DESKTOP_ZIP" | cut -f1)"
}

# ============ 4. 发布 ============
upload() {
  [[ "$DO_UPLOAD" != 1 ]] && { log "跳过上传（--no-upload）"; return; }

  if [[ "$DO_WEB" == 1 && "$SKIP_WEB" != 1 ]]; then
    log "备份服务器旧 index.html"
    ssh "$SERVER" "cp $WEB_REMOTE_DIR/index.html $WEB_REMOTE_DIR/index.html.bak-\$(date +%Y%m%d) 2>/dev/null || true"
    log "上传 web-runtime dist → $WEB_REMOTE_DIR"
    cd "$WEB_RUNTIME_DIR/dist"
    scp index.html web-runtime.* wasm_crypto_bg.wasm "$SERVER:$WEB_REMOTE_DIR/"
    log "验证 runtime.chenddcoder.cn"
    curl --noproxy '*' -s -o /dev/null -w "  index.html: %{http_code}\n" "https://runtime.chenddcoder.cn/"
    curl --noproxy '*' -s -o /dev/null -w "  web-runtime.$WEB_HASH.js: %{http_code}\n" "https://runtime.chenddcoder.cn/web-runtime.$WEB_HASH.js"
  fi

  if [[ "$DO_DESKTOP" == 1 ]]; then
    log "备份服务器旧桌面 zip"
    ssh "$SERVER" "cp $DESKTOP_REMOTE_DIR/$DESKTOP_ZIP $DESKTOP_REMOTE_DIR/$DESKTOP_ZIP.bak-\$(date +%Y%m%d-%H%M) 2>/dev/null || true"
    log "上传桌面 zip → $DESKTOP_REMOTE_DIR/$DESKTOP_ZIP"
    scp "/tmp/$DESKTOP_ZIP" "$SERVER:$DESKTOP_REMOTE_DIR/$DESKTOP_ZIP"
    log "验证下载链接"
    curl --noproxy '*' -s -o /dev/null -w "  $DESKTOP_ZIP: %{http_code} %{size_download}B\n" "https://www.chenddcoder.cn/downloads/$DESKTOP_ZIP"
  fi
}

# ============ 执行 ============
bump_version
build_web
build_desktop
pack_zip
upload
log "✓ 发布完成"
