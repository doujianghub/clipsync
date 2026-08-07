//! 基于 `arboard` 的跨平台剪贴板后端（文本 + 图片）。
//!
//! `arboard` 在 Windows 与 macOS 上均可用，提供文本与 RGBA 图片的读写。
//! 文件列表 arboard 不覆盖，将在 M4 用平台特定代码补充。
//!
//! 变化监听：`arboard` 不提供事件，本模块用一个轻量轮询监听器
//! （`PollingWatcher`）比较连续两次读取的内容哈希来发现变化。M1b 将在
//! Windows 上改用系统序列号信号（`AddClipboardFormatListener`）以进一步
//! 降低开销；macOS 则改用 `NSPasteboard.changeCount`。当前轮询实现保证
//! 两平台均可编译运行。

use std::borrow::Cow;
use std::time::Duration;

use anyhow::{Context, Result};
use clipsync_core::{ClipContent, ImageData};

use crate::{ClipRead, Clipboard, ClipboardWatcher};

/// arboard 剪贴板后端。
pub struct ArboardClipboard {
    inner: arboard::Clipboard,
}

/// 剪贴板被占用时的重试次数与间隔。
///
/// Windows/macOS 的剪贴板是独占资源：任何程序写入时都会短暂持锁（浏览器、
/// Office、密码管理器都如此）。不重试会造成两个后果——本次变化漏同步，以及
/// 我们的访问直接失败。锁通常只持有几十毫秒，短间隔重试即可化解；总等待
/// 上限约 300ms，对资源占用无实质影响。
const RETRY_ATTEMPTS: u32 = 12;
const RETRY_DELAY: Duration = Duration::from_millis(25);

/// 去抖动：检测到剪贴板变化后先等这么久，确认没有后续变化再读取。
/// 取 120ms——足以覆盖常见程序写入多种格式的间隔，用户又感知不到延迟。
const DEBOUNCE_DELAY: Duration = Duration::from_millis(120);
/// 去抖动最多等待的轮数，防止某些程序持续改写导致永不触发同步。
const DEBOUNCE_MAX_ROUNDS: u32 = 5;

/// 在"剪贴板被占用"时重试给定操作；其它错误立即返回。
fn retry_on_occupied<T>(
    mut op: impl FnMut() -> std::result::Result<T, arboard::Error>,
) -> std::result::Result<T, arboard::Error> {
    for attempt in 0..RETRY_ATTEMPTS {
        match op() {
            Err(arboard::Error::ClipboardOccupied) => {
                if attempt + 1 < RETRY_ATTEMPTS {
                    std::thread::sleep(RETRY_DELAY);
                }
            }
            other => return other,
        }
    }
    Err(arboard::Error::ClipboardOccupied)
}

impl ArboardClipboard {
    pub fn new() -> Result<Self> {
        let inner = arboard::Clipboard::new().context("初始化 arboard 剪贴板失败")?;
        Ok(Self { inner })
    }
}

impl Clipboard for ArboardClipboard {
    fn read(&mut self) -> Result<Option<ClipRead>> {
        // 敏感标记独立于内容类型，先探测一次（不打开剪贴板，开销极小）。
        let sensitive = crate::sensitive::clipboard_is_sensitive();

        // 文件优先：资源管理器复制文件时往往同时提供文件列表与一份文本表示
        // （文件名/路径）。用户的意图是文件，故先看有没有文件列表。
        match crate::filelist::read_file_paths() {
            Ok(Some(paths)) => {
                // 元数据与路径必须严格一一对应：上层用下标把两者配对起来
                // （`hub` 里的 `metas.zip(file_paths)`），错位会让对端拿着
                // A 文件的标识去接收 B 文件的字节，表现为校验失败或内容错乱。
                //
                // 因此在同一次循环里成对收集，而不是先收元数据、再按文件名
                // 反查路径——按名字反查在**不同目录下的同名文件**上会失效：
                // 两条路径的 `file_name()` 相同，只要其中一个取元数据失败，
                // 反查就会把两条路径都保留，从此与元数据错位。
                let mut metas = Vec::with_capacity(paths.len());
                let mut kept = Vec::with_capacity(paths.len());
                for p in paths {
                    match crate::filelist::meta_for_path(&p) {
                        Ok(m) => {
                            metas.push(m);
                            kept.push(p);
                        }
                        // 单个文件读不到信息（已删除/无权限）不应毁掉整次同步，
                        // 跳过它并记录。
                        Err(e) => tracing::debug!("跳过无法读取的文件 {}: {e:#}", p.display()),
                    }
                }
                if !metas.is_empty() {
                    debug_assert_eq!(metas.len(), kept.len(), "元数据与路径必须等长");
                    return Ok(Some(ClipRead {
                        content: ClipContent::Files(metas),
                        sensitive,
                        file_paths: kept,
                    }));
                }
            }
            Ok(None) => { /* 没有文件，继续看图片/文本 */ }
            Err(e) => tracing::debug!("读取剪贴板文件列表失败: {e:#}"),
        }

        // 先用廉价查询判断是否真有图片，避免无谓地打开剪贴板与其它程序抢锁。
        let try_image = crate::formats::clipboard_has_image().unwrap_or(true);

        if try_image {
            match retry_on_occupied(|| self.inner.get_image()) {
                Ok(img) => {
                    let content = arboard_image_to_content(img);
                    return Ok(Some(ClipRead::simple(content, sensitive)));
                }
                Err(arboard::Error::ContentNotAvailable) => { /* 退回读取文本 */ }
                Err(e) => {
                    // arboard 解不了这张图。macOS 上这不是罕见情况：Retina 屏
                    // 上经 `NSImage.lockFocus` 绘制再复制的图片是 16-bit 浮点
                    // TIFF，arboard 内部的 `image` crate 不支持该采样格式，
                    // 返回 ConversionFailure。让 AppKit 自己把它规范化为
                    // 8-bit RGBA 再取一次。
                    if let Some(img) = platform_image_fallback() {
                        tracing::debug!(
                            "arboard 读图失败（{e}），已由平台规范化路径取到 {}×{}",
                            img.width,
                            img.height
                        );
                        return Ok(Some(ClipRead::simple(ClipContent::Image(img), sensitive)));
                    }
                    return Err(e).context("读取剪贴板图片失败");
                }
            }
        }

        match retry_on_occupied(|| self.inner.get_text()) {
            Ok(text) => Ok(Some(ClipRead::simple(ClipContent::Text(text), sensitive))),
            Err(arboard::Error::ContentNotAvailable) => Ok(None),
            Err(e) => Err(e).context("读取剪贴板文本失败"),
        }
    }

    fn write(&mut self, content: &ClipContent) -> Result<()> {
        match content {
            ClipContent::Text(s) => retry_on_occupied(|| self.inner.set_text(s.clone()))
                .context("写入剪贴板文本失败"),
            ClipContent::Image(img) => {
                retry_on_occupied(|| self.inner.set_image(content_image_to_arboard(img)))
                    .context("写入剪贴板图片失败")
            }
            // 文件需要真实的本机路径才能写入剪贴板，而 `ClipContent` 只有元数据。
            // 上层在文件内容落地后调用 `write_files` 写入实际路径。
            ClipContent::Files(_) => {
                anyhow::bail!("文件需经 write_files 写入（须提供落地后的本机路径）")
            }
        }
    }
}

impl ArboardClipboard {
    /// 把一组已落地的本机文件写入剪贴板，使其可被正常粘贴。
    pub fn write_files(&mut self, paths: &[std::path::PathBuf]) -> Result<()> {
        crate::filelist::write_file_paths(paths)
    }
}

/// arboard 解不了图片时的平台兜底。仅 macOS 有实现，其余平台恒为 `None`
/// （Windows 上 arboard 走 DIB，不存在这类采样格式问题）。
#[cfg(target_os = "macos")]
fn platform_image_fallback() -> Option<ImageData> {
    crate::image_mac::read_image_rgba()
}

#[cfg(not(target_os = "macos"))]
fn platform_image_fallback() -> Option<ImageData> {
    None
}

/// 把 arboard 图片转为核心 `ImageData`（均为 RGBA8）。
fn arboard_image_to_content(img: arboard::ImageData<'_>) -> ClipContent {
    ClipContent::Image(ImageData {
        width: img.width as u32,
        height: img.height as u32,
        rgba: img.bytes.into_owned(),
    })
}

/// 把核心 `ImageData` 转为 arboard 图片。
fn content_image_to_arboard(img: &ImageData) -> arboard::ImageData<'static> {
    arboard::ImageData {
        width: img.width as usize,
        height: img.height as usize,
        bytes: Cow::Owned(img.rgba.clone()),
    }
}

/// 轮询式剪贴板监听器。
///
/// 优先使用平台提供的廉价变化令牌（Windows 的剪贴板序列号）判断"是否变化"，
/// 仅在无令牌的平台才回退到"读取内容并比较哈希"。间隔取 500ms，在"无感"
/// 与"低占用"之间平衡：有令牌时每轮仅比较一个整数，开销可忽略。
///
/// 注意：令牌对"任何变化"递增，包括本程序自己写入剪贴板；重复/回环的抑制
/// 由上层同步引擎在内容层面完成，监听器只负责廉价地发现"发生了变化"。
pub struct PollingWatcher {
    clipboard: ArboardClipboard,
    interval: Duration,
    /// 平台廉价令牌的上次值（若平台支持）。
    last_token: Option<u64>,
    /// 回退路径下的上次内容哈希（无令牌平台使用）。
    last_hash: Option<u64>,
}

impl PollingWatcher {
    pub fn new() -> Result<Self> {
        Ok(Self {
            clipboard: ArboardClipboard::new()?,
            interval: Duration::from_millis(500),
            last_token: None,
            last_hash: None,
        })
    }

    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }
}

impl ClipboardWatcher for PollingWatcher {
    fn run(&mut self, on_change: &mut dyn FnMut() -> bool) -> Result<()> {
        // 首轮记录初始令牌，避免把"启动时已有内容"误报为一次新变化。
        self.last_token = crate::change_token::clipboard_change_token();

        loop {
            let changed = match crate::change_token::clipboard_change_token() {
                // 有廉价令牌：仅比较整数，不读取内容。
                Some(token) => {
                    if self.last_token == Some(token) {
                        false
                    } else {
                        // 一次复制常触发多次令牌变化（程序会依次放入多种格式，
                        // 如同时提供文本与 HTML）。先等令牌稳定再通知，把它们
                        // 合并为一次读取——既减少无谓的剪贴板打开（降低与其它
                        // 程序抢锁的概率），也避免重复处理。
                        self.last_token = Some(self.wait_for_stable_token(token));
                        true
                    }
                }
                // 无令牌：回退到读取内容并比较哈希。
                None => self.detect_change_by_hash(),
            };

            if changed && !on_change() {
                return Ok(());
            }

            std::thread::sleep(self.interval);
        }
    }
}

impl PollingWatcher {
    /// 等待剪贴板令牌不再变化，返回最终令牌。
    ///
    /// 每次等待 [`DEBOUNCE_DELAY`]；若期间令牌又变了说明写入仍在进行，继续等，
    /// 最多 [`DEBOUNCE_MAX_ROUNDS`] 轮以防某些程序持续改写导致永不触发。
    fn wait_for_stable_token(&self, mut token: u64) -> u64 {
        for _ in 0..DEBOUNCE_MAX_ROUNDS {
            std::thread::sleep(DEBOUNCE_DELAY);
            match crate::change_token::clipboard_change_token() {
                Some(t) if t != token => token = t, // 仍在变化，继续等
                _ => break,                         // 已稳定
            }
        }
        token
    }

    /// 回退路径：读取内容并与上次哈希比较，判断是否变化。
    /// 仅在无平台令牌时使用。返回是否发生了"有内容的变化"。
    fn detect_change_by_hash(&mut self) -> bool {
        let current_hash = match self.clipboard.read() {
            Ok(Some(read)) => Some(read.content.content_hash()),
            Ok(None) => None,
            Err(e) => {
                tracing::debug!("轮询读取剪贴板失败（本轮跳过）: {e:#}");
                return false;
            }
        };

        if current_hash != self.last_hash {
            self.last_hash = current_hash;
            // 仅在有内容时视为需通知的变化（内容被清空不触发同步）。
            return current_hash.is_some();
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 端到端回归：arboard 解不了的图片，整条读取链路也必须能拿到。
    ///
    /// 与 `image_mac` 里那条的区别在于测的是**用户可见行为**：那条只证明兜底
    /// 函数本身能工作，这条证明 `read()` 真的会在 arboard 失败时走到兜底——
    /// 接错线的话那条照样绿，用户的图片照样同步不过去。
    ///
    /// 场景是 Retina 屏上经 `NSImage.lockFocus` 绘制再复制的图片，格式为
    /// 16-bit 浮点 TIFF，arboard 内部的 `image` 0.25 返回 `ConversionFailure`。
    ///
    /// > 验证这条时踩过一个坑，值得记下来：曾用一个独立小程序直接调
    /// > `arboard::get_image()` 做对照，得到"能解"的结论，差点据此判定缺口
    /// > 已消失并删掉整个兜底。实际原因是当时**后台还跑着两个 clipsync 实例**
    /// > ——它们把图片同步一圈后用 arboard 写回了剪贴板，对照程序读到的是
    /// > 已被转成 8-bit 的版本。做剪贴板对照实验前务必确认没有本程序在跑。
    #[cfg(target_os = "macos")]
    #[test]
    fn float_tiff_image_is_readable_end_to_end() {
        let _guard = crate::clipboard_test_lock();

        let script = r#"import AppKit
let img = NSImage(size: NSSize(width: 40, height: 30))
img.lockFocus()
NSColor(srgbRed: 0.0, green: 1.0, blue: 0.0, alpha: 1.0).setFill()
NSRect(x: 0, y: 0, width: 40, height: 30).fill()
img.unlockFocus()
NSPasteboard.general.clearContents()
NSPasteboard.general.writeObjects([img])
"#;
        let spawned = std::process::Command::new("swift")
            .arg("-")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .and_then(|mut c| {
                use std::io::Write;
                c.stdin.take().unwrap().write_all(script.as_bytes())?;
                c.wait()
            });
        match spawned {
            Ok(s) if s.success() => {}
            _ => {
                eprintln!("跳过：swift 不可用");
                return;
            }
        }

        // 前置条件：确认剪贴板里**真是**浮点 TIFF。少了这一步，本用例可能只是
        // 在验证一张普通 8-bit 图，看着绿油油却守不住任何东西。
        {
            use objc2_app_kit::{NSBitmapFormat, NSBitmapImageRep, NSPasteboard, NSPasteboardTypeTIFF};
            let pb = NSPasteboard::generalPasteboard();
            // SAFETY: AppKit 导出的常量类型标识。
            let Some(raw) = (unsafe { pb.dataForType(NSPasteboardTypeTIFF) }) else {
                eprintln!("跳过：剪贴板里没有 TIFF");
                return;
            };
            let rep = NSBitmapImageRep::imageRepWithData(&raw).expect("AppKit 应能解析自己写的 TIFF");
            if !rep
                .bitmapFormat()
                .contains(NSBitmapFormat::FloatingPointSamples)
            {
                eprintln!(
                    "跳过：本机产出的是 {} bps 非浮点图，构造不出目标场景",
                    rep.bitsPerSample()
                );
                return;
            }
        }

        let Ok(mut cb) = ArboardClipboard::new() else {
            eprintln!("跳过：无可用剪贴板");
            return;
        };
        let read = cb
            .read()
            .expect("读取不应报错——arboard 解不了时应由平台路径兜底")
            .expect("应读到内容");

        match read.content {
            ClipContent::Image(img) => {
                assert!(img.width > 0 && img.height > 0);
                assert_eq!(img.rgba.len(), (img.width * img.height * 4) as usize);
                // 画的是纯绿；顺带守住兜底路径没把通道顺序搞反。
                let px = &img.rgba[..4];
                assert!(px[1] > 200, "绿通道应接近满值，实际 {px:?}");
                assert!(px[0] < 60 && px[2] < 60, "红/蓝通道应接近 0，实际 {px:?}");
            }
            other => panic!("应读到图片，实际是 {other:?}——兜底未接上，图片会同步失败"),
        }
    }

    /// 回归：元数据与路径必须严格一一对应，即便中途有文件读不到。
    ///
    /// 触发条件是**不同目录下的同名文件**：旧实现先收集元数据，再按
    /// `file_name()` 反查该保留哪些路径。两条路径的文件名相同，只要其中
    /// 一个取元数据失败，反查就会把两条都留下——于是 1 条元数据配 2 条路径。
    /// 上层 `hub` 用 `zip` 配对，静默取前 1 组，结果是拿着 B 文件的标识
    /// （大小/哈希）去发送 A 文件的字节，对端校验失败。
    ///
    /// 需要真实剪贴板，故限定 macOS；无 GUI 会话时自动跳过。
    #[cfg(target_os = "macos")]
    #[test]
    fn same_named_files_keep_meta_and_path_aligned() {
        let _guard = crate::clipboard_test_lock();
        let root = std::env::temp_dir().join("clipsync_align_test");
        let _ = std::fs::remove_dir_all(&root);
        let (da, db) = (root.join("a"), root.join("b"));
        std::fs::create_dir_all(&da).unwrap();
        std::fs::create_dir_all(&db).unwrap();

        // 同名、不同目录、不同大小——大小差异让错配一眼可辨。
        let (pa, pb) = (da.join("x.txt"), db.join("x.txt"));
        std::fs::write(&pa, b"a").unwrap();
        std::fs::write(&pb, b"bb").unwrap();

        crate::filelist::write_file_paths(&[pa.clone(), pb.clone()]).unwrap();

        // 让第一个文件在"复制之后、读取之前"消失——这正是取元数据会失败的
        // 真实情形（用户复制后删掉了文件）。
        std::fs::remove_file(&pa).unwrap();

        let Ok(mut cb) = ArboardClipboard::new() else {
            eprintln!("跳过：无可用剪贴板（无 GUI 会话）");
            return;
        };
        let read = cb.read().expect("读取不应出错").expect("应读到文件");

        let ClipContent::Files(metas) = &read.content else {
            panic!("应读到文件列表，实际 {:?}", read.content);
        };
        assert_eq!(
            metas.len(),
            read.file_paths.len(),
            "元数据 {} 条、路径 {} 条——两者错位后上层 zip 会静默配错文件",
            metas.len(),
            read.file_paths.len()
        );
        assert_eq!(metas.len(), 1, "只有一个文件仍存在");
        assert_eq!(read.file_paths[0], pb, "保留的应是仍存在的那条路径");
        assert_eq!(metas[0].size, 2, "元数据应描述 b/x.txt（2 字节）而非已删除的 a/x.txt");

        let _ = std::fs::remove_dir_all(&root);
    }
}
