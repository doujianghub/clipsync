//! ClipSync 核心：平台无关的类型与同步逻辑。
//!
//! 本 crate 不做任何 I/O（剪贴板、网络、文件、时间），
//! 因此可脱离系统环境完整单元测试。平台相关能力由
//! `clipsync-clip` / `clipsync-net` 提供，在 `clipsync-app` 组装。

pub mod content;
pub mod device;
pub mod engine;
pub mod hash;
pub mod message;

pub use content::{ClipContent, ContentKind, FileMeta, ImageData};
pub use device::{DeviceId, DeviceInfo};
pub use engine::{Limits, LocalDecision, RemoteDecision, SkipReason, SyncEngine};
pub use message::SyncMessage;
