//! 用户手输的大小与速率的解析。
//!
//! 托盘的预设档覆盖不了所有人（有人要 300 MB、有人要 5 GB），既然加了输入框，
//! 就该让用户写得自然些，而不是逼他们数零。

use anyhow::Result;

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
}
