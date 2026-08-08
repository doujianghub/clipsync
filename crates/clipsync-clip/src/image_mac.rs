//! macOS 上从 `NSPasteboard` 直接读取图片，并规范化为 8-bit RGBA。
//!
//! **为什么不直接用 arboard**：`arboard` 把剪贴板里的 TIFF 交给 `image` crate
//! 解码，而 macOS 上相当一部分图片是 **16-bit 浮点 TIFF**（SampleFormat 3）——
//! Retina 屏上任何经 `NSImage.lockFocus` 绘制再复制的图片都是这种格式。
//! `image` 0.25 不支持浮点 TIFF，`get_image()` 直接返回 `ConversionFailure`，
//! 该图片同步**彻底失败**。
//!
//! AppKit 自己解得了这些格式（它本来就是产出方），所以这里绕开 `image`：
//! 用 `NSBitmapImageRep` 拿到像素，再让 AppKit 把它**重绘**到一块规格固定的
//! 8-bit RGBA 画布上。
//!
//! **为什么一律重绘、而不是"只在需要时转换"**：源位图的位深、通道顺序
//! （`AlphaFirst` 即 ARGB）、alpha 是否预乘、行间是否有 padding
//! （`bytesPerRow > width*4`）、色彩空间，各维度都可能不同，组合起来的分支
//! 数量远超收益。让 AppKit 重绘一次就把所有维度一次性归一，代价是一次绘制
//! ——对"用户按了一次复制"这种频率完全不值一提，换来的是不必自己实现
//! 通道重排与去预乘（那里每一步都能写出难查的颜色 bug）。

use clipsync_core::ImageData;
use objc2::rc::Retained;
use objc2::AnyThread;
use objc2_app_kit::{
    NSBitmapFormat, NSBitmapImageRep, NSDeviceRGBColorSpace, NSGraphicsContext, NSPasteboard,
    NSPasteboardTypePNG, NSPasteboardTypeTIFF,
};
use objc2_foundation::{NSPoint, NSRect, NSSize};

/// 剪贴板图片的像素上限（宽 × 高）。
///
/// 规范化会按 `宽 × 高 × 4` 字节分配一块画布。剪贴板里的图片尺寸由其它程序
/// 决定，不设上限的话，一张异常巨大的图能让我们直接分配掉几个 GB。
/// 8000 万像素 ≈ 320 MB 画布，已远超任何正常截图；超出则放弃读取，
/// 让上层退回文本路径，而不是把内存打爆。
const MAX_PIXELS: usize = 80_000_000;

/// 从剪贴板读取图片并规范化为 8-bit RGBA。
///
/// `None` 表示剪贴板里没有图片、或 AppKit 也解不了——两种情况调用方都应
/// 退回原有的 `arboard` 路径，本模块只做"补上 arboard 解不了的那部分"。
pub fn read_image_rgba() -> Option<ImageData> {
    let pb = NSPasteboard::generalPasteboard();

    // TIFF 优先：macOS 剪贴板里图片的通用表示，PNG 只有部分程序会同时提供。
    // SAFETY: AppKit 导出的常量类型标识，读取始终有效。
    let data = unsafe {
        pb.dataForType(NSPasteboardTypeTIFF)
            .or_else(|| pb.dataForType(NSPasteboardTypePNG))
    }?;

    let src = NSBitmapImageRep::imageRepWithData(&data)?;
    let width = src.pixelsWide();
    let height = src.pixelsHigh();
    if width <= 0 || height <= 0 {
        return None;
    }
    let (w, h) = (width as usize, height as usize);
    if w.checked_mul(h).is_none_or(|px| px > MAX_PIXELS) {
        tracing::warn!("剪贴板图片 {w}×{h} 超出可处理尺寸，跳过");
        return None;
    }

    let dst = make_rgba8_canvas(width, height)?;
    draw_onto(&src, &dst, width, height)?;
    copy_out(&dst, w, h)
}

/// 造一块 8-bit、**预乘** RGBA、无行间 padding 的画布。
///
/// `planes = null` 让 AppKit 自行分配像素缓冲，其生命周期随 `dst` 走。
///
/// **为什么是预乘**：`ImageData.rgba` 要的是非预乘（与 arboard 一致），但画布
/// 不能直接建成非预乘——`graphicsContextWithBitmapImageRep` 底层是
/// CGBitmapContext，而 Core Graphics **只支持预乘 alpha**，传
/// `AlphaNonpremultiplied` 会让它返回 `nil`（实测如此，且不给任何错误原因，
/// 表现为"画布建得出来、上下文却是空的"）。所以这里按 CG 能接受的格式绘制，
/// 去预乘留到 [`copy_out`] 里做。
fn make_rgba8_canvas(width: isize, height: isize) -> Option<Retained<NSBitmapImageRep>> {
    // SAFETY: planes 传 null 是文档允许的用法（要求 AppKit 自行分配）；
    // 其余参数自洽——bytesPerRow = 宽 × 4 与 bitsPerPixel = 32 与
    // "8 位 × 4 通道、非平面" 一致。
    unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bitmapFormat_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            width,
            height,
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            NSBitmapFormat::empty(),
            width * 4,
            32,
        )
    }
}

/// 把源位图重绘到画布上，完成位深/通道顺序/色彩空间的归一。
fn draw_onto(
    src: &NSBitmapImageRep,
    dst: &NSBitmapImageRep,
    width: isize,
    height: isize,
) -> Option<()> {
    let ctx = NSGraphicsContext::graphicsContextWithBitmapImageRep(dst)?;

    // 存档/还档成对出现：绘制会改动全局的"当前上下文"，不还原会影响
    // 同进程内其它 AppKit 绘制（托盘图标也走 AppKit）。
    NSGraphicsContext::saveGraphicsState_class();
    NSGraphicsContext::setCurrentContext(Some(&ctx));
    let ok = src.drawInRect(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(width as f64, height as f64),
    ));
    NSGraphicsContext::restoreGraphicsState_class();

    ok.then_some(())
}

/// 把画布像素拷成 `ImageData`。
fn copy_out(dst: &NSBitmapImageRep, w: usize, h: usize) -> Option<ImageData> {
    let ptr = dst.bitmapData();
    if ptr.is_null() {
        return None;
    }
    // 画布是我们按 `宽 × 4` 建的，但仍以 AppKit 报告的行距为准读取——
    // 若它出于对齐考虑加了 padding，按理想值读会整幅图斜掉。
    let stride = dst.bytesPerRow() as usize;
    let row_bytes = w.checked_mul(4)?;
    if stride < row_bytes {
        return None;
    }

    let mut rgba = Vec::with_capacity(row_bytes.checked_mul(h)?);
    for y in 0..h {
        // SAFETY: AppKit 保证缓冲区至少 stride × h 字节；每行只读前 row_bytes 个。
        let row =
            unsafe { std::slice::from_raw_parts(ptr.add(y * stride) as *const u8, row_bytes) };
        rgba.extend_from_slice(row);
    }

    unpremultiply(&mut rgba);

    Some(ImageData {
        width: w as u32,
        height: h as u32,
        rgba,
    })
}

/// 把预乘 alpha 的像素还原为直白 RGBA。
///
/// 画布必须是预乘的（Core Graphics 的硬性要求，见 [`make_rgba8_canvas`]），
/// 而 `ImageData.rgba` 的约定是非预乘——与 arboard 在其它路径上给出的
/// 数据保持一致。不还原的话，半透明像素传到对端会明显偏暗。
///
/// `a == 0` 的像素颜色分量已被抹成 0，无从还原，保持全透明即可；
/// `a == 255` 无需换算，跳过省一遍除法（截图这类不透明图片走的正是这条路）。
fn unpremultiply(rgba: &mut [u8]) {
    for px in rgba.chunks_exact_mut(4) {
        let a = px[3] as u32;
        if a == 0 || a == 255 {
            continue;
        }
        for c in &mut px[..3] {
            // +a/2 做四舍五入，避免整幅图系统性偏暗一档。
            *c = (((*c as u32) * 255 + a / 2) / a).min(255) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unpremultiply_restores_straight_alpha() {
        // 半透明红：预乘后 r = 255 × 0.5 ≈ 128，还原应回到接近 255。
        let mut px = vec![128, 0, 0, 128];
        unpremultiply(&mut px);
        assert!(px[0] >= 250, "红通道应还原到接近满值，实际 {}", px[0]);
        assert_eq!(px[3], 128, "alpha 本身不参与换算");

        // 不透明与全透明是快路径，必须原样保留。
        let mut opaque = vec![10, 20, 30, 255];
        unpremultiply(&mut opaque);
        assert_eq!(opaque, vec![10, 20, 30, 255]);

        let mut clear = vec![0, 0, 0, 0];
        unpremultiply(&mut clear);
        assert_eq!(clear, vec![0, 0, 0, 0]);
    }

    /// 还原不得溢出：预乘值理论上不会超过 alpha，但剪贴板数据来自其它程序，
    /// 不能假定它一定自洽——夹到 255 而不是回绕成一个乱七八糟的暗色。
    #[test]
    fn unpremultiply_clamps_inconsistent_input() {
        let mut px = vec![255, 255, 255, 1];
        unpremultiply(&mut px);
        assert_eq!(&px[..3], &[255, 255, 255]);
    }

    /// 空剪贴板（或无图片）时应安静地返回 `None`，让调用方退回文本路径。
    #[test]
    fn no_image_returns_none() {
        let _guard = crate::clipboard_test_lock();
        let pb = NSPasteboard::generalPasteboard();
        pb.clearContents();
        assert!(read_image_rgba().is_none());
    }

    /// 回归：16-bit **浮点** TIFF 必须能读出来。
    ///
    /// 这正是 arboard 走不通的那条路——它内部的 `image` 0.25 解不了浮点采样，
    /// 返回 `ConversionFailure`，该图片同步彻底失败。Retina 屏上经
    /// `NSImage.lockFocus` 绘制再复制的图片都是这个格式，并非边角情况。
    ///
    /// 用 `swift` 现场造一张这样的图放进剪贴板；没有 swift 工具链就跳过。
    #[test]
    fn reads_sixteen_bit_float_tiff() {
        let _guard = crate::clipboard_test_lock();

        let script = r#"import AppKit
let img = NSImage(size: NSSize(width: 40, height: 30))
img.lockFocus()
NSColor(srgbRed: 0.0, green: 0.0, blue: 1.0, alpha: 1.0).setFill()
NSRect(x: 0, y: 0, width: 40, height: 30).fill()
img.unlockFocus()
NSPasteboard.general.clearContents()
NSPasteboard.general.writeObjects([img])
"#;
        let out = std::process::Command::new("swift")
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
        match out {
            Ok(s) if s.success() => {}
            _ => {
                eprintln!("跳过：swift 不可用，无法造出 16-bit 浮点 TIFF");
                return;
            }
        }

        // 前置条件：确认剪贴板里的确是 arboard 解不了的那种图。测不到这一点
        // 的话，本用例可能只是在验证一张普通 8-bit 图，起不到回归作用。
        let pb = NSPasteboard::generalPasteboard();
        // SAFETY: AppKit 导出的常量类型标识。
        let raw = unsafe { pb.dataForType(NSPasteboardTypeTIFF) };
        let Some(raw) = raw else {
            eprintln!("跳过：剪贴板里没有 TIFF");
            return;
        };
        let rep = NSBitmapImageRep::imageRepWithData(&raw).expect("AppKit 应能解析自己写的 TIFF");
        let is_float = rep
            .bitmapFormat()
            .contains(NSBitmapFormat::FloatingPointSamples);
        if !is_float {
            eprintln!(
                "跳过：本机产出的是 {} bps 非浮点图，构造不出目标场景",
                rep.bitsPerSample()
            );
            return;
        }

        // 尺寸以 rep 报告的**像素**数为准，不能硬编码 NSImage 的 40×30：
        // Retina 屏上 backingScaleFactor = 2，实际位图是 80×60。
        let (ew, eh) = (rep.pixelsWide() as u32, rep.pixelsHigh() as u32);

        let img = read_image_rgba().expect("16-bit 浮点 TIFF 必须能读出——这正是本修复的目标");
        assert_eq!((img.width, img.height), (ew, eh));
        assert_eq!(
            img.rgba.len(),
            (ew * eh * 4) as usize,
            "应为 8-bit RGBA，每像素 4 字节"
        );

        // 画的是纯蓝，规范化后应仍是纯蓝且完全不透明。允许少量色彩空间误差。
        let px = &img.rgba[..4];
        assert!(px[2] > 200, "蓝通道应接近满值，实际 {:?}", px);
        assert!(px[0] < 60 && px[1] < 60, "红/绿通道应接近 0，实际 {:?}", px);
        assert_eq!(px[3], 255, "alpha 应为不透明");
    }
}
