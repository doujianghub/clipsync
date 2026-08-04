# macOS 实现指南

> **背景**：本项目在 Windows 上开发并完成全部功能验证。跨平台部分（同步引擎、
> 网络、加密、文件缓存与续传）本身平台无关，在 macOS 上应当直接可用；但有
> **5 处平台特定代码**在 macOS 侧只留了空实现，需要在 Mac 上补齐并验证。
>
> 本文逐项说明：**在哪、要做什么、用什么 API、怎么验证**。
>
> ---
>
> ## ✅ 补齐状态（2026-08-04，Apple Silicon / macOS 15.5 / rustc 1.97.1）
>
> 下列 7 项均已实现并在本机实测通过，`cargo test` 98 项全过。
> 本文档下方各节保留原始设计说明，但**代码已与其有若干出入**，以代码为准；
> 出入之处已在对应小节标注「实现修正」。
>
> | # | 项目 | 状态 |
> |---|---|---|
> | 2 | 敏感内容探测 | ✅ 已补齐并实测 |
> | 1 | changeCount 令牌 | ✅ 已补齐并实测 |
> | 5 | 托盘事件循环 | ✅ 已补齐并实测 |
> | 4 | 文件列表读写 | ✅ 已补齐，并修复一处静默丢数据问题 |
> | 3 | 图片格式查询 | ✅ 已补齐并实测 |
> | 6 | 开机自启 | ✅ 已实测（此前从未验证过） |
> | 7 | 私钥权限 0600 | ✅ 已加固 |
>
> **依赖版本修正（重要）**：本文下方建议 `objc2 = "0.5"` / `objc2-app-kit = "0.2"`，
> 但 `Cargo.lock` 显示 `arboard` 3.6 与 `tray-icon` 0.19 实际使用的是
> **objc2 0.6 / objc2-app-kit 0.3**。工作区里那份 0.5 是 `muda`（tray-icon 的
> 菜单依赖）的传递依赖。按 0.5 写会引入第二份编译产物，故实际采用 0.6/0.3。
> 两者 API 有差异：0.6 中 `generalPasteboard()`、`changeCount()`、`types()`
> 等均为**安全**函数，不需要 `unsafe`。
>
> **尚未验证**：第 4 项的 Mac↔Windows 双机端到端验证（本机只有 Mac，
> 需要 Windows 侧配合），以及验证清单中的 1–10 项双机测试。单机侧的
> 剪贴板读写、探测、托盘、自启均已验证。

---

## 0. 先跑起来看看现状

```bash
git clone <本仓库>
cd clipsync
cargo build
cargo test            # 应全部通过：这些测试都是平台无关的
cargo run -- addrs    # 应列出本机地址（含 Tailscale 等虚拟网卡）
```

**预期现状**（补齐前）：

| 功能 | 补齐前 | 补齐后（现状） |
|---|---|---|
| 文本同步 | ✅ 可用（`arboard` 跨平台） | ✅ |
| 图片同步 | ✅ 可用 | ✅ |
| 配对 / 加密 / 自动发现 | ✅ 可用（纯网络逻辑） | ✅ |
| 文件同步 | ❌ 读不到文件列表，收到文件时报错 | ✅ 读写均可用 |
| 敏感内容跳过 | ⚠️ 永远返回"非敏感"（密码会被同步！） | ✅ 已探测并跳过 |
| 剪贴板变更检测 | ⚠️ 退化为读取内容比哈希 | ✅ 用 changeCount 廉价令牌 |
| 托盘菜单 | ⚠️ 图标可能出现但点击无响应 | ✅ 事件循环已接入 |
| 开机自启 | ⚠️ 已写 LaunchAgent，但**从未验证** | ✅ 已实测可用 |

> ⚠️ **原安全提示（已解决）**：补齐第 2 项之前，密码复制会被同步到其它设备。
> 该缺口已于 2026-08-04 补齐，见下方第 2 节。

---

## 1. 剪贴板变更令牌（性能）

**文件**：`crates/clipsync-clip/src/change_token.rs` → `mod platform`（`#[cfg(not(windows))]` 分支）

**要做什么**：返回 `NSPasteboard.general.changeCount`。这是一个单调递增的整数，
任何剪贴板变化都会使它 +1。有了它，监听器每轮只需比较一个整数，而不必读取
并哈希剪贴板内容——这正是"极低占用"的关键。

**API**：
```rust
use objc2_app_kit::NSPasteboard;

pub fn change_token() -> Option<u64> {
    // SAFETY: generalPasteboard 可在任意线程调用；changeCount 是纯读取。
    let pb = unsafe { NSPasteboard::generalPasteboard() };
    let count = unsafe { pb.changeCount() }; // NSInteger
    Some(count as u64)
}
```

**依赖**：在 `crates/clipsync-clip/Cargo.toml` 加
```toml
[target.'cfg(target_os = "macos")'.dependencies]
objc2 = "0.5"
objc2-app-kit = { version = "0.2", features = ["NSPasteboard"] }
objc2-foundation = "0.2"
```
> 版本需与 `arboard` 传递引入的一致，避免同一 crate 出现两个版本。先用
> `cargo tree -p arboard | grep objc2` 查清实际版本再填。

**怎么验证**：跑起来后连续复制几次，日志（`CLIPSYNC_LOG=debug`）中每次复制
应只触发一次读取；CPU 占用应接近 0。

---

## 2. 敏感内容探测（安全，优先级最高）

**文件**：`crates/clipsync-clip/src/sensitive.rs` → `mod platform`（非 Windows 分支）

**要做什么**：识别密码管理器等工具打的"不要记录此内容"标记，跳过同步。
macOS 生态的社区约定（见 nspasteboard.com）是往剪贴板写入一个特殊类型：

- `org.nspasteboard.ConcealedType` —— 敏感内容（密码等）
- `org.nspasteboard.TransientType` —— 瞬态内容（不应被历史工具记录）

1Password、KeePassXC 等均遵循。只要这两个类型任一存在，就应跳过。

**API**：
```rust
use objc2_app_kit::NSPasteboard;
use objc2_foundation::NSString;

pub fn is_sensitive() -> bool {
    let pb = unsafe { NSPasteboard::generalPasteboard() };
    let types = match unsafe { pb.types() } {
        Some(t) => t,
        None => return false,
    };
    for t in types.iter() {
        let s = t.to_string();
        if s == "org.nspasteboard.ConcealedType" || s == "org.nspasteboard.TransientType" {
            return true;
        }
    }
    false
}
```

**怎么验证**：用一段 Swift/ObjC 脚本或密码管理器写入带 `ConcealedType` 的内容，
观察日志出现 `本地变化跳过 (Sensitive)`。最简验证脚本：

```bash
osascript -e 'tell application "System Events" to set the clipboard to "test"'
# 更精确的验证需要写一小段 Swift：
```
```swift
import AppKit
let pb = NSPasteboard.general
pb.clearContents()
pb.setString("fake-password", forType: .string)
pb.setData(Data(), forType: NSPasteboard.PasteboardType("org.nspasteboard.ConcealedType"))
```
保存为 `conceal.swift`，`swift conceal.swift` 运行，随后日志应显示跳过。

---

## 3. 剪贴板是否含图片（减少干扰）

**文件**：`crates/clipsync-clip/src/formats.rs` → `mod platform`（非 Windows 分支）

**要做什么**：不打开/读取剪贴板的前提下，判断当前是否有图片。目的是避免
"先尝试读图片失败再读文本"造成的多余一次剪贴板访问——减少与其它程序抢锁。

**API**：检查 `pb.types()` 是否包含 `NSPasteboardTypePNG` / `NSPasteboardTypeTIFF`：
```rust
pub fn has_image() -> Option<bool> {
    let pb = unsafe { NSPasteboard::generalPasteboard() };
    let types = unsafe { pb.types() }?;
    let has = types.iter().any(|t| {
        let s = t.to_string();
        s == "public.png" || s == "public.tiff"
    });
    Some(has)
}
```

**怎么验证**：截图（`Cmd+Shift+4`）后日志应识别为图片；复制文本后不应尝试读图片。

---

## 4. 文件列表读写（M4 文件同步的前提）

**文件**：`crates/clipsync-clip/src/filelist.rs` → `mod platform`（非 Windows 分支）

**要做什么**：两个函数。

### 4a. 读：从剪贴板取出文件路径

Finder 复制文件时，剪贴板里是 `NSURL` 对象（`public.file-url`）。

```rust
use objc2_app_kit::NSPasteboard;
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString, NSURL};

pub fn read_file_paths() -> Result<Option<Vec<PathBuf>>> {
    let pb = unsafe { NSPasteboard::generalPasteboard() };
    // options: @{ NSPasteboardURLReadingFileURLsOnlyKey: @YES }
    // classes: [NSURL class]
    let objects = unsafe { pb.readObjectsForClasses_options(&classes, Some(&options)) };
    // 逐个转成 PathBuf：url.path() -> Option<Retained<NSString>>
}
```

关键点：
- `readObjectsForClasses:options:` 的 options 传 `NSPasteboardURLReadingFileURLsOnlyKey = true`，
  否则普通网页 URL 也会被当成文件。
- `NSURL::path()` 得到文件系统路径。

### 4b. 写：把文件路径放回剪贴板

接收到文件、落地之后，要写回剪贴板使其可在 Finder 中粘贴。

```rust
pub fn write_file_paths(paths: &[PathBuf]) -> Result<()> {
    let pb = unsafe { NSPasteboard::generalPasteboard() };
    unsafe { pb.clearContents() };
    // 构造 NSArray<NSURL>，每个用 NSURL::fileURLWithPath
    let ok = unsafe { pb.writeObjects(&urls) };
    if !ok { anyhow::bail!("写入剪贴板文件列表失败") }
    Ok(())
}
```

同时把 `SUPPORTED` 改为 `true`。

> **实现修正（务必注意）**：`writeObjects` 向 pasteboard 服务的提交是**异步**的。
> 若写完就让进程退出，**除第一条以外的条目会来不及落地**——多文件复制到对端
> 只会剩下第一个文件，而 `writeObjects` 仍然返回 `true`，没有任何报错。
>
> 修复办法：写入后回读一次 `pb.types()` 强制完成往返。注意：
> - 只数 `pasteboardItems()` 的**条数**不行（本地应答，起不到同步作用）；
> - 读 `changeCount()` 也不行（同上）；
> - 必须真的取数据（`pb.types()` 最便宜）。
>
> 这一点只能**跨进程**验证：同一进程内即便没有回读也能看到全部条目，
> 只有另一个进程（真实的粘贴方）才会看到被截断的结果。回归测试
> `written_files_survive_writer_exit` 因此要 spawn 一个写完即退出的子进程。

**怎么验证**：见下方"验证清单"的文件同步部分。Finder 里复制一个文件，
应在对端 Finder 中可粘贴出同名同内容的文件。

---

## 5. 托盘事件循环（界面）

**文件**：`crates/clipsync-app/src/tray.rs` → `pump_platform_events()`（非 Windows 分支）

**要做什么**：`tray-icon` 在 macOS 上依赖 `NSApplication` 的运行循环，否则图标
可能出现但菜单点击无响应。本项目的架构是"主线程跑托盘循环 + 后台线程跑同步"，
因此**不能**用会阻塞的 `app.run()`，而应每轮手动抽干事件队列。

**API 思路**：
```rust
use objc2_app_kit::{NSApplication, NSEventMask};
use objc2_foundation::{MainThreadMarker, NSDate, NSDefaultRunLoopMode};

fn pump_platform_events() {
    let mtm = MainThreadMarker::new().expect("托盘必须在主线程运行");
    let app = NSApplication::sharedApplication(mtm);
    // 用 distantPast 表示"不等待，有就取、没有就返回"
    while let Some(event) = unsafe {
        app.nextEventMatchingMask_untilDate_inMode_dequeue(
            NSEventMask::Any,
            Some(&NSDate::distantPast()),
            NSDefaultRunLoopMode,
            true,
        )
    } {
        unsafe { app.sendEvent(&event) };
    }
}
```

**还需要做的两件事**（否则行为不对）：

1. **首次进入循环前初始化 NSApp 并设为附件模式**，否则 Dock 里会出现一个图标，
   而托盘程序不应该有 Dock 图标：
   ```rust
   app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
   unsafe { app.finishLaunching() };
   ```
   建议加在 `tray::run()` 开头，用 `#[cfg(target_os = "macos")]` 包起来。

2. 若打包成 `.app`，在 `Info.plist` 中设 `LSUIElement = true`，同样是隐藏 Dock 图标。

> **实现修正（objc2 0.6）**：
> - `nextEventMatchingMask_...`、`sendEvent`、`setActivationPolicy`、
>   `finishLaunching` 都是**安全**函数，写 `unsafe {}` 会触发 `unused_unsafe` 警告。
> - 反过来，`NSDefaultRunLoopMode` 是 extern static，**读取它需要 `unsafe`**
>   （`let mode = unsafe { NSDefaultRunLoopMode };`）。
> - `MainThreadMarker::new()` 返回 `None`（非主线程）时不要 `expect` 直接 panic：
>   取不到事件只会让菜单无响应，不该让整个同步程序崩溃，记一条 warn 返回即可。

**怎么验证**：菜单栏出现剪贴板图标；点击能弹出菜单；"暂停同步"能勾选并生效
（日志出现"同步已暂停"）；"退出"能真正退出。

> 若无法用 Accessibility 权限自动点击菜单，可改为验证事件泵本身：
> `cargo run -p clipsync-app --example pump_check` 会投递一个
> `ApplicationDefined` 事件并确认能被取出派发，同时验证空队列时立即返回
> （实测 ~200µs，不阻塞 200ms 的托盘循环）。
> 另外 `CGWindowListCopyWindowInfo` 可看到 clipsync 在 layer 101 的菜单窗口；
> `osascript` 查 `background only is false` 的进程列表里不应有 clipsync
> （即 Accessory 策略生效、无 Dock 图标）。

---

## 6. 开机自启（已写，需验证）

**文件**：`crates/clipsync-app/src/autostart.rs` → `#[cfg(target_os = "macos")] mod platform`

已实现为在 `~/Library/LaunchAgents/com.clipsync.plist` 写入 LaunchAgent。
**从未在 Mac 上运行过**，请验证：

```bash
cargo run -- autostart on
cat ~/Library/LaunchAgents/com.clipsync.plist    # 应存在且路径正确
launchctl load ~/Library/LaunchAgents/com.clipsync.plist   # 或重启验证
cargo run -- autostart off
ls ~/Library/LaunchAgents/com.clipsync.plist     # 应已删除
```

可能需要调整的点：plist 里的可执行路径在开发期指向 `target/debug/clipsync`，
正式使用应指向安装后的位置或 `.app` 内的可执行文件。

> **实测结果（此前从未验证）**：全部通过。`plutil -lint` 语法正确；
> `launchctl load` 成功并真实拉起进程（PPID 1、`LastExitStatus = 0`）；
> `unload` 后进程停止；`autostart off` 删除 plist。
>
> 已按上述"可能需要调整的点"加了防护：`set_enabled(true)` 时若可执行文件
> 位于 `target/` 或 `deps/` 下会打一条 warn——`cargo clean` 或移动仓库会让
> 自启静默失效，而这种失败要到下次开机才会被发现。

---

## 7. 私钥文件权限（安全加固）

**文件**：`crates/clipsync-app/src/config.rs` → `load_or_init_identity`

`identity.json` 含 Noise 静态私钥（设备长期身份）。目前按默认权限创建。
macOS/Unix 上建议收紧为 `0600`：

```rust
#[cfg(unix)]
{
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&path)?.permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(&path, perms)?;
}
```

> **实现修正**：上面这种"先按默认权限创建、再 chmod"的写法会留下一个私钥
> 短暂可被其它用户读取的窗口。实际实现改为在**创建时**就带 0600 打开：
> `OpenOptions::new().write(true).create(true).truncate(true).mode(0o600)`。
> 加载既有文件时另外收紧一次，覆盖旧版本已用默认权限创建的情况。

---

## 验证清单（与 Windows 侧已通过的项目对齐）

补齐后请按此清单逐项验证，确保两平台行为一致。

### 单机

```bash
cargo test                    # 98 项应全过（含新增的跨进程文件列表回归测试）
cargo run -- addrs            # 列出本机地址，含 Tailscale/ZeroTier 等虚拟网卡
```

补齐过程中新增了两个手动验证小工具：

```bash
# 打印当前剪贴板的敏感标记 / 变化令牌 / 是否含图片 / 文件列表
cargo run -p clipsync-clip --example probe

# 把路径写入剪贴板（用于验证写入后能否被别的进程粘贴）
cargo run -p clipsync-clip --example write_files -- /path/a /path/b

# 验证 macOS 事件泵能取出并派发事件、且空队列不阻塞
cargo run -p clipsync-app --example pump_check
```

配合 Swift 单行脚本可覆盖各类剪贴板状态，例如构造带敏感标记的内容：

```bash
swift -e 'import AppKit
let pb = NSPasteboard.general
pb.clearContents()
pb.setString("fake-password", forType: .string)
pb.setData(Data(), forType: NSPasteboard.PasteboardType("org.nspasteboard.ConcealedType"))'
cargo run -p clipsync-clip --example probe   # sensitive 应为 true
```

### 双机（Mac ↔ Windows）

配对（任一方主持均可）：
```bash
# Mac 上
cargo run -- pair --host      # 显示 6 位配对码
# Windows 上（同一局域网，无需输入 IP）
clipsync pair <配对码>
# 若不在同一局域网，用 Mac 上打印出的地址：
clipsync pair <Mac的IP> <配对码>
```
配对成功后双方 `list` 应能看到对方及其地址。

然后各自启动 `clipsync`，依次验证：

| # | 操作 | 预期 |
|---|---|---|
| 1 | Mac 复制一段文字 | Windows 剪贴板同步出现 |
| 2 | Windows 复制一段文字 | Mac 同步出现 |
| 3 | 截图后复制图片 | 对端可粘贴出图片 |
| 4 | Finder 复制一个小文件 | 对端 Finder/资源管理器可粘贴出同名文件，内容一致 |
| 5 | 复制一个大文件（几十 MB），传输中立刻复制一段文字 | 文字**立即**同步；文件传输中止；日志显示"已收部分保留待续传" |
| 6 | 再次复制该大文件 | 日志显示"断点续传"，完成后文件内容与源一致（用 `md5` 比对） |
| 7 | 再复制一次同一文件 | 日志显示"缓存中命中，秒同步（零传输）" |
| 8 | 用密码管理器复制密码 | 日志显示"跳过 (Sensitive)"，对端**不应**收到 |
| 9 | 断开 Wi-Fi 只留 Tailscale | 同步应继续（走覆盖网地址） |
| 10 | 挂机十几分钟 | 连接保持，CPU 接近 0，内存不增长 |

### 参考：Windows 侧实测基线

| 指标 | Windows 实测 |
|---|---|
| 空闲 CPU | ≈0.1%（30 秒 0.03 秒） |
| 内存 | 12–15 MB |
| 传 90MB 文件时内存 | 恒定不涨 |
| 90MB 断点续传 | MD5 完全一致 |
| 重复内容秒同步 | 0.8 毫秒，零传输 |
| 连接稳定性 | 长时间 0 次断开 |

---

## 调试技巧

```bash
CLIPSYNC_LOG=debug cargo run                 # 详细日志
CLIPSYNC_CONFIG_DIR=/tmp/dev-a cargo run     # 隔离配置目录（一机多实例测试）
CLIPSYNC_NO_TRAY=1 cargo run                 # 禁用托盘，纯后台（排查 UI 问题时用）
CLIPSYNC_NO_WATCH=1 cargo run                # 纯接收模式（一机双实例时避免共用剪贴板打架）
CLIPSYNC_PEERS=1.2.3.4:47684 cargo run       # 手动指定对端地址
```

**一机双实例测试法**（在一台 Mac 上模拟两台设备）：
```bash
# 实例 A（发送方）
CLIPSYNC_CONFIG_DIR=/tmp/dev-a cargo run
# 实例 B（纯接收，避免与 A 抢同一个系统剪贴板）
CLIPSYNC_CONFIG_DIR=/tmp/dev-b CLIPSYNC_NO_WATCH=1 CLIPSYNC_NO_TRAY=1 cargo run
```
注意：两个实例需用不同 `listen_port`（改各自 `settings.json`）；第二个实例的
组播信标端口会冲突并打印警告，属正常，此时依赖配对时交换的地址连接。

---

## macOS 特有注意事项

- **工具链安装**：若 `static.rust-lang.org` 连不上（TLS handshake eof，国内常见），
  用镜像装 rustup 并配置 cargo 源，否则 `cargo build` 会卡在拉取 crates.io：
  ```bash
  export RUSTUP_DIST_SERVER=https://rsproxy.cn RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  # ~/.cargo/config.toml
  # [source.crates-io]
  # replace-with = "rsproxy-sparse"
  # [source.rsproxy-sparse]
  # registry = "sparse+https://rsproxy.cn/index/"
  ```
- **`timeout` 命令不存在**：macOS 自带的 coreutils 没有 `timeout`。在脚本里用它
  包裹命令会得到 "command not found"，而**被包裹的命令根本没执行**——很容易
  把这个错误当成命令本身失败（例如误判"仓库是空的"）。需要时装
  `brew install coreutils` 用 `gtimeout`。
- **防火墙**：首次监听端口时系统会弹窗询问是否允许接受连接，需点允许。
- **剪贴板权限**：读写剪贴板本身不需要特殊授权（不同于辅助功能/屏幕录制）。
- **截图验证的坑**：用 `screencapture -c` 往剪贴板抓图需要"屏幕录制"权限，
  未授权时报 "could not create image from rect"。验证图片探测不必依赖它——
  用 Swift 往 pasteboard 写一个 `NSImage` 即可，无需任何权限。
  另外实测发现：写入 PNG 后 macOS 会**自动合成** `public.tiff`，所以
  `has_image()` 判断 PNG/TIFF 与 arboard 只读 TIFF 并不冲突。
- **打包 `.app`**：托盘程序应设 `LSUIElement = true` 隐藏 Dock 图标；分发给他人
  需要签名与公证，自用可在"隐私与安全性"中放行。
- **架构**：Apple Silicon 上默认编译 `aarch64-apple-darwin`；若需通用二进制，
  用 `lipo` 合并 x86_64 与 aarch64 两份产物。

---

## 有问题时

每处待实现点在代码里都有 `TODO(macos)` 或说明性注释，可直接搜索定位：

```bash
grep -rn "TODO(macos)\|待具备 macOS" crates/
```

补齐顺序建议：**2（安全）→ 1（性能）→ 5（托盘可用）→ 4（文件）→ 3 → 6 → 7**。
先做敏感内容探测是因为它关系到密码不被误同步。
