//! 托盘图标的绘制。
//!
//! **图标由代码生成**而非打包图片文件，这样发布物始终是单个可执行文件，
//! 也免去了不同平台的资源打包差异。

use tray_icon::Icon;

use super::TrayStatus;

/// 托盘图标的三种视觉状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconState {
    /// 至少一台设备已连接——同步正常。
    Connected,
    /// 无设备连接。
    Disconnected,
    /// 用户主动暂停。
    Paused,
    /// 同步中枢已停止——功能实际不可用。
    Broken,
}

impl IconState {
    pub fn of(status: &TrayStatus) -> Self {
        // 故障优先于一切：这时候显示"已连接"是彻头彻尾的误导。
        if status.is_hub_dead() {
            IconState::Broken
        } else if status.is_paused() {
            IconState::Paused
        } else if status.snapshot().connected > 0 {
            IconState::Connected
        } else {
            IconState::Disconnected
        }
    }

    /// 该状态对应的主色（RGB）。
    fn color(self) -> (u8, u8, u8) {
        match self {
            // 绿：一切正常
            IconState::Connected => (0x35, 0xB5, 0x6A),
            // 灰：未连接
            IconState::Disconnected => (0x8A, 0x8A, 0x8A),
            // 琥珀：已暂停
            IconState::Paused => (0xE0, 0xA0, 0x30),
            // 红：出故障了，与"暂停"的琥珀明确区分开
            IconState::Broken => (0xD0, 0x3A, 0x3A),
        }
    }
}

/// 图标边长（像素）。
const ICON_SIZE: u32 = 32;

/// 按状态生成托盘图标：一个简化的剪贴板轮廓。
pub fn make_icon(state: IconState) -> anyhow::Result<Icon> {
    let rgba = draw_clipboard(state);
    Icon::from_rgba(rgba, ICON_SIZE, ICON_SIZE)
        .map_err(|e| anyhow::anyhow!("生成托盘图标失败: {e}"))
}

/// 绘制剪贴板形状的 RGBA 像素。
///
/// 形状：一个圆角板身，顶部一个夹子。用纯计算绘制，无需图片资源。
fn draw_clipboard(state: IconState) -> Vec<u8> {
    let (r, g, b) = state.color();
    let n = ICON_SIZE as i32;
    let mut px = vec![0u8; (ICON_SIZE * ICON_SIZE * 4) as usize];

    // 板身范围（留出边距）与夹子范围。
    let body = Rect {
        x0: 6,
        y0: 7,
        x1: n - 6,
        y1: n - 4,
    };
    let clip = Rect {
        x0: n / 2 - 5,
        y0: 3,
        x1: n / 2 + 5,
        y1: 9,
    };

    for y in 0..n {
        for x in 0..n {
            let idx = ((y * n + x) * 4) as usize;
            let in_body = body.contains_rounded(x, y, 3);
            let in_clip = clip.contains_rounded(x, y, 2);

            if in_clip {
                // 夹子用更深的同色，形成层次。
                px[idx] = r.saturating_sub(40);
                px[idx + 1] = g.saturating_sub(40);
                px[idx + 2] = b.saturating_sub(40);
                px[idx + 3] = 255;
            } else if in_body {
                px[idx] = r;
                px[idx + 1] = g;
                px[idx + 2] = b;
                px[idx + 3] = 255;
            }
            // 其余保持全透明。
        }
    }
    px
}

struct Rect {
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
}

impl Rect {
    /// 是否落在带圆角的矩形内。
    fn contains_rounded(&self, x: i32, y: i32, radius: i32) -> bool {
        if x < self.x0 || x >= self.x1 || y < self.y0 || y >= self.y1 {
            return false;
        }
        // 四角做圆角裁切。
        let corners = [
            (self.x0 + radius, self.y0 + radius),
            (self.x1 - 1 - radius, self.y0 + radius),
            (self.x0 + radius, self.y1 - 1 - radius),
            (self.x1 - 1 - radius, self.y1 - 1 - radius),
        ];
        for (cx, cy) in corners {
            let outside_x = (x < cx && cx == self.x0 + radius) || (x > cx && cx != self.x0 + radius);
            let outside_y = (y < cy && cy == self.y0 + radius) || (y > cy && cy != self.y0 + radius);
            if outside_x && outside_y {
                let dx = x - cx;
                let dy = y - cy;
                if dx * dx + dy * dy > radius * radius {
                    return false;
                }
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    fn icon_state_priority() {
        let s = TrayStatus::new(1);
        assert_eq!(IconState::of(&s), IconState::Disconnected);

        s.set_connected_ids(["dev-a".to_string()].into_iter().collect());
        assert_eq!(IconState::of(&s), IconState::Connected);

        // 暂停优先于连接状态——用户主动暂停时应明确显示。
        s.set_paused(true);
        assert_eq!(IconState::of(&s), IconState::Paused);
    }


    #[test]
    fn icon_pixels_have_expected_size_and_content() {
        let px = draw_clipboard(IconState::Connected);
        assert_eq!(px.len(), (ICON_SIZE * ICON_SIZE * 4) as usize);
        // 应有不透明像素（画出了图形），也应有透明像素（四周留白）。
        assert!(px.chunks(4).any(|p| p[3] == 255), "应绘制出可见图形");
        assert!(px.chunks(4).any(|p| p[3] == 0), "四周应为透明");
    }


    #[test]
    fn different_states_produce_different_icons() {
        let a = draw_clipboard(IconState::Connected);
        let b = draw_clipboard(IconState::Disconnected);
        let c = draw_clipboard(IconState::Paused);
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
    }


    #[test]
    fn broken_icon_is_visually_distinct() {
        let broken = draw_clipboard(IconState::Broken);
        for other in [IconState::Connected, IconState::Disconnected, IconState::Paused] {
            assert_ne!(broken, draw_clipboard(other), "故障图标应与 {other:?} 有区别");
        }
    }
}
