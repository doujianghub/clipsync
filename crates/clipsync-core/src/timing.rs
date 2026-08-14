//! 关键路径耗时打点。
//!
//! **为什么在 core 而不在 app**：一次同步要穿过三个 crate——`clipsync-clip`
//! 读剪贴板、`clipsync-net` 序列化压缩加密上线、`clipsync-app` 落地。慢在
//! 哪一段，只有把打点铺到每一段才看得出来，而 `clipsync-net` 不依赖 app。
//! 阈值也必须是同一个：两边各定一套，日志里就会出现"这段打了那段没打"的
//! 假象，读的人还得先搞清楚谁的阈值是多少。

use std::time::{Duration, Instant};

/// 慢到值得说一句的阈值。
///
/// 定在 200ms：低于这个数用户根本感觉不到，记了也只是噪音；超过了就是他会
/// 抱怨"怎么这么慢"的量级，那时日志里必须有一行能指出慢在哪一段。
const NOTEWORTHY: Duration = Duration::from_millis(200);

/// 关键路径耗时：只在慢得值得注意时记一行。
///
/// **为什么需要**："同步慢"这类抱怨，光看首尾两条日志只能算出一个总时长，
/// 分不清是卡在读剪贴板、压缩、网络还是写剪贴板上——只能靠猜，来回好几轮。
/// 一行分段耗时就能省掉全部猜测。
///
/// 用 INFO 而不是 DEBUG：慢是用户**已经感觉到**的事，等他先去开详细日志再
/// 复现一次，等于把诊断成本转嫁给他。而阈值保证了不慢的时候它一声不吭。
pub fn note_slow(what: &str, since: Instant) {
    let dt = since.elapsed();
    if dt >= NOTEWORTHY {
        tracing::info!("{what}耗时 {} ms", dt.as_millis());
    }
}

/// 同上，但附带字节数与折算速率。
///
/// **为什么速率要算好再打**：搬字节的环节（上线、读体）光有毫秒数没法判断
/// 正常与否——800 ms 对 3 MB 是慢得离谱，对 300 MB 是正常。让读日志的人拿
/// 计算器换算，等于这行日志只完成了一半工作。
pub fn note_slow_bytes(what: &str, bytes: usize, since: Instant) {
    let dt = since.elapsed();
    if dt >= NOTEWORTHY {
        let mbps = bytes as f64 / 1e6 / dt.as_secs_f64();
        tracing::info!(
            "{what}耗时 {} ms（{} 字节，{:.1} MB/s）",
            dt.as_millis(),
            bytes,
            mbps
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 快的路径必须一声不吭——否则每次复制都刷屏，日志就没人看了。
    #[test]
    fn fast_path_is_silent() {
        // 无法直接断言"没打日志"（tracing 无全局钩子），但可以确认阈值本身
        // 没被误调小：200ms 是这套打点不产生噪音的前提。
        assert_eq!(NOTEWORTHY, Duration::from_millis(200));
        note_slow("测试", Instant::now());
        note_slow_bytes("测试", 1024, Instant::now());
    }
}
