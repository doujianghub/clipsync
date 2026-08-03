//! ClipSync 网络层：设备发现、传输、配对。
//!
//! 模块划分：
//!   - `peer`：已配对设备的候选地址管理与**路径优选**（LAN 优先、Tailscale 兜底）。
//!     纯逻辑，可单测。
//!   - `pairing`：一次性配对码类型与配对记录。
//!   - `transport`：加密传输接口（M2 用 snow/Noise + TCP 实现）。
//!   - `discovery`：设备发现接口（M3 用 mdns-sd + tailscale status 实现）。
//!
//! M0 阶段提供类型与接口定义；重实现（tokio/snow/mdns-sd/spake2）在
//! 对应里程碑填充并核实依赖版本。

pub mod crypto;
pub mod discovery;
pub mod local;
pub mod pairing;
pub mod pairing_handshake;
pub mod peer;
pub mod transport;
pub mod wire;

pub use peer::{AddrClass, AddrSource, Candidate, PeerAddresses};
