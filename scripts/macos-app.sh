#!/usr/bin/env bash
# Wrap a torrenter-gui Mach-O in torrenter.app and zip it for GitHub Releases.
# Usage: macos-app.sh <gui-binary> <version> <out.zip>
# Does not replace the naked updater asset. macOS only (sips, iconutil, ditto).
set -euo pipefail

if [[ $# -ne 3 ]]; then
  echo "usage: $0 <gui-binary> <version> <out.zip>" >&2
  exit 2
fi

GUI=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
VERSION="${2#v}"
OUT_ZIP=$(cd "$(dirname "$3")" && pwd)/$(basename "$3")
ROOT=$(cd "$(dirname "$0")/.." && pwd)
RASTER="$ROOT/src/icon_raster.rs"

if [[ ! -f "$GUI" ]]; then
  echo "missing GUI binary: $GUI" >&2
  exit 1
fi
if [[ ! -f "$RASTER" ]]; then
  echo "missing icon raster: $RASTER" >&2
  exit 1
fi
if [[ "$VERSION" == *[!0-9.]* || -z "$VERSION" ]]; then
  echo "version must look like X.Y.Z, got: $VERSION" >&2
  exit 1
fi

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

python3 - "$RASTER" "$WORK/dump_rgba.rs" <<'PY'
import pathlib, sys
raster, dest = sys.argv[1], sys.argv[2]
escaped = pathlib.Path(raster).resolve().as_posix().replace("\\", "\\\\").replace('"', '\\"')
pathlib.Path(dest).write_text(
    "mod icon_raster {\n"
    f'    include!("{escaped}");\n'
    "}\n"
    "fn main() {\n"
    "    let size = 1024u32;\n"
    "    std::fs::write(std::env::args().nth(1).expect(\"rgba path\"), icon_raster::raster(size)).unwrap();\n"
    "}\n"
)
PY
rustc --edition 2021 -O -o "$WORK/dump_rgba" "$WORK/dump_rgba.rs"
"$WORK/dump_rgba" "$WORK/icon.rgba"

python3 - "$WORK/icon.rgba" "$WORK/icon.png" <<'PY'
import pathlib, struct, sys, zlib

size = 1024
rgba = pathlib.Path(sys.argv[1]).read_bytes()
if len(rgba) != size * size * 4:
    raise SystemExit(f"unexpected rgba length {len(rgba)}")

def chunk(tag: bytes, data: bytes) -> bytes:
    return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)

raw = b"".join(b"\x00" + rgba[y * size * 4 : (y + 1) * size * 4] for y in range(size))
png = b"\x89PNG\r\n\x1a\n"
png += chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
png += chunk(b"IDAT", zlib.compress(raw, 9))
png += chunk(b"IEND", b"")
pathlib.Path(sys.argv[2]).write_bytes(png)
PY

ICONSET="$WORK/torrenter.iconset"
mkdir -p "$ICONSET"
sips -z 16 16 "$WORK/icon.png" --out "$ICONSET/icon_16x16.png" >/dev/null
sips -z 32 32 "$WORK/icon.png" --out "$ICONSET/icon_16x16@2x.png" >/dev/null
sips -z 32 32 "$WORK/icon.png" --out "$ICONSET/icon_32x32.png" >/dev/null
sips -z 64 64 "$WORK/icon.png" --out "$ICONSET/icon_32x32@2x.png" >/dev/null
sips -z 128 128 "$WORK/icon.png" --out "$ICONSET/icon_128x128.png" >/dev/null
sips -z 256 256 "$WORK/icon.png" --out "$ICONSET/icon_128x128@2x.png" >/dev/null
sips -z 256 256 "$WORK/icon.png" --out "$ICONSET/icon_256x256.png" >/dev/null
sips -z 512 512 "$WORK/icon.png" --out "$ICONSET/icon_256x256@2x.png" >/dev/null
sips -z 512 512 "$WORK/icon.png" --out "$ICONSET/icon_512x512.png" >/dev/null
sips -z 1024 1024 "$WORK/icon.png" --out "$ICONSET/icon_512x512@2x.png" >/dev/null
iconutil -c icns -o "$WORK/AppIcon.icns" "$ICONSET"

APP="$WORK/torrenter.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$GUI" "$APP/Contents/MacOS/torrenter-gui"
chmod +x "$APP/Contents/MacOS/torrenter-gui"
cp "$WORK/AppIcon.icns" "$APP/Contents/Resources/AppIcon.icns"
printf 'APPL????' > "$APP/Contents/PkgInfo"
cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDevelopmentRegion</key>
	<string>en</string>
	<key>CFBundleDisplayName</key>
	<string>torrenter</string>
	<key>CFBundleExecutable</key>
	<string>torrenter-gui</string>
	<key>CFBundleIconFile</key>
	<string>AppIcon</string>
	<key>CFBundleIdentifier</key>
	<string>com.danted4.torrenter</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundleName</key>
	<string>torrenter</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>${VERSION}</string>
	<key>CFBundleVersion</key>
	<string>${VERSION}</string>
	<key>LSApplicationCategoryType</key>
	<string>public.app-category.utilities</string>
	<key>LSMinimumSystemVersion</key>
	<string>12.0</string>
	<key>NSHighResolutionCapable</key>
	<true/>
</dict>
</plist>
EOF

xattr -cr "$APP"
codesign --force --deep -s - "$APP"
xattr -cr "$APP"
rm -f "$OUT_ZIP"
# Plain zip: no AppleDouble ._* forks (ditto --keepParent would add them).
(
  cd "$WORK"
  COPYFILE_DISABLE=1 zip -r -X "$OUT_ZIP" torrenter.app
)
echo "wrote $OUT_ZIP"
