//! 无图形环境时的退化实现：只记日志。

use anyhow::Result;


pub fn show(title: &str, body: &str) -> Result<()> {
    // 无统一的弹窗方案，退化为日志。
    tracing::info!("[{title}] {body}");
    Ok(())
}

pub fn prompt(title: &str, body: &str) -> Result<Option<String>> {
    // 无图形环境可用，调用方会退回命令行路径。
    tracing::info!("[{title}] {body}（本平台无输入框，请用命令行 `clipsync pair <配对码>`）");
    Ok(None)
}

pub fn confirm(title: &str, body: &str) -> Result<bool> {
    // 无从征得同意，一律当作否——破坏性操作不能默认执行。
    tracing::info!("[{title}] {body}（本平台无确认框，按取消处理）");
    Ok(false)
}
