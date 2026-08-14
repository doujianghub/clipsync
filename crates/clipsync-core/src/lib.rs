//! ClipSync 核心：平台无关的类型与同步逻辑。
//!
//! 本 crate 不做任何 I/O（剪贴板、网络、文件、时间），
//! 因此可脱离系统环境完整单元测试。平台相关能力由
//! `clipsync-clip` / `clipsync-net` 提供，在 `clipsync-app` 组装。
//!
//! **唯一例外是 [`timing`]**：它读时钟。放这儿是因为耗时打点得横跨
//! `clipsync-net` 与 `clipsync-app` 两侧，而 core 是它们唯一的共同依赖。
//! 它不参与任何同步决策，删掉只会少几行日志，因此不影响"逻辑可脱离系统
//! 环境测试"这个真正的约束。

pub mod content;
pub mod device;
pub mod engine;
pub mod hash;
pub mod i18n;
pub mod message;
pub mod timing;

pub use content::{ClipContent, ContentKind, FileMeta, ImageData};
pub use device::{DeviceId, DeviceInfo};
pub use engine::{Limits, LocalDecision, RemoteDecision, SkipReason, SyncEngine, INLINE_MAX_BYTES};
pub use i18n::Lang;
pub use message::{PeerIntro, SyncMessage, PROTOCOL_VERSION};
