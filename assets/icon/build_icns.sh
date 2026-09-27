#!/bin/sh
# 由两份图稿生成 berth.icns。<=32px 用简化稿，>=64px 用完整稿。
set -eu
here=$(cd "$(dirname "$0")" && pwd)
out=${1:-$here/berth.icns}
set=$(mktemp -d)/berth.iconset
mkdir -p "$set"

emit() {   # emit <源图> <像素> <文件名>
    sips -s format png -z "$2" "$2" "$1" --out "$set/$3" >/dev/null
}
emit "$here/icon-small-1024.png"   16 icon_16x16.png
emit "$here/icon-small-1024.png"   32 icon_16x16@2x.png
emit "$here/icon-small-1024.png"   32 icon_32x32.png
emit "$here/icon-full-1024.png"    64 icon_32x32@2x.png
emit "$here/icon-full-1024.png"   128 icon_128x128.png
emit "$here/icon-full-1024.png"   256 icon_128x128@2x.png
emit "$here/icon-full-1024.png"   256 icon_256x256.png
emit "$here/icon-full-1024.png"   512 icon_256x256@2x.png
emit "$here/icon-full-1024.png"   512 icon_512x512.png
cp "$here/icon-full-1024.png" "$set/icon_512x512@2x.png"

iconutil -c icns "$set" -o "$out"
rm -rf "$(dirname "$set")"
echo "$out"
