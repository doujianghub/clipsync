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
pub fn load_or_init_settings(dir: &Path) -> Result<Settings> {
    let path = dir.join("settings.json");
    if path.exists() {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("读取设置失败: {}", path.display()))?;
        let settings: Settings =
            serde_json::from_str(&text).with_context(|| "解析 settings.json 失败")?;
        Ok(settings)
    } else {
        let settings = Settings::default();
        save_settings(dir, &settings)?;
        Ok(settings)
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
        let id = serde_json::from_str(&text).context("解析 identity.json 失败")?;
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

/// 加载全部配对记录；文件不存在返回空表。
pub fn load_pairings(dir: &Path) -> Result<Vec<clipsync_net::pairing::PairingRecord>> {
    let path = dir.join("pairings.json");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("读取配对记录失败: {}", path.display()))?;
    let list = serde_json::from_str(&text).context("解析 pairings.json 失败")?;
    Ok(list)
}

/// 追加或更新一条配对记录（按设备 ID 去重），并保存。
pub fn upsert_pairing(dir: &Path, record: clipsync_net::pairing::PairingRecord) -> Result<()> {
    let mut list = load_pairings(dir)?;
    if let Some(existing) = list.iter_mut().find(|r| r.device == record.device) {
        *existing = record;
    } else {
        list.push(record);
    }
    let path = dir.join("pairings.json");
    let text = serde_json::to_string_pretty(&list).context("序列化配对记录失败")?;
    std::fs::write(&path, text).with_context(|| format!("写入配对记录失败: {}", path.display()))?;
    Ok(())
}
