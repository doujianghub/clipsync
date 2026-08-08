//! 界面语言。
//!
//! **为什么是"就地双语"而不是资源文件 + key**：那套做法要维护一张 key 表，
//! 翻译和用它的代码离得很远，加一句提示要改三个地方，漏翻时编译器也不会吭声，
//! 最后总会剩下几条永远没人发现的中文。写成 `t!("中文", "English")` 之后，
//! 两种语言就在同一行上，改一处必然看到另一处，漏不掉——代价是只能支持两种
//! 语言，而这正是本项目的全部需求。
//!
//! **为什么放在 core**：用户可见的文案不只在托盘里，`clipsync-clip` 报的
//! "系统拒绝读取…该去哪儿开权限"同样要给人看。放在最底层的 crate 才能让
//! 上面几层都用上。
//!
//! core 不做任何 I/O，所以这里**不检测**系统语言——由 `clipsync-app` 在启动时
//! 查好了调 [`set`] 灌进来。

use std::sync::atomic::{AtomicU8, Ordering};

/// 界面语言。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lang {
    /// 简体中文（默认）。
    #[default]
    Zh,
    English,
}

impl Lang {
    fn as_u8(self) -> u8 {
        match self {
            Lang::Zh => 0,
            Lang::English => 1,
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => Lang::English,
            _ => Lang::Zh,
        }
    }

    /// 配置文件里的写法。
    pub fn as_str(self) -> &'static str {
        match self {
            Lang::Zh => "zh",
            Lang::English => "en",
        }
    }

    /// 解析配置或系统 locale 给出的语言标签。
    ///
    /// 只认前缀：`zh-Hans-CN`、`zh_CN`、`zh` 都算中文，其余一律英文。这不是
    /// 偷懒——我们只有两种语言，"不是中文就用英文"是准确的判断，而按完整
    /// 标签精确匹配反而会把 `zh-Hant` 之类漏成英文。
    pub fn parse(tag: &str) -> Self {
        let tag = tag.trim().to_ascii_lowercase();
        if tag.starts_with("zh") {
            Lang::Zh
        } else {
            Lang::English
        }
    }
}

/// 当前语言。用原子量而不是 `Mutex`/`OnceLock`：每一条界面文案都要读它一次，
/// 托盘刷新时一次就是几十上百次，必须廉价到可以忽略。
static CURRENT: AtomicU8 = AtomicU8::new(0);

/// 设置界面语言。可随时调用——托盘里改语言会立即重建菜单。
pub fn set(lang: Lang) {
    CURRENT.store(lang.as_u8(), Ordering::Relaxed);
}

/// 当前界面语言。
pub fn current() -> Lang {
    Lang::from_u8(CURRENT.load(Ordering::Relaxed))
}

/// 当前是否为英文界面。供 [`t!`] 与 [`tf!`] 使用。
#[inline]
pub fn is_en() -> bool {
    CURRENT.load(Ordering::Relaxed) == 1
}

/// 就地双语的静态文案：`t!("中文", "English")`。
#[macro_export]
macro_rules! t {
    ($zh:literal, $en:literal) => {
        if $crate::i18n::is_en() {
            $en
        } else {
            $zh
        }
    };
}

/// 就地双语的格式化文案：`tf!("已连接 {n} 台", "{n} connected")`。
///
/// 两个字面量各自独立格式化，所以插值顺序可以不同——英文语序常常和中文不一样，
/// 硬套同一个参数顺序会译出别扭的句子。
///
/// **隐式命名捕获（`{name}`）能正常工作，但有个坑**：若某个变量**只**在这里
/// 被用到，`unused_variables` 会误报它没被使用——那个 lint 看不到宏展开后
/// 生成的捕获。功能是对的，可 CI 用 `-D warnings`，一条误报就够把构建弄红。
/// 遇到这种变量改成显式传参（`tf!("… {}", "… {}", x)`）即可。
#[macro_export]
macro_rules! tf {
    ($zh:literal, $en:literal) => {
        if $crate::i18n::is_en() {
            format!($en)
        } else {
            format!($zh)
        }
    };
    ($zh:literal, $en:literal, $($arg:tt)*) => {
        if $crate::i18n::is_en() {
            format!($en, $($arg)*)
        } else {
            format!($zh, $($arg)*)
        }
    };
}

/// 双语的 `println!`：`tprintln!("中文 {n}", "English {n}")`。
///
/// 命令行输出几乎全是"整行一句话"，写成 `println!("{}", tf!(..))` 每一条都要
/// 多包一层，读起来全是噪音。
#[macro_export]
macro_rules! tprintln {
    ($zh:literal, $en:literal) => {
        println!("{}", $crate::t!($zh, $en))
    };
    ($zh:literal, $en:literal, $($arg:tt)*) => {
        println!("{}", $crate::tf!($zh, $en, $($arg)*))
    };
}

/// 双语的 `eprintln!`。用法同 [`tprintln!`]。
#[macro_export]
macro_rules! teprintln {
    ($zh:literal, $en:literal) => {
        eprintln!("{}", $crate::t!($zh, $en))
    };
    ($zh:literal, $en:literal, $($arg:tt)*) => {
        eprintln!("{}", $crate::tf!($zh, $en, $($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 语言标签解析：只看是不是 zh 开头。
    #[test]
    fn locale_tags_are_matched_by_prefix() {
        for zh in [
            "zh",
            "zh-CN",
            "zh_CN",
            "zh-Hans",
            "zh-Hant-TW",
            "ZH-hans-cn",
        ] {
            assert_eq!(Lang::parse(zh), Lang::Zh, "{zh} 应判为中文");
        }
        for en in ["en", "en-US", "ja", "de-DE", "", "  "] {
            assert_eq!(Lang::parse(en), Lang::English, "{en:?} 应判为英文");
        }
    }

    /// 配置往返：写出去的字符串必须能再读回同一个值。
    #[test]
    fn config_round_trip() {
        for lang in [Lang::Zh, Lang::English] {
            assert_eq!(Lang::parse(lang.as_str()), lang);
        }
    }

    /// 默认是中文——设置从未初始化时也不能变成英文。
    ///
    /// 这条看着多余，但它锁住的是 `AtomicU8::new(0)` 与 `Lang::Zh = 0` 之间的
    /// 对应关系：谁哪天调整了枚举顺序，默认语言会**静默**翻转。
    #[test]
    fn the_default_is_chinese() {
        assert_eq!(Lang::default(), Lang::Zh);
        assert_eq!(Lang::from_u8(0), Lang::Zh);
    }
}
