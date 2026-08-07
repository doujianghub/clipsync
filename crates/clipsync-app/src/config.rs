//! 配置与本地状态的持久化。
//!
//! 配置目录（`directories` 定位）：
//!   - Windows：`%APPDATA%\ClipSync\`
//!   - macOS：`~/Library/Application Support/ClipSync/`
//!
//! 存放：本机 Noise 静态密钥、设备名、已配对记录、用户可调设置（大小上限、
//! 类型开关等）。密钥文件权限在 M2 收紧（macOS 0600）。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

/// 应用标识，用于定位配置目录。
const QUALIFIER: &str = "";
const ORGANIZATION: &str = "ClipSync";
const APPLICATION: &str = "ClipSync";

/// 用户可调设置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// 单次内容最大字节数。默认 100 MiB。
    pub max_bytes: usize,
    /// 是否同步图片。
    pub allow_image: bool,
    /// 是否同步文件。
    pub allow_files: bool,
    /// 监听端口（TCP，供对端连接）。0 表示随机分配。
    pub listen_port: u16,
    /// 文件内容缓存上限（字节）。
    ///
    /// 缓存用于断点续传与重复内容秒同步：传输被新剪贴板内容打断时，已收到的
    /// 字节会保留，下次同步同一文件即可续传。超出上限时按最久未使用淘汰，
    /// 当前剪贴板引用的内容不会被淘汰。
    #[serde(default = "default_file_cache_bytes")]
    pub file_cache_bytes: u64,

    /// 文件传输的发送速率上限（字节/秒）。`0` 表示不限速。
    ///
    /// 仅限制文件内容——文本/图片同步不受影响，因此限速状态下复制文字
    /// 依然瞬时同步。传大文件会占满链路时可设个值（如 20MB/s = 20971520）。
    #[serde(default = "default_upload_limit")]
    pub upload_limit_bytes_per_sec: u64,

    /// 是否在传输文件前自适应压缩。
    ///
    /// 会先取文件开头试压，压不动（如 jpg/mp4/zip）则自动跳过，不浪费 CPU。
    /// 文本/代码/日志类文件通常能显著提速并省带宽。
    #[serde(default = "default_compress")]
    pub compress_transfers: bool,

    /// 是否记录详细日志（debug 级）。
    ///
    /// 平时关着：传大文件时每个分块都会记一行，长期开着既费磁盘也吵。
    /// 出问题时从托盘勾上，无需重启即可生效，复现一次再关掉。
    #[serde(default)]
    pub verbose_log: bool,
}

fn default_file_cache_bytes() -> u64 {
    1024 * 1024 * 1024 // 1 GiB
}

fn default_upload_limit() -> u64 {
    0 // 0 = 不限速
}

fn default_compress() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            max_bytes: 100 * 1024 * 1024,
            allow_image: true,
            allow_files: true,
            listen_port: 47_684, // 固定默认端口，便于 mDNS 之外的直连调试
            file_cache_bytes: default_file_cache_bytes(),
            upload_limit_bytes_per_sec: default_upload_limit(),
            compress_transfers: default_compress(),
            verbose_log: false,
        }
    }
}

impl Settings {
    /// 映射为核心引擎的 `Limits`。
    pub fn to_limits(&self) -> clipsync_core::Limits {
        clipsync_core::Limits {
            max_bytes: self.max_bytes,
            allow_image: self.allow_image,
            allow_files: self.allow_files,
        }
    }
}

/// 定位并（按需）创建配置目录。
///
/// 若设置了环境变量 `CLIPSYNC_CONFIG_DIR`，则用其指定的目录（便于在一台机器上
/// 运行多个隔离实例做测试，也方便高级用户自定义位置）；否则用系统默认位置
/// （Windows `%APPDATA%\ClipSync\`、macOS `~/Library/Application Support/ClipSync/`）。
pub fn config_dir() -> Result<PathBuf> {
    let dir = if let Some(custom) = std::env::var_os("CLIPSYNC_CONFIG_DIR") {
        PathBuf::from(custom)
    } else {
        let dirs = ProjectDirs::from(QUALIFIER, ORGANIZATION, APPLICATION)
            .context("无法定位系统配置目录")?;
        dirs.config_dir().to_path_buf()
    };
    std::fs::create_dir_all(&dir).with_context(|| format!("无法创建配置目录: {}", dir.display()))?;
    Ok(dir)
}

/// 从配置目录加载设置；不存在则返回默认值并写入。
/// 从配置目录加载设置；不存在或**已损坏**时回退到默认值。
///
/// **为什么损坏不算致命**：这是个托盘常驻程序，而我们又在文档和菜单里反复
/// 引导用户"需要精确值可以直接改 settings.json"——手改出语法错误的概率并不低。
/// 原来这里直接把错误往上抛，`main` 返回 `Err` 后进程退出，用户的体验是
/// **双击图标毫无反应**，且没有任何地方告诉他为什么。
///
/// 现在改为：把坏文件另存为 `settings.json.bad`（不能直接覆盖——用户手写的
/// 内容里可能有他还想找回的值），用默认值继续跑，并在日志里写清楚。
pub fn load_or_init_settings(dir: &Path) -> Result<Settings> {
    let path = dir.join("settings.json");
    if !path.exists() {
        let settings = Settings::default();
        save_settings(dir, &settings)?;
        return Ok(settings);
    }

    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("读取设置失败: {}", path.display()))?;
    match serde_json::from_str(&text) {
        Ok(settings) => Ok(settings),
        Err(e) => {
            let backup = quarantine(&path);
            tracing::warn!(
                "settings.json 无法解析（{e}），已改用默认设置继续运行{}",
                backup
            );
            let settings = Settings::default();
            let _ = save_settings(dir, &settings);
            Ok(settings)
        }
    }
}

/// 把坏掉的配置文件挪到 `.bad` 旁路，返回一句可直接拼进日志的说明。
///
/// 保留原内容而不是直接覆盖：用户手写的值可能还想找回来。
fn quarantine(path: &Path) -> String {
    let bad = path.with_extension(
        path.extension()
            .map(|e| format!("{}.bad", e.to_string_lossy()))
            .unwrap_or_else(|| "bad".to_string()),
    );
    let _ = std::fs::remove_file(&bad);
    match std::fs::rename(path, &bad) {
        Ok(()) => format!("；原文件已保留为 {}", bad.display()),
        Err(_) => String::new(),
    }
}

/// 保存设置到配置目录。
pub fn save_settings(dir: &Path, settings: &Settings) -> Result<()> {
    let path = dir.join("settings.json");
    let text = serde_json::to_string_pretty(settings).context("序列化设置失败")?;
    std::fs::write(&path, text).with_context(|| format!("写入设置失败: {}", path.display()))?;
    Ok(())
}

// ————————————————————————————————————————————————————————————————
// 用户输入的数值解析
// ————————————————————————————————————————————————————————————————

/// 解析用户手输的字节数，支持带单位的常见写法。
///
/// 托盘的预设档覆盖不了所有人（有人要 300 MB、有人要 5 GB），加了输入框就
/// 该让用户写得自然些：`500MB`、`1.5 GiB`、`200m`、`104857600` 都认。
///
/// 单位规则遵循惯例：`KB/MB/GB` 按 1000 进制，`KiB/MiB/GiB` 按 1024 进制；
/// 只写 `k/m/g` 时按 1024 进制处理——手输单字母的人通常想的是"多少兆"，
/// 而在这个语境（大小上限）下按 1024 算更贴近他们在别处看到的数字。
/// 不带单位则视为字节。
pub fn parse_byte_size(input: &str) -> Result<u64> {
    let s = input.trim().replace(['_', ' '], "");
    if s.is_empty() {
        anyhow::bail!("请输入一个大小，例如 500MB 或 2GiB");
    }

    let digits_end = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(digits_end);
    let value: f64 = num
        .parse()
        .map_err(|_| anyhow::anyhow!("看不懂的数字「{num}」，请输入如 500MB 或 2GiB"))?;
    if !value.is_finite() || value < 0.0 {
        anyhow::bail!("大小必须是正数");
    }

    let multiplier: u64 = match unit.to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kib" => 1 << 10,
        "kb" => 1_000,
        "m" | "mib" => 1 << 20,
        "mb" => 1_000_000,
        "g" | "gib" => 1 << 30,
        "gb" => 1_000_000_000,
        "t" | "tib" => 1u64 << 40,
        "tb" => 1_000_000_000_000,
        other => anyhow::bail!("看不懂的单位「{other}」，可用 B/KB/MB/GB 或 KiB/MiB/GiB"),
    };

    let bytes = value * multiplier as f64;
    // 上界卡在 u64 可表示范围内；超出多半是手滑多打了几位。
    if bytes >= u64::MAX as f64 {
        anyhow::bail!("这个大小太大了，请输入更小的值");
    }
    Ok(bytes as u64)
}

/// 解析用户手输的速率（字节/秒），支持 `10MB/s`、`20mbps`、`5M` 等写法。
///
/// `0` / `不限速` / `unlimited` 一律解释为不限速。
pub fn parse_rate(input: &str) -> Result<u64> {
    let s = input.trim();
    let lowered = s.to_ascii_lowercase();
    if matches!(lowered.as_str(), "0" | "不限速" | "无限制" | "unlimited" | "none") {
        return Ok(0);
    }
    // 去掉速率后缀再按大小解析——`/s`、`ps`、`每秒` 都只是修饰，不影响数值。
    let trimmed = lowered
        .trim_end_matches("每秒")
        .trim_end_matches("/s")
        .trim_end_matches("ps")
        .trim_end_matches("/秒");
    parse_byte_size(trimmed)
}

// ————————————————————————————————————————————————————————————————
// 运行期可变设置
// ————————————————————————————————————————————————————————————————

/// 托盘与中枢之间共享的设置句柄。
///
/// 用户在托盘菜单改设置后需要**立即生效**，不该要求重启。做法与已有的
/// `TrayStatus`（暂停开关）一致：托盘写、中枢读。
///
/// `version` 是变更计数：中枢每轮比对一次整数即可知道要不要重读设置，
/// 无需每轮都去拿锁拷贝整个 `Settings`。
#[derive(Clone)]
pub struct SettingsHandle {
    inner: std::sync::Arc<std::sync::Mutex<Settings>>,
    dir: std::sync::Arc<PathBuf>,
    version: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl SettingsHandle {
    pub fn new(dir: PathBuf, settings: Settings) -> Self {
        Self {
            inner: std::sync::Arc::new(std::sync::Mutex::new(settings)),
            dir: std::sync::Arc::new(dir),
            version: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    pub fn snapshot(&self) -> Settings {
        self.inner.lock().unwrap().clone()
    }

    /// 变更计数。值改变即表示设置被动过。
    pub fn version(&self) -> u64 {
        self.version.load(std::sync::atomic::Ordering::Acquire)
    }

    /// 修改设置并立即落盘。
    ///
    /// 落盘失败只记 warn、**不回滚内存值**：用户在菜单上的操作应当立刻见效，
    /// 若因磁盘只读之类的原因存不下，也好过界面点了没反应；代价是重启后
    /// 恢复旧值，日志里有据可查。
    pub fn update(&self, f: impl FnOnce(&mut Settings)) {
        let snapshot = {
            let mut g = self.inner.lock().unwrap();
            f(&mut g);
            g.clone()
        };
        // 先递增版本再落盘：即便落盘卡住，中枢也已能读到新值。
        self.version
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        if let Err(e) = save_settings(&self.dir, &snapshot) {
            tracing::warn!("设置已生效但保存失败（重启后会恢复旧值）: {e:#}");
        }
    }
}

// ————————————————————————————————————————————————————————————————
// 本机加密身份（Noise 静态密钥）持久化
// ————————————————————————————————————————————————————————————————

/// 加载本机静态身份；不存在则生成并保存。
///
/// `identity.json` 含 Noise 静态私钥（设备长期身份），在 Unix 上以 0600 创建，
/// 仅本用户可读写。
pub fn load_or_init_identity(dir: &Path) -> Result<clipsync_net::crypto::StaticIdentity> {
    let path = dir.join("identity.json");
    if path.exists() {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("读取身份失败: {}", path.display()))?;
        // 这里**故意不做**设置/配对记录那样的"损坏就用默认值"降级：
        // identity.json 里是本机的长期身份私钥，换一个新的等于换了台设备——
        // 所有对端都会因公钥对不上而拒绝连接，而用户只会看到"忽然全都连不上了"，
        // 完全猜不到原因。宁可明确报错、让他知道发生了什么。
        let id = serde_json::from_str(&text).with_context(|| {
            format!(
                "解析 {} 失败。该文件保存着本机的加密身份，损坏后无法自动恢复——\
                 若无备份，需要删除它并与所有设备重新配对",
                path.display()
            )
        })?;
        // 旧版本可能以默认权限创建过该文件，这里顺带收紧一次。
        #[cfg(unix)]
        restrict_to_owner(&path)?;
        Ok(id)
    } else {
        let id = clipsync_net::crypto::StaticIdentity::generate()?;
        let text = serde_json::to_string_pretty(&id).context("序列化身份失败")?;
        write_private(&path, &text)
            .with_context(|| format!("写入身份失败: {}", path.display()))?;
        Ok(id)
    }
}

/// 写入仅属主可读写的文件。
///
/// Unix 上在**创建时**就带 0600 打开，而不是先按默认权限写好再 chmod——
/// 后者会留下一个私钥短暂可被其它用户读取的窗口。
#[cfg(unix)]
fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(contents.as_bytes())
}

#[cfg(not(unix))]
fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    // Windows 下 ACL 默认继承用户目录权限，其它用户无法读取。
    std::fs::write(path, contents)
}

/// 把既有文件的权限收紧为 0600（仅属主可读写）。
#[cfg(unix)]
fn restrict_to_owner(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut perms = std::fs::metadata(path)
        .with_context(|| format!("读取文件权限失败: {}", path.display()))?
        .permissions();
    if perms.mode() & 0o777 != 0o600 {
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms)
            .with_context(|| format!("收紧文件权限失败: {}", path.display()))?;
    }
    Ok(())
}

// ————————————————————————————————————————————————————————————————
// 已配对设备记录持久化
// ————————————————————————————————————————————————————————————————

/// 加载全部配对记录；文件不存在或已损坏时返回空表。
///
/// 损坏时同样不阻止启动（理由见 [`load_or_init_settings`]）。代价是用户需要
/// 重新配对——但那至少是个能自己动手解决的处境，比程序打不开、也不说为什么
/// 要好。坏文件同样保留为 `.bad`，万一还能手工救回来。
pub fn load_pairings(dir: &Path) -> Result<Vec<clipsync_net::pairing::PairingRecord>> {
    let path = dir.join("pairings.json");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("读取配对记录失败: {}", path.display()))?;
    match serde_json::from_str(&text) {
        Ok(list) => Ok(list),
        Err(e) => {
            let backup = quarantine(&path);
            tracing::warn!(
                "pairings.json 无法解析（{e}），已按「尚无配对设备」继续运行{}；\
                 需要重新配对",
                backup
            );
            Ok(Vec::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_bytes_and_binary_units() {
        assert_eq!(parse_byte_size("1024").unwrap(), 1024);
        assert_eq!(parse_byte_size("1KiB").unwrap(), 1024);
        assert_eq!(parse_byte_size("1MiB").unwrap(), 1024 * 1024);
        assert_eq!(parse_byte_size("2GiB").unwrap(), 2 * 1024 * 1024 * 1024);
        // 单字母按 1024 进制——手输 "500m" 的人想的是 MiB。
        assert_eq!(parse_byte_size("500m").unwrap(), 500 * 1024 * 1024);
    }

    #[test]
    fn parses_decimal_units_by_thousand() {
        assert_eq!(parse_byte_size("1KB").unwrap(), 1_000);
        assert_eq!(parse_byte_size("500MB").unwrap(), 500_000_000);
        assert_eq!(parse_byte_size("1GB").unwrap(), 1_000_000_000);
    }

    /// 用户不会按规范写：大小写混杂、带空格、带小数点都得认。
    #[test]
    fn tolerates_messy_user_input() {
        assert_eq!(parse_byte_size(" 500 mb ").unwrap(), 500_000_000);
        assert_eq!(parse_byte_size("1.5GiB").unwrap(), 1_610_612_736);
        assert_eq!(parse_byte_size("100MiB").unwrap(), parse_byte_size("100mib").unwrap());
    }

    #[test]
    fn rejects_nonsense_with_actionable_message() {
        for bad in ["", "abc", "12XY", "-5MB"] {
            let err = parse_byte_size(bad).unwrap_err().to_string();
            assert!(
                err.contains("请输入") || err.contains("可用") || err.contains("正数"),
                "错误信息应告诉用户怎么写，实际: {err}"
            );
        }
    }

    #[test]
    fn rate_accepts_speed_suffixes_and_unlimited() {
        assert_eq!(parse_rate("10MB/s").unwrap(), 10_000_000);
        assert_eq!(parse_rate("20mbps").unwrap(), 20_000_000);
        assert_eq!(parse_rate("5M").unwrap(), 5 * 1024 * 1024);
        // 不限速的几种说法。
        for none in ["0", "不限速", "unlimited"] {
            assert_eq!(parse_rate(none).unwrap(), 0, "「{none}」应表示不限速");
        }
    }

    /// 损坏的 settings.json 不得阻止程序启动。
    ///
    /// 这是个托盘常驻程序，而文档与菜单都在引导用户"要精确值就手改
    /// settings.json"——改出语法错误是很现实的。原实现把错误抛给 `main`，
    /// 进程直接退出，用户看到的是"双击图标毫无反应"，且无处可查原因。
    #[test]
    fn corrupt_settings_falls_back_instead_of_failing() {
        let dir = std::env::temp_dir().join("clipsync_bad_settings");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("settings.json"), "{ 这不是合法 JSON ,,, ").unwrap();

        let s = load_or_init_settings(&dir).expect("配置损坏不该让启动失败");
        assert_eq!(s.max_bytes, Settings::default().max_bytes, "应回退到默认值");

        // 坏文件要保留下来，用户手写的内容可能还想找回。
        assert!(
            dir.join("settings.json.bad").exists(),
            "原文件应被保留为 .bad，而不是直接覆盖丢弃"
        );
        // 同时应写出一份可用的新配置，下次启动不再走这条路。
        assert!(dir.join("settings.json").exists());
        assert!(load_or_init_settings(&dir).is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 损坏的 pairings.json 同样不阻止启动，退化为"尚无配对设备"。
    #[test]
    fn corrupt_pairings_falls_back_to_empty() {
        let dir = std::env::temp_dir().join("clipsync_bad_pairings");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("pairings.json"), "[[[ 坏掉了").unwrap();

        let list = load_pairings(&dir).expect("配对记录损坏不该让启动失败");
        assert!(list.is_empty());
        assert!(dir.join("pairings.json.bad").exists(), "坏文件应保留待人工挽救");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 与上面相反：identity.json 损坏**必须**报错。
    ///
    /// 那里面是本机长期身份私钥，悄悄换一个新的等于换了台设备——所有对端都会
    /// 因公钥对不上而拒绝连接，用户只会看到"忽然全都连不上了"，完全猜不到原因。
    /// 这种时候明确失败比自作主张地"恢复"要负责得多。
    #[test]
    fn corrupt_identity_fails_loudly() {
        let dir = std::env::temp_dir().join("clipsync_bad_identity");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("identity.json"), "not json at all").unwrap();

        let err = load_or_init_identity(&dir).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("重新配对") || msg.contains("加密身份"),
            "错误信息应说清后果与可行动作，实际: {msg}"
        );
        // 不得偷偷换一个新身份。
        assert!(
            !dir.join("identity.json.bad").exists(),
            "身份文件不该被自动旁路——那会静默失去所有配对"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 解除配对要真的从磁盘上去掉，否则重启后它又回来了。
    #[test]
    fn remove_pairing_persists() {
        use clipsync_net::pairing::PairingRecord;

        let dir = std::env::temp_dir().join("clipsync_unpair_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mk = |seed: u8, name: &str| PairingRecord {
            device: clipsync_core::DeviceId::from_public_key(&[seed; 32]),
            name: name.to_string(),
            static_public_key: vec![seed; 32],
            addrs: vec![],
        };
        let a = mk(1, "笔记本");
        let b = mk(2, "台式机");
        upsert_pairing(&dir, a.clone()).unwrap();
        upsert_pairing(&dir, b.clone()).unwrap();
        assert_eq!(load_pairings(&dir).unwrap().len(), 2);

        let removed = remove_pairing(&dir, &b.device).unwrap();
        assert_eq!(removed.as_deref(), Some("台式机"), "应返回被删设备的名字");

        // 重新从磁盘读——这才证明是真删了而不是只改了内存。
        let left = load_pairings(&dir).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].device, a.device);

        // 删不存在的设备是无操作，不该报错。
        assert!(remove_pairing(&dir, &b.device).unwrap().is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 设置往返：写入的自定义值应能原样读回。
    #[test]
    fn settings_roundtrip_custom_values() {
        let dir = std::env::temp_dir().join("clipsync_settings_roundtrip");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut s = Settings::default();
        s.max_bytes = parse_byte_size("777MB").unwrap() as usize;
        s.upload_limit_bytes_per_sec = parse_rate("33MB/s").unwrap();
        save_settings(&dir, &s).unwrap();

        let back = load_or_init_settings(&dir).unwrap();
        assert_eq!(back.max_bytes, 777_000_000);
        assert_eq!(back.upload_limit_bytes_per_sec, 33_000_000);
    }
}

/// 追加或更新一条配对记录（按设备 ID 去重），并保存。
pub fn upsert_pairing(dir: &Path, record: clipsync_net::pairing::PairingRecord) -> Result<()> {
    let mut list = load_pairings(dir)?;
    if let Some(existing) = list.iter_mut().find(|r| r.device == record.device) {
        *existing = record;
    } else {
        list.push(record);
    }
    save_pairings(dir, &list)
}

/// 删除一条配对记录。返回被删设备的名字；设备不存在时返回 `None`。
///
/// 配对此前是**只能加不能减**的：配错了、设备换了、电脑送人了，都只能去手工
/// 编辑 `pairings.json`——而它在 macOS 上位于 `~/Library/Application Support/`，
/// Finder 里默认隐藏，普通用户根本找不到。
pub fn remove_pairing(dir: &Path, device: &clipsync_core::DeviceId) -> Result<Option<String>> {
    let mut list = load_pairings(dir)?;
    let Some(pos) = list.iter().position(|r| &r.device == device) else {
        return Ok(None);
    };
    let removed = list.remove(pos);
    save_pairings(dir, &list)?;
    Ok(Some(removed.name))
}

fn save_pairings(dir: &Path, list: &[clipsync_net::pairing::PairingRecord]) -> Result<()> {
    let path = dir.join("pairings.json");
    let text = serde_json::to_string_pretty(list).context("序列化配对记录失败")?;
    std::fs::write(&path, text).with_context(|| format!("写入配对记录失败: {}", path.display()))?;
    Ok(())
}
