#!/bin/sh
# 把 release 产物打包成 Berth.app。不安装、不启动，只在 dist/ 下产出。
#
#   ./packaging/make-app.sh [输出目录]
#
# berth 在启动 berthd 时会先找自己旁边的同名文件（client.rs 的
# find_berthd），所以三个二进制放进同一个 Contents/MacOS 就够了，
# .app 不依赖 PATH。
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
dist=${1:-$root/dist}
app=$dist/Berth.app
ver=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/crates/berth-app/Cargo.toml" | head -1)
: "${ver:=0.1.0}"
id=io.github.xsser.berth

for b in berth berthd berth-hook; do
    [ -x "$root/target/release/$b" ] || {
        echo "缺少 target/release/$b，先跑 cargo build --release --workspace" >&2
        exit 1
    }
done

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"

for b in berth berthd berth-hook; do
    cp "$root/target/release/$b" "$app/Contents/MacOS/$b"
done

"$root/assets/icon/build_icns.sh" "$app/Contents/Resources/berth.icns" >/dev/null

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleInfoDictionaryVersion</key>   <string>6.0</string>
    <key>CFBundlePackageType</key>             <string>APPL</string>
    <key>CFBundleName</key>                    <string>berth</string>
    <key>CFBundleDisplayName</key>             <string>berth</string>
    <key>CFBundleIdentifier</key>              <string>$id</string>
    <key>CFBundleExecutable</key>              <string>berth</string>
    <key>CFBundleIconFile</key>                <string>berth</string>
    <key>CFBundleShortVersionString</key>      <string>$ver</string>
    <key>CFBundleVersion</key>                 <string>$ver</string>
    <key>LSMinimumSystemVersion</key>          <string>11.0</string>
    <key>LSApplicationCategoryType</key>       <string>public.app-category.developer-tools</string>
    <key>NSHighResolutionCapable</key>         <true/>
    <key>NSSupportsAutomaticGraphicsSwitching</key> <true/>
</dict>
</plist>
PLIST

# Apple Silicon 上二进制必须带签名才能运行。cargo 链接时已加过 ad-hoc
# 签名，但改动过 bundle 之后要整体重签，否则签名与 bundle id 不一致。
# 先签嵌套的可执行文件，再签 bundle 本身（--deep 已废弃，不用）。
for b in berthd berth-hook berth; do
    codesign --force --sign - --timestamp=none "$app/Contents/MacOS/$b" 2>/dev/null
done
codesign --force --sign - --timestamp=none --identifier "$id" "$app" 2>/dev/null

codesign --verify --strict "$app"
echo "$app"
