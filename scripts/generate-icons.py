#!/usr/bin/env python3
"""Reproduce brand assets from bridge-mark.svg.

Requirements: Python 3 standard library, RTK and cargo-tauri 2.12.4 on PATH.
Run from any directory: rtk proxy python3 scripts/generate-icons.py
Tauri's SVG renderer produces every raster; no image libraries are required.
"""

from pathlib import Path
import shutil
import struct
import subprocess
import tempfile
import xml.etree.ElementTree as ET


ROOT = Path(__file__).resolve().parents[1]
ICONS = ROOT / "crates/smoke/icons"
NS = {"svg": "http://www.w3.org/2000/svg"}


def run(*args):
    subprocess.run(["rtk", "proxy", *map(str, args)], cwd=ROOT, check=True)


def svg(body, size=64):
    return f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {size} {size}">\n{body}\n</svg>\n'


def render(source, output, sizes):
    args = ["cargo", "tauri", "icon", source, "--output", output]
    for size in sizes:
        args.extend(["--png", str(size)])
    run(*args)


def main():
    mark = ET.parse(ICONS / "bridge-mark.svg").getroot()
    paths = mark.findall("svg:g/svg:path", NS)
    if len(paths) != 2 or mark.attrib.get("viewBox") != "0 0 64 64":
        raise ValueError("bridge-mark.svg must contain two paths in a 64-unit viewBox")
    geometry = "\n".join(f'    <path d="{path.attrib["d"]}"/>' for path in paths)

    def bridge(stroke, width=4, transform=""):
        return (f'<g fill="none" stroke="{stroke}" stroke-width="{width}" '
                f'stroke-linecap="round" stroke-linejoin="round"{transform}>\n'
                f'{geometry}\n  </g>')

    defs = '''<defs>
    <linearGradient id="base" gradientUnits="userSpaceOnUse" x1="32" y1="4" x2="32" y2="60">
      <stop offset="0%" stop-color="#18333A"/><stop offset="100%" stop-color="#0C2027"/>
    </linearGradient>
    <linearGradient id="bridge" gradientUnits="userSpaceOnUse" x1="12" y1="20" x2="52" y2="44">
      <stop offset="0%" stop-color="#F8E6C1"/><stop offset="100%" stop-color="#D6AA72"/>
    </linearGradient>
  </defs>'''
    app = '<rect x="4" y="4" width="56" height="56" rx="13" fill="url(#base)"/>\n  ' + bridge("url(#bridge)")
    tray_transform = ' transform="translate(-4 -4) scale(1.125)"'
    tray = bridge("#000000", 4.8, tray_transform)
    (ICONS / "app-icon.svg").write_text(svg(defs + "\n  " + app))
    (ICONS / "tray-icon.svg").write_text(svg("  " + tray))

    # The official platform pass owns ICNS/ICO; discard mobile/Store assets.
    with tempfile.TemporaryDirectory(prefix="aubridge-icons-") as scratch:
        scratch = Path(scratch)
        run("cargo", "tauri", "icon", ICONS / "app-icon.svg", "--output", scratch / "platform")
        # Tauri's ICNS encoder iterates a map; canonical chunk order makes the
        # container reproducible without changing the encoded image payloads.
        icns = (scratch / "platform/icon.icns").read_bytes()
        chunks = []
        offset = 8
        while offset < len(icns):
            length = struct.unpack_from(">I", icns, offset + 4)[0]
            chunks.append(icns[offset:offset + length])
            offset += length
        (ICONS / "icon.icns").write_bytes(icns[:8] + b"".join(sorted(chunks)))
        render(ICONS / "app-icon.svg", scratch / "app", [16, 32, 48, 64, 128, 256, 512, 1024])
        for size in (16, 32, 48, 64, 128, 256, 512, 1024):
            shutil.copyfile(scratch / "app" / f"{size}x{size}.png", ICONS / f"{size}x{size}.png")
        shutil.copyfile(ICONS / "1024x1024.png", ICONS / "icon.png")
        # Tauri's default ICO omits 128px. Package its SVG-rendered PNG frames
        # directly in a standard PNG-backed ICO so all documented sizes exist.
        render(ICONS / "app-icon.svg", scratch / "ico", [24])
        ico_sizes = (16, 24, 32, 48, 64, 128, 256)
        offset = 6 + 16 * len(ico_sizes)
        entries, frames = [], []
        for size in ico_sizes:
            frame = ((scratch / "ico/24x24.png") if size == 24 else
                     (ICONS / f"{size}x{size}.png")).read_bytes()
            entries.append(struct.pack("<BBBBHHII", size % 256, size % 256,
                                       0, 0, 1, 32, len(frame), offset))
            frames.append(frame)
            offset += len(frame)
        (ICONS / "icon.ico").write_bytes(struct.pack("<HHH", 0, 1, len(frames)) +
                                         b"".join(entries) + b"".join(frames))
        render(ICONS / "tray-icon.svg", scratch / "tray", [16, 18, 36])
        for size, name in ((16, "tray-icon-16.png"), (18, "tray-icon.png"), (36, "tray-icon@2x.png")):
            shutil.copyfile(scratch / "tray" / f"{size}x{size}.png", ICONS / name)

        # Geometry-only contact sheet, with actual pixel-size app and tray rows.
        # White tray strokes on dark rows simulate macOS template tinting.
        preview = [defs, '<rect width="1024" height="1024" fill="#ECEDEF"/>',
                   f'<svg x="256" y="8" width="512" height="512" viewBox="0 0 64 64">{app}</svg>']
        for row, color in enumerate(("#FFFFFF", "#ECEDEF", "#16191D")):
            y = 536 + row * 160
            preview.append(f'<rect x="8" y="{y}" width="1008" height="152" rx="8" fill="{color}"/>')
            x = 24
            for size in (16, 32, 64, 128):
                preview.append(f'<svg x="{x}" y="{y + (152-size)/2}" width="{size}" height="{size}" viewBox="0 0 64 64">{app}</svg>')
                x += size + 28
            for size in (16, 18, 36):
                tinted = bridge("#FFFFFF" if row == 2 else "#000000", 4.8, tray_transform)
                preview.append(f'<svg x="{x}" y="{y + (152-size)/2}" width="{size}" height="{size}" viewBox="0 0 64 64">{tinted}</svg>')
                x += size + 30
        preview_source = scratch / "brand-preview.svg"
        preview_source.write_text(svg("\n".join(preview), 1024))
        render(preview_source, scratch / "preview", [1024])
        preview_dir = ROOT / "docs/brand"
        preview_dir.mkdir(exist_ok=True)
        shutil.copyfile(preview_source, preview_dir / "qa-preview.svg")
        shutil.copyfile(scratch / "preview/1024x1024.png", preview_dir / "qa-preview.png")


if __name__ == "__main__":
    main()
