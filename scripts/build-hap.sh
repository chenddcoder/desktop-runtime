#!/usr/bin/env bash
#
# build-hap.sh —— 把 desktop-runtime 的 web-runtime 前端打成鸿蒙 HAP
#
# 做的事：
#   1. 同步 quicktvui/packages/web-runtime/dist → harmony/entry/src/main/resources/rawfile/web
#   2. 把 harmony/ 工程镜像到英文临时目录（hvigor 硬限制：工程路径不许含中文，
#      而本仓库根目录叫「扩展屏快应用」，所以只能镜像出去构建）
#   3. hvigorw assembleHap
#   4. 把产物搬回 harmony/entry/build/.../outputs
#
# 用法：
#   ./scripts/build-hap.sh                 # 全量：同步 dist + 编 Rust 桥 + 构建
#   ./scripts/build-hap.sh --skip-web      # 跳过 dist 同步（前端没改时更快）
#   ./scripts/build-hap.sh --no-rust       # 只打前端壳，不重编 .so
#   ./scripts/build-hap.sh --release       # release 构建
#   ./scripts/build-hap.sh --clean         # 构建前清空镜像目录（缓存异常时用）
#
# 环境变量：
#   DEVECO_HOME   DevEco Studio.app 路径（默认自动探测）
#   HAP_BUILD_DIR 英文构建目录（默认 ~/.quickapp-harmony-build）
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DESKTOP_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
HARMONY_DIR="$DESKTOP_DIR/harmony"
WEB_RUNTIME_DIR="${WEB_RUNTIME_DIR:-$DESKTOP_DIR/../quicktvui/packages/web-runtime}"
BUILD_DIR="${HAP_BUILD_DIR:-$HOME/.quickapp-harmony-build}"
RAWFILE_WEB="$HARMONY_DIR/entry/src/main/resources/rawfile/web"

SKIP_WEB=0
BUILD_MODE="debug"
DO_CLEAN=0
DO_RUST=1
while [[ $# -gt 0 ]]; do
  case "$1" in
    --skip-web) SKIP_WEB=1 ;;
    --release)  BUILD_MODE="release" ;;
    --clean)    DO_CLEAN=1 ;;
    --no-rust)  DO_RUST=0 ;;
    -h|--help)  sed -n '3,22p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "未知参数: $1"; exit 1 ;;
  esac
  shift
done

log()  { echo "==> $*"; }
warn() { echo "⚠️  $*" >&2; }
die()  { echo "✗ $*" >&2; exit 1; }

# ============ 0. 定位 DevEco ============
find_deveco() {
  local candidates=(
    "${DEVECO_HOME:-}"
    "/Applications/DevEco-Studio.app"
    "/Volumes/WD/Applications/DevEco-Studio.app"
    "$HOME/Applications/DevEco-Studio.app"
  )
  for c in "${candidates[@]}"; do
    [[ -n "$c" && -d "$c" ]] && { echo "$c"; return 0; }
  done
  return 1
}
DEVECO="$(find_deveco)" || die "找不到 DevEco Studio。请设置 DEVECO_HOME=/path/to/DevEco-Studio.app"
export DEVECO_SDK_HOME="$DEVECO/Contents/sdk"
export PATH="$DEVECO/Contents/tools/node/bin:$DEVECO/Contents/tools/hvigor/bin:$PATH"
[[ -x "$DEVECO/Contents/tools/hvigor/bin/hvigorw" ]] || die "hvigorw 不存在：$DEVECO/Contents/tools/hvigor/bin/hvigorw"
log "DevEco: $DEVECO"

# ============ 1. 同步 web-runtime dist ============
sync_web() {
  [[ "$SKIP_WEB" == 1 ]] && { log "跳过 dist 同步（--skip-web）"; return; }
  [[ -d "$WEB_RUNTIME_DIR/dist" ]] || die "web-runtime dist 不存在: $WEB_RUNTIME_DIR/dist"
  log "同步 dist → rawfile/web"
  RAW_SRC="$WEB_RUNTIME_DIR/dist" RAW_DST="$RAWFILE_WEB" python3 - <<'PY'
import os, shutil, filecmp
src, dst = os.environ["RAW_SRC"], os.environ["RAW_DST"]
os.makedirs(dst, exist_ok=True)
copied = skipped = removed = 0
keep = set()
for name in os.listdir(src):
    if name.endswith(".gz"):          # hvigor 自己压缩，rawfile 里放 .gz 只会占体积
        continue
    keep.add(name)
    s, d = os.path.join(src, name), os.path.join(dst, name)
    if os.path.isdir(s):
        for root, _, files in os.walk(s):
            rel = os.path.relpath(root, s)
            tdir = os.path.join(d, rel) if rel != "." else d
            os.makedirs(tdir, exist_ok=True)
            for f in files:
                if f.endswith(".gz"):
                    continue
                sf, df = os.path.join(root, f), os.path.join(tdir, f)
                if os.path.exists(df) and filecmp.cmp(sf, df, shallow=False):
                    skipped += 1
                    continue
                shutil.copy2(sf, df)
                copied += 1
    else:
        if os.path.exists(d) and filecmp.cmp(s, d, shallow=False):
            skipped += 1
        else:
            shutil.copy2(s, d)
            copied += 1
# 清掉 dist 里已经消失的旧文件，避免 HAP 里残留上一个版本的 chunk
for name in os.listdir(dst):
    if name not in keep:
        p = os.path.join(dst, name)
        shutil.rmtree(p) if os.path.isdir(p) else os.remove(p)
        removed += 1
print(f"   copied={copied} unchanged={skipped} removed={removed}")
PY
  [[ -f "$RAWFILE_WEB/index.html" ]] || die "rawfile/web/index.html 缺失，dist 同步失败"
}

# ============ 2. 镜像到英文路径 ============
mirror() {
  [[ "$DO_CLEAN" == 1 && -d "$BUILD_DIR" ]] && { log "清空镜像目录"; rm -rf "$BUILD_DIR"; }
  log "镜像工程 → ${BUILD_DIR}（hvigor 不许中文路径）"
  SRC="$HARMONY_DIR" DST="$BUILD_DIR" python3 - <<'PY'
import os, shutil, filecmp
src, dst = os.environ["SRC"], os.environ["DST"]
IGNORE_DIRS = {"build", ".hvigor", ".preview", "oh_modules", ".git", ".idea", "node_modules"}
os.makedirs(dst, exist_ok=True)
copied = skipped = 0
for root, dirs, files in os.walk(src):
    dirs[:] = [d for d in dirs if d not in IGNORE_DIRS]
    rel = os.path.relpath(root, src)
    target = dst if rel == "." else os.path.join(dst, rel)
    os.makedirs(target, exist_ok=True)
    for f in files:
        if f == ".DS_Store":
            continue
        sf, df = os.path.join(root, f), os.path.join(target, f)
        if os.path.exists(df) and filecmp.cmp(sf, df, shallow=False):
            skipped += 1
            continue
        shutil.copy2(sf, df)
        copied += 1
print(f"   copied={copied} unchanged={skipped}")
PY
}

# ============ 3. 构建 ============
build() {
  log "hvigorw assembleHap（mode=${BUILD_MODE}）"
  cd "$BUILD_DIR"
  if [[ "$BUILD_MODE" == "release" ]]; then
    hvigorw assembleHap --mode module -p product=default -p buildMode=release --no-daemon
  else
    hvigorw assembleHap --mode module -p product=default --no-daemon
  fi
}

# ============ 4. 搬回产物 ============
collect() {
  local out_root="$BUILD_DIR/entry/build/default/outputs/default"
  [[ -d "$out_root" ]] || die "构建产物目录不存在: $out_root"
  local dest="$HARMONY_DIR/entry/build/default/outputs/default"
  mkdir -p "$dest"
  log "产物搬回 harmony/entry/build/default/outputs/default"
  for f in "$out_root"/*.hap "$out_root"/*.app; do
    [[ -e "$f" ]] || continue
    cp -f "$f" "$dest/"
    echo "   $(basename "$f")  $(du -h "$f" | cut -f1)"
  done
}

# ============ 5. 报告 ============
report() {
  echo
  log "构建完成"
  local signed_hap="$HARMONY_DIR/entry/build/default/outputs/default/entry-default-signed.hap"
  local unsigned_hap="$HARMONY_DIR/entry/build/default/outputs/default/entry-default-unsigned.hap"
  if [[ -f "$signed_hap" ]]; then
    log "已签名: $signed_hap"
  else
    warn "只有 unsigned HAP。要装真机必须先在 DevEco 里给本工程生成自动签名："
    echo "     DevEco 打开 $HARMONY_DIR → File → Project Structure → Signing Configs →"
    echo "     勾选 Automatically generate signature（需登录华为账号）→ 再跑一次本脚本"
  fi
  echo
  echo "真机安装与观测："
  echo "  hdc install -r ${signed_hap}"
  echo "  hdc shell aa start -a EntryAbility -b com.chenddcoder.touping"
  echo "  hdc shell hilog -T ToupingPage"
  echo "  hdc shell hilog -T ToupingRouter"
}

# ============ 1.5 Rust 桥（NAPI .so） ============
# 桌面端那套原生能力里，只有「必须自己持 socket」的部分下沉到 Rust。
# 产物 libquickapp_bridge.so 放进 entry/libs/arm64-v8a，由 hvigor 的 ProcessLibs 打进 HAP。
build_rust_bridge() {
  [[ "$DO_RUST" == 1 ]] || { log "跳过 Rust 桥（--no-rust）"; return; }
  local crate="$DESKTOP_DIR/harmony-bridge"
  [[ -f "$crate/Cargo.toml" ]] || { log "没有 harmony-bridge，跳过"; return; }

  local ndk="$DEVECO/Contents/sdk/default/openharmony/native"
  [[ -d "$ndk/llvm/bin" ]] || die "找不到 OHOS NDK: $ndk"
  # napi-build-ohos 在编译期硬读这个变量（缺了直接 panic: OHOS_NDK_HOME not set）
  export OHOS_NDK_HOME="$ndk"

  export PATH="$HOME/.cargo/bin:$PATH"
  # NAPI 模块名必须是 ArkTS import 的那个（libquickapp_bridge.so → quickapp_bridge），
  # 否则 requireNapi 拿到的模块是 undefined，全桥报 "not callable"。
  export NAPI_BUILD_TARGET_NAME=quickapp_bridge
  # rustc 对 aarch64-unknown-linux-ohos 不知道用哪个链接器，必须显式指到 NDK 的 clang 包装
  export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_OHOS_LINKER="$ndk/llvm/bin/aarch64-unknown-linux-ohos-clang"
  export CARGO_TARGET_DIR="$HOME/.quickapp-bridge-target"

  if ! rustup toolchain list 2>/dev/null | grep -q '^nightly'; then
    die "缺 nightly 工具链。ohos 目标没有预编译 std，必须 nightly + build-std：
     RUSTUP_DIST_SERVER=https://mirrors.tuna.tsinghua.edu.cn/rustup \\
       rustup toolchain install nightly --component rust-src --profile minimal"
  fi

  log "编译 Rust 桥 → aarch64-unknown-linux-ohos（build-std，首次约 3~5 分钟）"
  ( cd "$crate" && cargo +nightly build --release --target aarch64-unknown-linux-ohos \
      -Z build-std=std,panic_abort )

  local so="$CARGO_TARGET_DIR/aarch64-unknown-linux-ohos/release/libquickapp_bridge.so"
  [[ -f "$so" ]] || die "Rust 桥产物缺失: $so"
  local strip="$ndk/llvm/bin/llvm-strip"
  [[ -x "$strip" ]] && "$strip" --strip-unneeded "$so" 2>/dev/null || true

  local libs="$HARMONY_DIR/entry/libs/arm64-v8a"
  mkdir -p "$libs"
  cp -f "$so" "$libs/"
  log "已放入 entry/libs/arm64-v8a/libquickapp_bridge.so（$(du -h "$so" | cut -f1)）"
}

sync_web
build_rust_bridge
mirror
build
collect
report
