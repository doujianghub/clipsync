//! 日志：同时写到终端与文件，级别可在运行期切换。
//!
//! **为什么必须写文件**：这个程序日常是从托盘/开机自启起来的，那时**没有终端**
//! ——`println!`/`tracing` 打到 stdout 的内容全部进了虚空。用户遇到"某次没同步
//! 过去""连不上对方"时，没有任何可查的东西，只能靠复现时手动开终端重跑，
//! 而偏偏这类问题往往不好复现。
//!
//! **为什么级别要能运行期切**：详细日志（debug）在传大文件时每块都会记一行，
//! 长期开着既费磁盘也吵。合理的用法是"平时 info，出问题时打开 debug 重现一次"
//! ——要求为此重启程序会打断用户正在排查的现场。用 `tracing_subscriber` 的
//! `reload` 层做到勾一下就生效。
//!
//! **滚动策略**：单文件超过 [`MAX_LOG_BYTES`] 就轮转为 `.1`，只保留一个旧文件。
//! 上限之内足够覆盖一次排查所需的历史，又不会让日志无声无息吃掉磁盘。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{reload, EnvFilter};

/// 单个日志文件的大小上限，超过即轮转。
const MAX_LOG_BYTES: u64 = 4 * 1024 * 1024;

/// 日志文件名（当前）与轮转后的旧文件名。
const LOG_NAME: &str = "clipsync.log";
const LOG_NAME_OLD: &str = "clipsync.log.1";

/// 运行期调整日志级别的句柄。
#[derive(Clone)]
pub struct LogControl {
    handle: reload::Handle<EnvFilter, tracing_subscriber::Registry>,
    dir: PathBuf,
}

impl LogControl {
    /// 日志文件所在目录（供"打开日志文件夹"用）。
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 切换详细程度。`verbose` 为真时记录 debug 级。
    pub fn set_verbose(&self, verbose: bool) -> Result<()> {
        self.handle
            .reload(build_filter(verbose))
            .context("切换日志级别失败")?;
        tracing::info!("日志级别已切换为 {}", if verbose { "详细(debug)" } else { "常规(info)" });
        Ok(())
    }
}

/// 构造过滤器。
///
/// `CLIPSYNC_LOG` 仍然优先——它是开发与排障时的既有习惯，不该被界面开关夺走；
/// 没设它时才按界面上的详细开关决定。
fn build_filter(verbose: bool) -> EnvFilter {
    if let Ok(f) = EnvFilter::try_from_env("CLIPSYNC_LOG") {
        return f;
    }
    let own = if verbose { "debug" } else { "info" };

    // 依赖库压到 warn：详细模式下要看的是"同步/连接发生了什么"，不是某个
    // 底层库的内部状态——那些会把真正有用的行淹掉。
    let mut filter = EnvFilter::default().add_directive(LevelFilter::WARN.into());
    // 本程序的四个 crate 都要放开。bin 的 target 是 `clipsync`（跟 [[bin]]
    // 的 name 走），库 crate 则是带下划线的形式。
    for krate in ["clipsync", "clipsync_core", "clipsync_clip", "clipsync_net"] {
        filter = filter.add_directive(
            format!("{krate}={own}")
                .parse()
                .expect("由固定字面量拼出，必然合法"),
        );
    }
    filter
}

/// 初始化日志：终端 + 文件双路输出。
///
/// 文件写不了时**不让程序失败**——日志是辅助设施，不该因为磁盘满或目录只读
/// 就把同步功能一起搭进去；此时退化为只输出到终端并记一条警告。
pub fn init(dir: &Path, verbose: bool) -> LogControl {
    let log_dir = dir.join("logs");
    let (filter, handle) = reload::Layer::new(build_filter(verbose));

    // 终端层：从托盘启动时没有终端，写了也没人看，但开发/命令行下有用。
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_writer(std::io::stderr);

    match RollingFile::new(&log_dir) {
        Ok(file) => {
            let file_layer = tracing_subscriber::fmt::layer()
                .with_target(false)
                // 文件里不要 ANSI 转义码——否则用记事本打开满屏乱码。
                .with_ansi(false)
                .with_writer(file);
            tracing_subscriber::registry()
                .with(filter)
                .with(stderr_layer)
                .with(file_layer)
                .init();
            tracing::debug!("日志文件: {}", log_dir.join(LOG_NAME).display());
        }
        Err(e) => {
            tracing_subscriber::registry()
                .with(filter)
                .with(stderr_layer)
                .init();
            tracing::warn!("无法写入日志文件（仅输出到终端）: {e:#}");
        }
    }

    LogControl {
        handle,
        dir: log_dir,
    }
}

/// 会自动轮转的日志文件。
///
/// 每次写入前检查大小，超限就把当前文件挪成 `.1` 再新建——只保留一代旧文件。
#[derive(Clone)]
struct RollingFile {
    inner: Arc<Mutex<RollingInner>>,
}

struct RollingInner {
    dir: PathBuf,
    file: std::fs::File,
    written: u64,
}

impl RollingFile {
    fn new(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("创建日志目录失败: {}", dir.display()))?;
        let path = dir.join(LOG_NAME);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("打开日志文件失败: {}", path.display()))?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            inner: Arc::new(Mutex::new(RollingInner {
                dir: dir.to_path_buf(),
                file,
                written,
            })),
        })
    }
}

impl RollingInner {
    fn roll_if_needed(&mut self) {
        if self.written < MAX_LOG_BYTES {
            return;
        }
        let cur = self.dir.join(LOG_NAME);
        let old = self.dir.join(LOG_NAME_OLD);
        let _ = std::fs::remove_file(&old);
        if std::fs::rename(&cur, &old).is_err() {
            // 轮转失败（文件被占用等）就继续往原文件写：日志超一点大小，
            // 远好过从此一条都记不下来。
            return;
        }
        if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(&cur) {
            self.file = f;
            self.written = 0;
        }
    }
}

impl Write for RollingFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.roll_if_needed();
        let n = g.file.write(buf)?;
        g.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.file.flush()
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for RollingFile {
    type Writer = RollingFile;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// 在系统文件管理器中打开某个目录。
///
/// 日志目录在两个平台上都藏得很深（macOS 的 `~/Library/Application Support/`
/// 在 Finder 里默认隐藏），让用户自己找等于不给。
pub fn open_in_file_manager(dir: &Path) -> Result<()> {
    // 目录可能还没建（从未写过日志），先确保存在，否则文件管理器会报错。
    let _ = std::fs::create_dir_all(dir);

    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(windows)]
    let program = "explorer";
    #[cfg(not(any(target_os = "macos", windows)))]
    let program = "xdg-open";

    let mut cmd = std::process::Command::new(program);
    #[cfg(windows)]
    crate::win_util::hidden(&mut cmd);
    cmd.arg(dir)
        .spawn()
        .with_context(|| format!("无法打开目录 {}", dir.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rolls_over_when_exceeding_limit() {
        let dir = std::env::temp_dir().join("clipsync_log_roll_test");
        let _ = std::fs::remove_dir_all(&dir);
        let mut f = RollingFile::new(&dir).unwrap();

        // 写超过上限的数据，中途应发生一次轮转。
        let chunk = vec![b'x'; 64 * 1024];
        let mut total = 0u64;
        while total <= MAX_LOG_BYTES + 128 * 1024 {
            f.write_all(&chunk).unwrap();
            total += chunk.len() as u64;
        }
        f.flush().unwrap();

        let cur = dir.join(LOG_NAME);
        let old = dir.join(LOG_NAME_OLD);
        assert!(old.exists(), "超过上限后应产生轮转文件");
        let cur_len = std::fs::metadata(&cur).unwrap().len();
        assert!(
            cur_len < MAX_LOG_BYTES,
            "轮转后当前文件应重新变小，实际 {cur_len}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 只保留一代旧文件——否则日志会无声地把磁盘吃光。
    #[test]
    fn keeps_only_one_old_generation() {
        let dir = std::env::temp_dir().join("clipsync_log_roll_test2");
        let _ = std::fs::remove_dir_all(&dir);
        let mut f = RollingFile::new(&dir).unwrap();

        let chunk = vec![b'y'; 256 * 1024];
        // 写足够多，触发至少两次轮转。
        for _ in 0..((MAX_LOG_BYTES / chunk.len() as u64 + 2) * 2) {
            f.write_all(&chunk).unwrap();
        }
        f.flush().unwrap();

        let count = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(count, 2, "应只有当前 + 一代旧文件，实际 {count} 个");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 非详细模式下不得记录 debug——否则"详细日志"开关形同虚设，
    /// 而且传大文件时每个分块一行会迅速把日志刷满。
    #[test]
    fn normal_mode_excludes_debug_from_own_crates() {
        // EnvFilter 的字符串表示直接反映生效的指令，够用且不碰内部 API。
        let repr = format!("{}", build_filter(false));
        assert!(
            repr.contains("clipsync=info"),
            "常规模式下本程序应为 info，实际指令: {repr}"
        );
        assert!(
            !repr.contains("clipsync=debug"),
            "常规模式不该放开 debug，实际指令: {repr}"
        );

        let verbose = format!("{}", build_filter(true));
        assert!(
            verbose.contains("clipsync=debug"),
            "详细模式应放开 debug，实际指令: {verbose}"
        );
    }

    /// 依赖库始终压到 warn：详细模式是为了看清自己的同步/连接过程，
    /// 不是把底层库的噪声也翻出来淹掉重点。
    #[test]
    fn dependencies_stay_quiet_even_in_verbose_mode() {
        let repr = format!("{}", build_filter(true));
        assert!(
            repr.contains("warn"),
            "应有一条兜底的 warn 指令约束依赖库，实际: {repr}"
        );
    }

    /// 追加模式：重启后不该把上次的日志冲掉。
    #[test]
    fn appends_across_reopen() {
        let dir = std::env::temp_dir().join("clipsync_log_append_test");
        let _ = std::fs::remove_dir_all(&dir);

        let mut a = RollingFile::new(&dir).unwrap();
        a.write_all(b"first-run\n").unwrap();
        a.flush().unwrap();
        drop(a);

        let mut b = RollingFile::new(&dir).unwrap();
        b.write_all(b"second-run\n").unwrap();
        b.flush().unwrap();

        let content = std::fs::read_to_string(dir.join(LOG_NAME)).unwrap();
        assert!(content.contains("first-run"), "重启不该丢掉上次的日志");
        assert!(content.contains("second-run"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
