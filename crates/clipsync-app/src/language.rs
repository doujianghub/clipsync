//! 决定界面用哪种语言。
//!
//! 优先级：设置里的显式选择 > 系统语言 > 中文。
//!
//! **为什么默认跟随系统而不是固定中文**：装上就该是看得懂的样子。让英文用户
//! 先在一堆中文菜单里找到"语言"那一项再切过去，这个门槛恰恰卡在他第一次
//! 使用的时候。

use clipsync_core::Lang;

/// 设置里的语言偏好。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LangPref {
    /// 跟随系统。
    #[default]
    Auto,
    Fixed(Lang),
}

impl LangPref {
    /// 配置文件里的写法：`auto` / `zh` / `en`。
    pub fn as_str(self) -> &'static str {
        match self {
            LangPref::Auto => "auto",
            LangPref::Fixed(l) => l.as_str(),
        }
    }

    /// 解析配置值。无法识别的值按 `auto` 处理——配置坏了不该让界面变成空白。
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "zh" => LangPref::Fixed(Lang::Zh),
            "en" => LangPref::Fixed(Lang::English),
            _ => LangPref::Auto,
        }
    }

    /// 解析成最终生效的语言。
    pub fn resolve(self) -> Lang {
        match self {
            LangPref::Fixed(l) => l,
            LangPref::Auto => system_language().unwrap_or_default(),
        }
    }
}

/// 按偏好设定全局界面语言，返回最终生效的那个。
pub fn apply(pref: LangPref) -> Lang {
    let lang = pref.resolve();
    clipsync_core::i18n::set(lang);
    lang
}

/// 系统界面语言。取不到返回 `None`，调用方回退到默认（中文）。
#[cfg(target_os = "macos")]
fn system_language() -> Option<Lang> {
    use objc2_foundation::NSLocale;

    // preferredLanguages 是用户在「系统设置 › 语言与地区」里排的顺序，第一个
    // 就是他希望应用优先使用的语言——比 currentLocale 更贴近"界面该用哪种话"，
    // 后者反映的是日期数字格式的区域设定，可能和界面语言不是一回事。
    let langs = NSLocale::preferredLanguages();
    let first = langs.iter().next()?;
    Some(Lang::parse(&first.to_string()))
}

#[cfg(windows)]
fn system_language() -> Option<Lang> {
    use windows_sys::Win32::Globalization::GetUserDefaultLocaleName;

    // LOCALE_NAME_MAX_LENGTH = 85（含结尾 NUL）。
    let mut buf = [0u16; 85];
    // SAFETY: 传入的是自己的缓冲区与它的真实长度。
    let len = unsafe { GetUserDefaultLocaleName(buf.as_mut_ptr(), buf.len() as i32) };
    if len <= 1 {
        return None;
    }
    // 返回值含结尾 NUL，去掉它。
    let name = String::from_utf16_lossy(&buf[..(len as usize - 1)]);
    Some(Lang::parse(&name))
}

#[cfg(not(any(target_os = "macos", windows)))]
fn system_language() -> Option<Lang> {
    // 其余平台按 POSIX 惯例看环境变量。这条路径当前用不到（Linux 上程序本身
    // 不可用），留着是为了不让非目标平台编译失败。
    let raw = std::env::var("LC_ALL")
        .or_else(|_| std::env::var("LC_MESSAGES"))
        .or_else(|_| std::env::var("LANG"))
        .ok()?;
    Some(Lang::parse(raw.split(['.', '@']).next().unwrap_or(&raw)))
}

/// 测试专用：串行化"切换界面语言"的测试。
///
/// 语言是进程级全局状态，而 `cargo test` 默认并行跑。两个测试同时切语言的
/// 表现是**随机失败**，且失败信息指向被干扰的那个，与真正的肇事者无关——
/// 和剪贴板测试锁是同一类问题。
///
/// 锁中毒时取回内部值继续：一个测试 panic 不该把其余的拖成连锁失败。
#[cfg(test)]
pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 测试专用：在指定语言下跑一段闭包，跑完恢复原语言。
#[cfg(test)]
pub(crate) fn with_lang<T>(lang: Lang, f: impl FnOnce() -> T) -> T {
    let _guard = test_lock();
    let before = clipsync_core::i18n::current();
    clipsync_core::i18n::set(lang);
    let out = f();
    clipsync_core::i18n::set(before);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_values_round_trip() {
        for p in [
            LangPref::Auto,
            LangPref::Fixed(Lang::Zh),
            LangPref::Fixed(Lang::English),
        ] {
            assert_eq!(LangPref::parse(p.as_str()), p);
        }
    }

    /// 配置坏了要退回跟随系统，而不是让界面无文案。
    #[test]
    fn unknown_values_fall_back_to_auto() {
        for bad in ["", "  ", "fr", "zh-CN-extra", "true", "0"] {
            assert_eq!(
                LangPref::parse(bad),
                LangPref::Auto,
                "{bad:?} 应回退到 auto"
            );
        }
    }

    /// 显式选择必须压过系统语言——用户点了就是点了。
    #[test]
    fn an_explicit_choice_ignores_the_system() {
        assert_eq!(LangPref::Fixed(Lang::English).resolve(), Lang::English);
        assert_eq!(LangPref::Fixed(Lang::Zh).resolve(), Lang::Zh);
    }

    /// 系统检测无论返回什么都不能 panic，且结果必须是两种语言之一。
    #[test]
    fn system_detection_is_total() {
        let got = LangPref::Auto.resolve();
        assert!(matches!(got, Lang::Zh | Lang::English));
    }
}
