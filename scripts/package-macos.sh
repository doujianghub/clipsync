#!/bin/bash
#
# 把 ClipSync 打包成 macOS 的 .app。
#
# 用法：
#   scripts/package-macos.sh                    仅本机架构（快）
#   scripts/package-macos.sh --universal        通用二进制（Intel + Apple Silicon）
#   scripts/package-macos.sh --universal --dmg  再打一个 dmg 便于分发
#
# 产物：target/ClipSync.app 与 ClipSync-<版本>.zip；加 --dmg 时还有 .dmg
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
MAKE_DMG=0
for arg in "$@"; do
    case "$arg" in
        --universal) UNIVERSAL=1 ;;
        --dmg)       MAKE_DMG=1 ;;
        *) echo "未知参数：$arg" >&2; exit 2 ;;
    esac
done

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

    <!-- 本地网络权限（macOS 14+ 必需）。
         局域网自动发现靠 UDP 组播（239.255.71.83），而 macOS 会把组播/广播
         归入"本地网络"隐私类别：没有这条声明，系统既不会弹授权请求，发送也
         直接失败——`send_to` 返回 EHOSTUNREACH (errno 65)。
         症状是日志里刷"局域网信标一个网卡都发不出去"，而同一份代码从终端
         跑却一切正常（终端自己有这个权限）。 -->
    <key>NSLocalNetworkUsageDescription</key>
    <string>用于在局域网内自动发现你的其它设备，免去手动输入 IP。</string>

    <!-- 文件访问用途说明。
         复制文件时剪贴板里只有**路径**，内容要由本程序去读——而这些位置都归
         macOS 的 TCC 管。没有这些键，系统弹窗只有一句干巴巴的默认文案，用户
         看不出为什么一个剪贴板工具要读他的「文稿」，多半就点了拒绝；而拒绝
         之后是**静默失败**，此后从这些位置复制文件永远同步不过去，日志里也
         只有一句"读不了"。
         排查用 `clipsync clipdiag`（必须从 App 包内跑，终端的授权是另一套）。 -->
    <key>NSDesktopFolderUsageDescription</key>
    <string>复制桌面上的文件时，需要读取文件内容才能同步到你的其它设备。</string>
    <key>NSDocumentsFolderUsageDescription</key>
    <string>复制「文稿」里的文件时，需要读取文件内容才能同步到你的其它设备。</string>
    <key>NSDownloadsFolderUsageDescription</key>
    <string>复制「下载」里的文件时，需要读取文件内容才能同步到你的其它设备。</string>
    <key>NSRemovableVolumesUsageDescription</key>
    <string>复制外置磁盘上的文件时，需要读取文件内容才能同步到你的其它设备。</string>
    <key>NSNetworkVolumesUsageDescription</key>
    <string>复制网络卷上的文件时，需要读取文件内容才能同步到你的其它设备。</string>
    <key>NSFileProviderDomainUsageDescription</key>
    <string>复制 iCloud 云盘等位置的文件时，需要读取文件内容才能同步到你的其它设备。</string>
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
    # 有证书时**不要**自定义指定要求：证书签名的默认要求本来就既稳定
    # （认 bundle id + 证书链，重新打包不变）又安全。
    codesign --force --deep --options runtime --sign "$IDENTITY" "$APP"
    echo "    已用证书签名：$IDENTITY"
else
    # ad-hoc 签名要**显式指定一个稳定的「指定要求」**，否则系统会把它退化成
    # `cdhash H"..."`——锁死在这一份二进制的哈希上。
    #
    # 后果是权限每次重新打包就作废：macOS 的隐私授权（本地网络、其他 App 的
    # 数据、文件与文件夹）都是按这个要求认 App 的，哈希对不上就当成另一个
    # 程序。用户看到的现象是**开关自己关掉了**——他明明刚打开过。
    #
    # 换成 `identifier "..."` 之后，重新打包不再影响已授的权限：
    #     两个不同二进制 → cdhash 38a47b… / d17d0970…
    #     指定要求        → 都是 identifier "com.clipsync.app"
    #
    # 代价：任何自称同一个 bundle id 的程序都能继承这些授权。对一个自己编译、
    # 本机安装的工具是划算的；要既稳定又严格，只能上 Developer ID 证书
    # （脚本检测到证书会自动改用它，不走这条分支）。
    codesign --force --deep --sign - \
        -r="designated => identifier \"${BUNDLE_ID}\"" "$APP"
    echo "    ad-hoc 签名（本机可运行；分发给他人需 Developer ID + 公证）"
fi

codesign --verify --strict "$APP"
[[ "$(plutil -extract LSUIElement raw "$APP/Contents/Info.plist")" == "true" ]] \
    || { echo "LSUIElement 未生效，Dock 会出现图标" >&2; exit 1; }

# 指定要求必须是稳定的那种。退化成 cdhash 的话，用户每装一次新包就要把
# 所有隐私授权重开一遍——而且系统不会说为什么，只是把开关默默关掉。
if [[ -z "$IDENTITY" ]]; then
    codesign -d -r- "$APP" 2>&1 | grep -q 'designated => identifier' \
        || { echo "指定要求退化成了 cdhash——每次重新打包都会让已授权限失效" >&2; exit 1; }
fi

# 权限用途说明漏一条，对应位置的文件就永远同步不了，而且是**静默**的——
# 没有报错、没有弹窗，只有日志里一句"读不了"。所以在这儿卡住，别等实机。
for k in NSLocalNetworkUsageDescription NSDesktopFolderUsageDescription \
         NSDocumentsFolderUsageDescription NSDownloadsFolderUsageDescription \
         NSRemovableVolumesUsageDescription NSNetworkVolumesUsageDescription; do
    plutil -extract "$k" raw "$APP/Contents/Info.plist" > /dev/null 2>&1 \
        || { echo "Info.plist 缺少 $k——对应位置的文件会静默同步失败" >&2; exit 1; }
done
test -s "$APP/Contents/Resources/AppIcon.icns"

# ————————————————————————— 分发包 —————————————————————————
#
# .app 是个**目录**，用普通 zip 传容易丢东西；而 Apple Silicon 上签名一旦
# 损坏，内核直接拒绝执行——报的是"应用程序无法打开"，比 Gatekeeper 那句
# "无法验证开发者"更让人摸不着头脑。
#
# 所以给出两种可靠形式：dmg（macOS 标准分发格式，只读镜像，内容动不了），
# 以及 ditto 打的 zip（Apple 官方推荐，完整保留签名与扩展属性）。

DMG="target/${APP_NAME}-${VERSION}.dmg"
ZIP="target/${APP_NAME}-${VERSION}.zip"

echo "==> 打分发包"
rm -f "$ZIP"
# 用 ditto 而不是 zip：它会完整保留签名所依赖的一切。
ditto -c -k --sequesterRsrc --keepParent "$APP" "$ZIP"
echo "    $ZIP  ($(du -h "$ZIP" | cut -f1))"

if [[ $MAKE_DMG == 1 ]]; then
    rm -f "$DMG"
    STAGE="$(mktemp -d)/${APP_NAME}"
    mkdir -p "$STAGE"
    cp -R "$APP" "$STAGE/"
    # 放一个「应用程序」快捷方式，用户拖进去即可安装——macOS 上的惯例。
    ln -s /Applications "$STAGE/应用程序"
    hdiutil create -quiet -volname "$APP_NAME" -srcfolder "$STAGE" \
        -ov -format UDZO "$DMG"
    rm -rf "$(dirname "$STAGE")"
    echo "    $DMG  ($(du -h "$DMG" | cut -f1))"
fi

echo
echo "完成：$APP"
echo "  版本 ${VERSION}   体积 $(du -sh "$APP" | cut -f1)   架构 $(lipo -archs "$APP/Contents/MacOS/clipsync")"
echo
echo "安装：把它拖进「应用程序」，双击即可（图标出现在菜单栏，不在 Dock）。"
echo "开机自启：托盘菜单勾选，或 ${APP}/Contents/MacOS/clipsync autostart on"
echo
echo "首次运行会请求「本地网络」权限——务必允许，否则局域网自动发现不可用"
echo "（覆盖网如 Tailscale 不受影响）。若误点了拒绝，去"
echo "「系统设置 › 隐私与安全性 › 本地网络」把 ClipSync 打开。"

if [[ -z "$IDENTITY" ]]; then
    cat <<'TIP'

给别人用时请一并转告（ad-hoc 签名的固有限制，与程序本身无关）：

  首次打开会被系统拦下。**右键点图标 → 打开**（不要双击），弹窗里选「打开」。
  macOS 15 及以上：改去「系统设置 › 隐私与安全性」，页面下方点「仍要打开」。
  只需一次，之后正常双击。

  若提示「已损坏」或「无法打开」，多半是传输弄坏了签名，让对方执行：
      xattr -cr /Applications/ClipSync.app

  要彻底免掉这些提示，需要 Apple Developer Program（$99/年）的
  Developer ID 证书并做公证。本脚本检测到证书会自动改用它，无需改动。
TIP
fi
