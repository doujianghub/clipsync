//! 一次性配对码的 PAKE 握手协议。
//!
//! 目标：两台设备仅凭一个短配对码，在不安全信道（TCP）上安全地互换各自的
//! Noise 静态公钥，抗中间人。之后即可用 `Noise_IK` 长期免配对互认。
//!
//! 协议（双方对称，用 SPAKE2 symmetric 模式，无需协调 A/B 角色）：
//!   1. 双方各以配对码为口令 `start_symmetric`，得到本地 PAKE 状态与一条
//!      待发送的 PAKE 消息。
//!   2. 交换 PAKE 消息，各自 `finish` 得到**相同的**共享密钥 `K`（仅当双方
//!      口令一致；否则密钥不同，后续认证失败）。
//!   3. 用 `K` 派生一个对称加密通道（这里复用 Noise 的 `NN` + PSK 无法直接
//!      用，故采用简单方案：用 `K` 作为 XChaCha 密钥认证加密 payload）。
//!      为保持依赖精简，直接用 `K` 对"身份负载"做 HMAC 式确认 + 明文交换：
//!      详见下方 `confirm` 设计。
//!
//! 身份负载交换与确认（防 MITM 的关键）：
//!   - 每方发送 `Identity { device_id, name, static_public_key }` 的明文，
//!     并附带 `tag = BLAKE2s(K || direction || identity_bytes)`。
//!   - 对端用同一 `K` 重算 tag 校验。MITM 不知 `K`（未持配对码），无法伪造
//!     tag，也无法在不被发现的情况下替换公钥。
//!
//! 说明：SPAKE2 已保证只有掌握相同配对码者能得到相同 `K`，此处的 tag 把
//! “拥有 K”绑定到“这份身份负载”，从而认证公钥来源。

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use spake2::{Ed25519Group, Identity as SpakeIdentity, Password, Spake2};

use crate::pairing::{PairingCode, PairingRecord};
use crate::wire::{read_frame, write_frame};

/// 双方共用的 SPAKE2 身份串（symmetric 模式需一致）。
const PAIRING_ID: &[u8] = b"clipsync-pairing-v1";

/// 在握手中交换的身份负载。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Identity {
    device_id: String,
    name: String,
    static_public_key: Vec<u8>,
    /// 本机可达地址（物理网卡 / 覆盖网 / 公网）。随身份一同被认证标签保护，
    /// 中间人无法篡改。
    addrs: Vec<std::net::SocketAddr>,
}

/// 带认证标签的身份消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AuthenticatedIdentity {
    identity: Identity,
    /// BLAKE2s(K || identity_bytes) —— 证明发送方掌握共享密钥 K。
    tag: Vec<u8>,
}

/// 本机在配对中提供的信息。
pub struct LocalPairingInfo {
    pub device_id: String,
    pub name: String,
    pub static_public_key: Vec<u8>,
    /// 本机可达地址，供对端首次连接使用。
    pub addrs: Vec<std::net::SocketAddr>,
}

/// 标记：这次失败是**配对码不对**，而不是连接层面的问题。
///
/// 主持方靠它决定要不要计入"猜测次数"。不区分的话，任何人反复连一下再断开
/// 就能耗光失败上限、把会话打死——而自动发现恰恰会去连一批地址，等于自己
/// 把自己的配对会话探挂。
///
/// 用 `anyhow` 的类型化上下文而不是匹配错误文案：文案会改，类型不会。
#[derive(Debug)]
pub struct WrongCode;

impl std::fmt::Display for WrongCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("配对码不匹配")
    }
}

/// 在已建立的字节流上执行配对握手，成功返回对端的 `PairingRecord`。
///
/// `stream` 需实现 `Read + Write`（如 `TcpStream`）。双方须使用相同配对码。
/// 协议对称，两端调用同一函数即可。
pub fn run_pairing<S: std::io::Read + std::io::Write>(
    stream: &mut S,
    code: &PairingCode,
    local: &LocalPairingInfo,
) -> Result<PairingRecord> {
    // 1) SPAKE2 symmetric：以配对码为口令。
    let (state, our_pake_msg) = Spake2::<Ed25519Group>::start_symmetric(
        &Password::new(code.as_str().as_bytes()),
        &SpakeIdentity::new(PAIRING_ID),
    );

    // 2) 交换 PAKE 消息。先写后读（对称场景下双方都先写，避免死锁：写入带
    //    长度前缀且先 flush，双方缓冲区足以容纳一条 PAKE 消息）。
    write_frame(stream, &our_pake_msg).context("发送 PAKE 消息失败")?;
    let their_pake_msg = read_frame(stream)
        .context("读取 PAKE 消息失败")?
        .ok_or_else(|| anyhow!("配对期间对端关闭（PAKE 阶段）"))?;

    // 3) 完成 PAKE，得到共享密钥 K。口令不一致时这里可能出错或得到不同 K。
    let key = state
        .finish(&their_pake_msg)
        .map_err(|e| anyhow!("PAKE 协商失败: {e:?}"))
        .context(WrongCode)?;

    // 4) 交换带认证标签的身份负载。
    let our_identity = Identity {
        device_id: local.device_id.clone(),
        name: local.name.clone(),
        static_public_key: local.static_public_key.clone(),
        addrs: local.addrs.clone(),
    };
    let our_auth = AuthenticatedIdentity {
        tag: identity_tag(&key, &our_identity)?,
        identity: our_identity,
    };
    let our_bytes = postcard::to_allocvec(&our_auth).context("编码身份负载失败")?;
    write_frame(stream, &our_bytes).context("发送身份负载失败")?;

    let their_bytes = read_frame(stream)
        .context("读取身份负载失败")?
        .ok_or_else(|| anyhow!("配对期间对端关闭（身份阶段）"))?;
    let their_auth: AuthenticatedIdentity =
        postcard::from_bytes(&their_bytes).context("解码对端身份负载失败")?;

    // 5) 校验对端标签：证明对端掌握相同 K（即相同配对码），认证公钥来源。
    let expected = identity_tag(&key, &their_auth.identity)?;
    if !constant_time_eq(&expected, &their_auth.tag) {
        return Err(anyhow!("标签不匹配（配对码错误或存在中间人）")).context(WrongCode);
    }

    // 6) 设备 ID **自己算**，不采信对端自报的那个。
    //
    // `DeviceId` 就是静态公钥的指纹（`DeviceId::from_public_key`），是可推导
    // 的——既然能算，就没有理由信别人报的。自报值与公钥不符时，后果全是
    // "连得上但行为诡异"的那类问题，极难排查：
    //   - **局域网发现失效**：信标里的 id 是发送方按公钥算的，与记录里存的
    //     自报值对不上，信标会被当作陌生设备丢弃；
    //   - **连接方向去重错乱**：谁拨号由 id 字典序决定，两边对 id 的认知不
    //     一致时，会双双拨号（连接抖动）或双双不拨（永远连不上）。
    //
    // 公钥本身已由上面的标签校验认证过，据它派生的 id 因此也是可信的。
    let device = clipsync_core::DeviceId::from_public_key(&their_auth.identity.static_public_key);
    if device.as_str() != their_auth.identity.device_id {
        // 正常实现不会走到这里；出现即说明对端版本/实现有问题，值得留痕。
        tracing::warn!(
            "对端自报的设备 ID（{}）与其公钥派生值（{}）不符，以派生值为准",
            their_auth.identity.device_id,
            device
        );
    }

    Ok(PairingRecord {
        device,
        name: their_auth.identity.name,
        static_public_key: their_auth.identity.static_public_key,
        addrs: their_auth.identity.addrs,
        // 走到这里的都是用户亲手完成的配对（输过配对码）。
        introduced_by: None,
    })
}

/// 计算身份负载的认证标签：BLAKE2s(K || identity_bytes)。
///
/// 用 SPAKE2 输出的共享密钥 `K` 作为密钥前缀，绑定“掌握 K”与“这份身份”。
fn identity_tag(key: &[u8], identity: &Identity) -> Result<Vec<u8>> {
    use blake2::digest::{Update, VariableOutput};
    use blake2::Blake2sVar;

    let identity_bytes = postcard::to_allocvec(identity).context("编码身份用于打标签失败")?;
    let mut hasher = Blake2sVar::new(32).expect("32 是合法的 BLAKE2s 输出长度");
    hasher.update(key);
    hasher.update(&(identity_bytes.len() as u64).to_le_bytes());
    hasher.update(&identity_bytes);
    let mut out = vec![0u8; 32];
    hasher
        .finalize_variable(&mut out)
        .expect("输出缓冲长度与设定一致");
    Ok(out)
}

/// 常数时间比较，避免标签校验的时序侧信道。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};

    fn info(seed: u8, name: &str) -> LocalPairingInfo {
        let pk = vec![seed; 32];
        LocalPairingInfo {
            device_id: clipsync_core::DeviceId::from_public_key(&pk).to_string(),
            name: name.to_string(),
            static_public_key: pk,
            addrs: vec![
                format!("192.168.1.{seed}:47684").parse().unwrap(),
                format!("100.64.0.{seed}:47684").parse().unwrap(),
            ],
        }
    }

    /// 用一对已连接的 TcpStream 跑完整配对握手，双方应各自得到对方的记录。
    #[test]
    fn pairing_succeeds_with_matching_code() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let code = PairingCode::from_entropy(b"ABCDEF");
            run_pairing(&mut s, &code, &info(2, "server"))
        });

        let mut c = TcpStream::connect(addr).unwrap();
        let code = PairingCode::from_entropy(b"ABCDEF");
        let client_result = run_pairing(&mut c, &code, &info(1, "client"));

        let client_rec = client_result.expect("client 配对应成功");
        let server_rec = server.join().unwrap().expect("server 配对应成功");

        // 各自拿到对端信息。
        assert_eq!(client_rec.name, "server");
        assert_eq!(client_rec.static_public_key, vec![2u8; 32]);
        assert_eq!(server_rec.name, "client");
        assert_eq!(server_rec.static_public_key, vec![1u8; 32]);

        // 地址也随身份被交换（供首次连接使用）。
        assert!(client_rec
            .addrs
            .contains(&"192.168.1.2:47684".parse().unwrap()));
        assert!(client_rec
            .addrs
            .contains(&"100.64.0.2:47684".parse().unwrap()));
        assert!(server_rec
            .addrs
            .contains(&"192.168.1.1:47684".parse().unwrap()));
    }

    /// 记录里的设备 ID 必须由对端公钥派生，而不是照抄对端自报的值。
    ///
    /// 自报值与公钥不符时会引出一类极难排查的故障：局域网信标里的 id 是
    /// 按公钥算的，与记录对不上就会被当陌生设备丢弃（发现失效）；而拨号
    /// 方向由 id 字典序决定，两边认知不一致会双双拨号或双双不拨。
    #[test]
    fn device_id_comes_from_public_key_not_peer_claim() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        // 服务端谎报一个与自己公钥无关的设备 ID。
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let code = PairingCode::from_entropy(b"ABCDEF");
            let mut liar = info(2, "server");
            liar.device_id = "deadbeefdeadbeef".to_string();
            run_pairing(&mut s, &code, &liar)
        });

        let mut c = TcpStream::connect(addr).unwrap();
        let code = PairingCode::from_entropy(b"ABCDEF");
        let rec = run_pairing(&mut c, &code, &info(1, "client")).expect("配对本身应成功");
        let _ = server.join().unwrap();

        assert_eq!(
            rec.device,
            clipsync_core::DeviceId::from_public_key(&vec![2u8; 32]),
            "应采用公钥派生值"
        );
        assert_ne!(
            rec.device.as_str(),
            "deadbeefdeadbeef",
            "不得采信对端自报的设备 ID"
        );
    }

    /// 配对码不一致时，认证必须失败（至少一方报错）。
    #[test]
    fn pairing_fails_with_mismatched_code() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let code = PairingCode::from_entropy(b"AAAAAA");
            run_pairing(&mut s, &code, &info(2, "server"))
        });

        let mut c = TcpStream::connect(addr).unwrap();
        let code = PairingCode::from_entropy(b"BBBBBB");
        let client_result = run_pairing(&mut c, &code, &info(1, "client"));
        let server_result = server.join().unwrap();

        assert!(
            client_result.is_err() || server_result.is_err(),
            "配对码不一致时不应双双成功"
        );
    }
}
