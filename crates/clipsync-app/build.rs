//! 构建脚本：把应用清单嵌进 Windows 可执行文件。
//!
//! 清单声明了通用控件 v6（`TaskDialogIndirect` 只在 v6 里）与 DPI 感知，
//! 详见 `clipsync.manifest`。
//!
//! **为什么用链接器参数而不是 `embed-resource`/`winres` 之类的 crate**：
//! 那些要么依赖资源编译器（`rc.exe`/`windres`），要么再拖进一串构建期依赖。
//! MSVC 链接器本身就认 `/MANIFEST:EMBED`，两行参数解决，不给项目增加负担。

fn main() {
    println!("cargo:rerun-if-changed=clipsync.manifest");

    // 注意判断的是**目标**平台而非构建机：在 macOS 上交叉检查 Windows 目标
    // 时，`cfg!(windows)` 是 false，会漏掉这段。
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os != "windows" || target_env != "msvc" {
        return;
    }

    let manifest = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("clipsync.manifest");

    // 只作用于可执行文件：测试与构建脚本自身不需要清单，给它们加反而会在
    // 某些链接场景下报错。
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
        manifest.display()
    );
}
