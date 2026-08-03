//! 内存 stub 剪贴板后端。
//!
//! 用于在未接入平台原生实现前，让上层可编译、可测试同步流程。
//! 它把"系统剪贴板"模拟为进程内的一块共享内存。

use std::sync::{Arc, Mutex};

use anyhow::Result;
use clipsync_core::ClipContent;

use crate::{ClipRead, Clipboard};

/// 进程内共享的模拟剪贴板存储。
#[derive(Debug, Clone, Default)]
pub struct StubClipboard {
    inner: Arc<Mutex<Option<ClipContent>>>,
}

impl StubClipboard {
    pub fn new() -> Self {
        Self::default()
    }

    /// 测试辅助：直接设置内容（模拟用户复制）。
    pub fn set(&self, content: ClipContent) {
        *self.inner.lock().unwrap() = Some(content);
    }

    /// 测试辅助：读取当前内容。
    pub fn get(&self) -> Option<ClipContent> {
        self.inner.lock().unwrap().clone()
    }
}

impl Clipboard for StubClipboard {
    fn read(&mut self) -> Result<Option<ClipRead>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .clone()
            .map(|content| ClipRead::simple(content, false)))
    }

    fn write(&mut self, content: &ClipContent) -> Result<()> {
        *self.inner.lock().unwrap() = Some(content.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_read_roundtrips() {
        let mut cb = StubClipboard::new();
        cb.write(&ClipContent::Text("hi".into())).unwrap();
        let r = cb.read().unwrap().unwrap();
        assert_eq!(r.content, ClipContent::Text("hi".into()));
        assert!(!r.sensitive);
    }

    #[test]
    fn empty_reads_none() {
        let mut cb = StubClipboard::new();
        assert!(cb.read().unwrap().is_none());
    }
}
