#!/bin/bash
#
# 把 ClipSync 打包成 macOS 的 .app。
#
# 用法：
#   scripts/package-macos.sh              仅本机架构（快）
#   scripts/package-macos.sh --universal  通用二进制（Intel + Apple Silicon）
#
# 产物：target/ClipSync.app
#
# 关于签名：本机没有开发者证书时用 ad-hoc 签名（codesign -s -）。这足以让
# 程序在**本机**正常运行；分发给别人需要 Developer ID 证书并做公证，否则
# 对方会被 Gatekeeper 拦下（可在「隐私与安全性」里手动放行）。

set -euo pipefail

cd "$(dirname "$0")/.."

APP_NAME="ClipSync"
BUNDLE_ID="com.clipsync.app"
# 版本号取自 workspace，避免两处各写一份而对不上。
VERSION="$(grep -m1 '^version = ' Cargo.toml | cut -d'"' -f2)"
APP="target/${APP_NAME}.app"

UNIVERSAL=0
[[ "${1:-}" == "--universal" ]] && UNIVERSAL=1

# ————————————————————————— 编译 —————————————————————————

if [[ $UNIVERSAL == 1 ]]; then
    echo "==> 构建通用二进制"
    for t in aarch64-apple-darwin x86_64-apple-darwin; do
        rustup target list --installed | grep -qx "$t" || {
            echo "缺少 target $t，请先执行：rustup target add $t" >&2
            exit 1
        }
        cargo build --release -p clipsync-app --target "$t"
    done
    BIN="target/clipsync-universal"
    lipo -create -output "$BIN" \
        target/aarch64-apple-darwin/release/clipsync \
        target/x86_64-apple-darwin/release/clipsync
else
    echo "==> 构建（本机架构）"
    cargo build --release -p clipsync-app
    BIN="target/release/clipsync"
fi

# ————————————————————————— 骨架 —————————————————————————

echo "==> 组装 ${APP}"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/clipsync"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>            <string>${APP_NAME}</string>
    <key>CFBundleDisplayName</key>     <string>${APP_NAME}</string>
    <key>CFBundleIdentifier</key>      <string>${BUNDLE_ID}</string>
    <key>CFBundleExecutable</key>      <string>clipsync</string>
    <key>CFBundleIconFile</key>        <string>AppIcon</string>
    <key>CFBundleVersion</key>         <string>${VERSION}</string>
    <key>CFBundleShortVersionString</key> <string>${VERSION}</string>
    <key>CFBundlePackageType</key>     <string>APPL</string>
    <key>LSMinimumSystemVersion</key>  <string>11.0</string>

    <!-- 托盘程序：不在 Dock 与 Cmd-Tab 里露面。代码里也调了
         setActivationPolicy(Accessory)，这里再声明一次，免得启动瞬间
         Dock 上闪一下图标。 -->
    <key>LSUIElement</key>             <true/>

    <key>NSHighResolutionCapable</key> <true/>
</dict>
</plist>
PLIST
plutil -lint "$APP/Contents/Info.plist" > /dev/null

# ————————————————————————— 图标 —————————————————————————
#
# 现画一张 1024 的母图再缩出各尺寸，不往仓库里塞二进制资源——与"发布物是
# 单个可执行文件、不带图片资源"的做法一致。
#
# 图案沿用托盘那把剪贴板，但这里是彩色大图标，与 32px 单色托盘图标各画各的：
# 前者要在 Finder 里好看，后者要在菜单栏一眼可辨，本就是两个设计。

echo "==> 生成图标"
ICONSET="$(mktemp -d)/AppIcon.iconset"
mkdir -p "$ICONSET"

swift - "$ICONSET/icon_512x512@2x.png" <<'SWIFT'
import AppKit

let out = CommandLine.arguments[1]
let n: CGFloat = 1024
let img = NSImage(size: NSSize(width: n, height: n))
img.lockFocus()

// 背景：macOS 风格的圆角方形（squircle 近似），用与托盘"已连接"一致的绿。
let inset = n * 0.06                       // 四周留白，符合系统图标网格
let rect = NSRect(x: inset, y: inset, width: n - inset * 2, height: n - inset * 2)
let bg = NSBezierPath(roundedRect: rect, xRadius: n * 0.2237, yRadius: n * 0.2237)
NSColor(srgbRed: 0.208, green: 0.710, blue: 0.416, alpha: 1).setFill()
bg.fill()

// 前景：白色剪贴板——板身 + 顶部夹子，与托盘图标同一个形状语言。
NSColor.white.setFill()
let bw = n * 0.42, bh = n * 0.50
let body = NSRect(x: (n - bw) / 2, y: n * 0.20, width: bw, height: bh)
NSBezierPath(roundedRect: body, xRadius: n * 0.05, yRadius: n * 0.05).fill()

let cw = n * 0.20, ch = n * 0.10
let clip = NSRect(x: (n - cw) / 2, y: n * 0.655, width: cw, height: ch)
NSBezierPath(roundedRect: clip, xRadius: n * 0.028, yRadius: n * 0.028).fill()

// 板面上三条横线，暗示"内容"。用背景色挖空，比叠灰色干净。
NSColor(srgbRed: 0.208, green: 0.710, blue: 0.416, alpha: 1).setFill()
for i in 0..<3 {
    let y = n * 0.53 - CGFloat(i) * n * 0.085
    let lw = (i == 2) ? bw * 0.42 : bw * 0.62
    let line = NSRect(x: (n - lw) / 2, y: y, width: lw, height: n * 0.030)
    NSBezierPath(roundedRect: line, xRadius: n * 0.015, yRadius: n * 0.015).fill()
}

img.unlockFocus()

guard let tiff = img.tiffRepresentation,
      let rep = NSBitmapImageRep(data: tiff),
      let png = rep.representation(using: .png, properties: [:]) else {
    FileHandle.standardError.write("生成图标失败\n".data(using: .utf8)!)
    exit(1)
}
try! png.write(to: URL(fileURLWithPath: out))
SWIFT

# 由母图缩出 iconutil 需要的全部尺寸。
for spec in "16:16x16" "32:16x16@2x" "32:32x32" "64:32x32@2x" \
            "128:128x128" "256:128x128@2x" "256:256x256" "512:256x256@2x" \
            "512:512x512"; do
    px="${spec%%:*}"; name="${spec##*:}"
    sips -z "$px" "$px" "$ICONSET/icon_512x512@2x.png" \
         --out "$ICONSET/icon_${name}.png" > /dev/null
done

iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/AppIcon.icns"
rm -rf "$(dirname "$ICONSET")"

# ————————————————————————— 签名与自检 —————————————————————————

echo "==> 签名"
# 有 Developer ID 就用它，否则 ad-hoc。ad-hoc 签名本机可运行，分发需公证。
# `|| true` 不可省：没有证书时 grep 返回 1，在 `set -e` 下会让这行赋值
# 直接终止脚本——而"没有证书"恰恰是最常见的情况，不是错误。
IDENTITY="$(security find-identity -v -p codesigning 2>/dev/null \
            | grep -o '"Developer ID Application:[^"]*"' | head -1 | tr -d '"' || true)"
if [[ -n "$IDENTITY" ]]; then
    codesign --force --deep --options runtime --sign "$IDENTITY" "$APP"
    echo "    已用证书签名：$IDENTITY"
else
    codesign --force --deep --sign - "$APP"
    echo "    ad-hoc 签名（本机可运行；分发给他人需 Developer ID + 公证）"
fi

codesign --verify --strict "$APP"
[[ "$(plutil -extract LSUIElement raw "$APP/Contents/Info.plist")" == "true" ]] \
    || { echo "LSUIElement 未生效，Dock 会出现图标" >&2; exit 1; }
test -s "$APP/Contents/Resources/AppIcon.icns"

echo
echo "完成：$APP"
echo "  版本 ${VERSION}   体积 $(du -sh "$APP" | cut -f1)   架构 $(lipo -archs "$APP/Contents/MacOS/clipsync")"
echo
echo "安装：把它拖进「应用程序」，双击即可（图标出现在菜单栏，不在 Dock）。"
echo "开机自启：托盘菜单勾选，或 ${APP}/Contents/MacOS/clipsync autostart on"
