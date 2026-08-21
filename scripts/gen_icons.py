#!/usr/bin/env python3
"""生成桌面运行时图标集。

母版：src-tauri/icons/icon.svg（保留 SVG，矢量可编辑）
输出：icons/32x32.png, 128x128.png, 256x256.png, icon.icns

依赖 ImageMagick（`magick` 或 `convert`）做栅格化，以及 macOS 的 `iconutil` 打包 icns。
如果找不到 ImageMagick，会退回到原先的无依赖占位方块生成。
"""
import os
import shutil
import subprocess
import sys
import zlib
import struct

HERE = os.path.dirname(os.path.abspath(__file__))
ICON_DIR = os.path.join(HERE, "..", "src-tauri", "icons")
MASTER_SVG = os.path.join(ICON_DIR, "icon.svg")
os.makedirs(ICON_DIR, exist_ok=True)

DARK = (0x18, 0x5F, 0xA5)   # 品牌蓝
LIGHT = (0x37, 0x8A, 0xDD)  # 内框亮蓝


def _find_magick():
    for cmd in ("magick", "convert"):
        if shutil.which(cmd):
            return cmd
    return None


def _run(cmd, **kwargs):
    print("run:", " ".join(cmd))
    subprocess.run(cmd, check=True, **kwargs)


def _rasterize_with_imagemagick():
    magick = _find_magick()
    if magick is None:
        return False

    sizes = {
        32: "32x32.png",
        128: "128x128.png",
        256: "256x256.png",
    }
    for size, name in sizes.items():
        out = os.path.join(ICON_DIR, name)
        # 用 PNG32: 前缀强制输出 8-bit RGBA（colortype=6），避免两坑：
        # ① 本机 ImageMagick 是 Q16 构建，默认会输出 16-bit/通道的 PNG（RGBA64），
        #   Tauri/tao 校验窗口图标要求 8-bit，否则 DimensionsVsPixelCount panic；
        # ② 自动量化成调色板 PNG（可能丢 alpha），同样会让解码字节数对不上。
        _run([magick, "-background", "none", MASTER_SVG, "-resize", f"{size}x{size}", f"PNG32:{out}"])

    iconset = os.path.join(ICON_DIR, "tmp.iconset")
    os.makedirs(iconset, exist_ok=True)
    icon_map = [
        (16, "icon_16x16.png"),
        (32, "icon_16x16@2x.png"),
        (32, "icon_32x32.png"),
        (64, "icon_32x32@2x.png"),
        (128, "icon_128x128.png"),
        (256, "icon_128x128@2x.png"),
        (256, "icon_256x256.png"),
        (512, "icon_256x256@2x.png"),
        (512, "icon_512x512.png"),
        (1024, "icon_512x512@2x.png"),
    ]
    for s, nm in icon_map:
        out_path = os.path.join(iconset, nm)
        _run([magick, "-background", "none", MASTER_SVG, "-resize", f"{s}x{s}", f"PNG32:{out_path}"])

    icns = os.path.join(ICON_DIR, "icon.icns")
    try:
        _run(["iconutil", "--convert", "icns", iconset, "-o", icns])
    except (FileNotFoundError, subprocess.CalledProcessError) as e:
        print("iconutil 不可用，跳过 icns 生成：", e)
    finally:
        for f in os.listdir(iconset):
            os.remove(os.path.join(iconset, f))
        os.rmdir(iconset)

    return True


def _write_png(path, size):
    """无依赖回退：绘制原来的占位方块。"""
    w = h = size
    margin = max(1, int(size * 0.28))
    raw = bytearray()
    for y in range(h):
        raw.append(0)
        for x in range(w):
            if margin <= x < size - margin and margin <= y < size - margin:
                raw += bytes(LIGHT) + b"\xff"
            else:
                raw += bytes(DARK) + b"\xff"
    comp = zlib.compress(bytes(raw), 9)

    def chunk(typ, data):
        body = typ + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    sig = b"\x89PNG\r\n\x1a\n"
    ihdr = struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0)
    with open(path, "wb") as f:
        f.write(sig)
        f.write(chunk(b"IHDR", ihdr))
        f.write(chunk(b"IDAT", comp))
        f.write(chunk(b"IEND", b""))


def _fallback_placeholder():
    sizes = [32, 128, 256]
    names = {32: "32x32.png", 128: "128x128.png", 256: "256x256.png"}
    for s in sizes:
        p = os.path.join(ICON_DIR, names[s])
        _write_png(p, s)
        print("wrote", p)

    iconset = os.path.join(ICON_DIR, "tmp.iconset")
    os.makedirs(iconset, exist_ok=True)
    icon_map = {
        16: "icon_16x16.png",
        32: "icon_32x32.png",
        128: "icon_128x128.png",
        256: "icon_256x256.png",
        512: "icon_512x512.png",
    }
    for s, nm in icon_map.items():
        _write_png(os.path.join(iconset, nm), s)
    icns = os.path.join(ICON_DIR, "icon.icns")
    try:
        subprocess.run(
            ["iconutil", "--convert", "icns", iconset, "-o", icns],
            check=True,
        )
        print("wrote", icns)
    except (FileNotFoundError, subprocess.CalledProcessError) as e:
        print("iconutil 不可用，跳过 icns 生成：", e)
    finally:
        for f in os.listdir(iconset):
            os.remove(os.path.join(iconset, f))
        os.rmdir(iconset)


def _touch_tauri_conf():
    """顶一下 tauri.conf.json 的 mtime，让 tauri-build 重新嵌入新图标。

    否则只换了图标文件、没动配置时，Cargo 会判定构建脚本无需重跑，
    直接沿用旧图标缓存（dev/构建出来的图标不变）。
    """
    conf = os.path.join(ICON_DIR, "..", "tauri.conf.json")
    if os.path.exists(conf):
        os.utime(conf, None)
        print("已更新 tauri.conf.json 时间戳，下次 dev/build 会重嵌图标")


def main():
    if not os.path.exists(MASTER_SVG):
        print(f"母版 SVG 不存在：{MASTER_SVG}，改用占位方块")
        _fallback_placeholder()
        return

    if _rasterize_with_imagemagick():
        _touch_tauri_conf()
        print("已从母版 SVG 生成图标集")
        return

    print("未找到 ImageMagick（magick/convert），改用零依赖占位方块")
    _fallback_placeholder()


if __name__ == "__main__":
    main()
