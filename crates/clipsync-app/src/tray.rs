//! 系统托盘：状态显示与常用操作入口。
//!
//! 托盘是本程序唯一的界面。设计原则是"平时不打扰、需要时找得到"：
//!   - 图标颜色即状态（已连接/未连接/已暂停），一眼可知同步是否正常。
//!   - 菜单只放真正需要的操作：配对、暂停、开机自启、退出。
//!
//! **图标由代码生成**而非打包图片文件，这样发布物始终是单个可执行文件，
//! 也免去了不同平台的资源打包差异。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tray_icon::Icon;

/// 同步状态，供托盘展示。
#[derive(Debug, Clone, Default)]
pub struct StatusData {
    /// 当前已连接的对端数。
    pub connected: usize,
    /// 已配对设备总数。
    pub paired: usize,
    /// 当前在线的设备 ID 集合，用于在设备列表里标出 ●/○。
    pub connected_ids: std::collections::HashSet<String>,
}

/// 线程安全的状态句柄：中枢更新，托盘读取。
#[derive(Clone, Default)]
pub struct TrayStatus {
    data: Arc<Mutex<StatusData>>,
    paused: Arc<AtomicBool>,
    /// 同步中枢是否已停止工作（异常退出）。
    hub_dead: Arc<AtomicBool>,
}

impl TrayStatus {
    pub fn new(paired: usize) -> Self {
        Self {
            data: Arc::new(Mutex::new(StatusData {
                connected: 0,
                paired,
                connected_ids: std::collections::HashSet::new(),
            })),
            paused: Arc::new(AtomicBool::new(false)),
            hub_dead: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 更新在线设备集合（同时刷新计数，避免两者脱节）。
    pub fn set_connected_ids(&self, ids: std::collections::HashSet<String>) {
        let mut g = self.data.lock().unwrap();
        g.connected = ids.len();
        g.connected_ids = ids;
    }

    /// 当前在线的设备 ID。
    pub fn connected_devices(&self) -> std::collections::HashSet<String> {
        self.data.lock().unwrap().connected_ids.clone()
    }

    /// 更新已配对设备总数。
    ///
    /// 配对可以在运行期从托盘发起，配完这个数就变了——不更新的话菜单首行
    /// 会一直停在"尚未配对设备"，而同步其实已经在工作了。
    pub fn set_paired(&self, n: usize) {
        self.data.lock().unwrap().paired = n;
    }

    pub fn snapshot(&self) -> StatusData {
        self.data.lock().unwrap().clone()
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::SeqCst);
    }

    /// 标记同步中枢已停止工作。
    ///
    /// 中枢线程若因意外退出，所有同步都会静默停止，而托盘图标还是绿的、
    /// 菜单还写着"已连接 N 台"——用户根本无从察觉，只会觉得"复制了怎么没
    /// 过去"。宁可明确显示故障，也不要给一个骗人的正常状态。
    pub fn set_hub_dead(&self) {
        self.hub_dead.store(true, Ordering::SeqCst);
    }

    pub fn is_hub_dead(&self) -> bool {
        self.hub_dead.load(Ordering::SeqCst)
    }

    /// 一句话状态描述，用于托盘提示文本。
    pub fn summary(&self) -> String {
        if self.is_hub_dead() {
            return "ClipSync — 同步已停止（请重启程序）".to_string();
        }
        if self.is_paused() {
            return "ClipSync — 已暂停".to_string();
        }
        let s = self.snapshot();
        if s.paired == 0 {
            "ClipSync — 尚未配对设备".to_string()
        } else if s.connected == 0 {
            format!("ClipSync — 未连接（已配对 {} 台）", s.paired)
        } else {
            format!("ClipSync — 已连接 {} / {} 台", s.connected, s.paired)
        }
    }
}

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

/// 托盘菜单被点击后需要主程序执行的动作。
// 不再是 Copy：`Unpair` 携带 device id。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayAction {
    TogglePause,
    ToggleAutostart,
    /// 主持配对：生成并显示配对码，等对方连入。
    ShowPairingCode,
    /// 加入配对：输入对方给的配对码，主动连过去。
    EnterPairingCode,
    Quit,
    /// 切换"发送图片到其它设备"。
    ToggleSendImages,
    /// 切换"发送文件到其它设备"。
    ToggleSendFiles,
    /// 切换传输前自动压缩。
    ToggleCompress,
    /// 设置单次内容大小上限（字节）。
    SetMaxBytes(usize),
    /// 设置发送限速（字节/秒，0 为不限速）。
    SetUploadLimit(u64),
    /// 弹输入框自定义单次大小上限。
    PromptMaxBytes,
    /// 弹输入框自定义发送限速。
    PromptUploadLimit,
    /// 弹输入框修改同步监听端口。
    PromptListenPort,
    /// 解除与某台设备的配对（携带其 device id）。
    Unpair(String),
    /// 打开日志所在文件夹。
    OpenLogDir,
    /// 切换详细日志（info ↔ debug）。
    ToggleVerboseLog,
}

/// 托盘要展示的一台已配对设备。
#[derive(Debug, Clone)]
pub struct TrayPeer {
    pub device: String,
    pub name: String,
    pub online: bool,
}

/// 单次大小上限的预设档。`usize::MAX` 表示不限制。
///
/// 预设档覆盖常见场景，档位之外由子菜单末尾的「自定义…」承接——它会弹一个
/// 输入框，接受 `500MB`、`1.5GiB` 这类写法。
///
/// （早先这里写的是"托盘菜单没有输入框，需要精确值的用户可直接改
/// `settings.json`"。自从配对流程引入 `dialog::prompt` 后这个前提就不成立了，
/// 而让用户去翻 `~/Library/Application Support/` 手改 JSON 显然不是好答案。）
const MAX_BYTES_PRESETS: &[(&str, usize)] = &[
    ("10 MiB", 10 * 1024 * 1024),
    ("100 MiB（默认）", 100 * 1024 * 1024),
    ("500 MiB", 500 * 1024 * 1024),
    ("2 GiB", 2 * 1024 * 1024 * 1024),
    ("不限制", usize::MAX),
];

/// 发送限速的预设档。`0` 表示不限速。
const UPLOAD_LIMIT_PRESETS: &[(&str, u64)] = &[
    ("不限速（默认）", 0),
    ("10 MB/s", 10 * 1000 * 1000),
    ("20 MB/s", 20 * 1000 * 1000),
    ("50 MB/s", 50 * 1000 * 1000),
];

/// 托盘需要展示的当前设置值（用于菜单初始勾选状态）。
#[derive(Debug, Clone, Copy)]
pub struct TraySettings {
    pub send_images: bool,
    pub send_files: bool,
    pub compress: bool,
    pub max_bytes: usize,
    pub upload_limit: u64,
    pub listen_port: u16,
    pub verbose_log: bool,
}

/// 「自定义…」项的标签。当前值不在预设档里时带上实际数值，让用户一眼看出
/// 现在生效的是多少——否则子菜单里一个勾都没有，会显得像没设置过。
fn custom_label_bytes(current: usize) -> String {
    if MAX_BYTES_PRESETS.iter().any(|(_, v)| *v == current) {
        "自定义…".to_string()
    } else {
        format!("自定义…（当前 {}）", human_bytes(current))
    }
}

fn custom_label_rate(current: u64) -> String {
    if UPLOAD_LIMIT_PRESETS.iter().any(|(_, v)| *v == current) {
        "自定义…".to_string()
    } else {
        format!("自定义…（当前 {}/s）", human_bytes(current as usize))
    }
}

/// 把字节数写成人能读的形式。挑最合适的单位，避免出现 "0.00 GiB" 这种。
fn human_bytes(n: usize) -> String {
    if n == usize::MAX {
        return "不限制".to_string();
    }
    const UNITS: &[(&str, usize)] = &[
        ("GiB", 1 << 30),
        ("MiB", 1 << 20),
        ("KiB", 1 << 10),
    ];
    for (unit, size) in UNITS {
        if n >= *size {
            let v = n as f64 / *size as f64;
            // 整数倍就不显示小数位，"100 MiB" 比 "100.0 MiB" 干净。
            return if (v.fract()).abs() < 0.05 {
                format!("{:.0} {unit}", v)
            } else {
                format!("{v:.1} {unit}")
            };
        }
    }
    format!("{n} B")
}

/// 托盘运行所需的回调。
pub struct TrayCallbacks {
    /// 处理一次菜单动作；返回 false 表示应退出程序。
    pub on_action: Box<dyn FnMut(TrayAction) -> bool>,
    /// 读取当前设置，用于点击后刷新勾选状态（以实际结果为准，避免脱节）。
    pub current_settings: Box<dyn Fn() -> TraySettings>,
    /// 读取当前已配对设备及其在线状态，用于渲染设备子菜单。
    pub current_peers: Box<dyn Fn() -> Vec<TrayPeer>>,
    /// 同步中枢是否仍在运行。托盘每轮询问一次；一旦为 false 就切到故障状态。
    pub hub_alive: Box<dyn Fn() -> bool>,
}

/// 重建「已配对设备」子菜单。
///
/// 设备列表在运行期会变（配对、解除配对），而菜单项是构建时创建的，所以每次
/// 变化都要整体重来一遍。返回新的 (菜单项, device id) 映射供点击时反查。
///
/// 列表为空时放一个禁用的提示项而不是留空白——空子菜单在两个平台上都显示为
/// 一个什么都没有的小方块，看着像坏了。
fn rebuild_peer_menu(
    menu: &tray_icon::menu::Submenu,
    old: &[(tray_icon::menu::MenuItem, String)],
    peers: &[TrayPeer],
) -> anyhow::Result<Vec<(tray_icon::menu::MenuItem, String)>> {
    use tray_icon::menu::MenuItem;

    for (item, _) in old {
        let _ = menu.remove(item);
    }
    // 上一轮的占位项也要清掉，否则会越堆越多。
    for item in menu.items() {
        let _ = menu.remove_at(0);
        drop(item);
    }

    let mut mapping = Vec::with_capacity(peers.len());
    if peers.is_empty() {
        let empty = MenuItem::new("（尚未配对任何设备）", false, None);
        menu.append(&empty)
            .map_err(|e| anyhow::anyhow!("构建设备子菜单失败: {e}"))?;
        return Ok(mapping);
    }

    for p in peers {
        // ● 在线 / ○ 离线，一眼能看出哪台连着。文案写明点击的后果——
        // 这是个破坏性操作，不能让人以为只是查看详情。
        let label = format!(
            "{} {} — 解除配对",
            if p.online { '●' } else { '○' },
            p.name
        );
        let item = MenuItem::new(label, true, None);
        menu.append(&item)
            .map_err(|e| anyhow::anyhow!("构建设备子菜单失败: {e}"))?;
        mapping.push((item, p.device.clone()));
    }
    Ok(mapping)
}

/// 在**主线程**上创建托盘并运行事件循环，直到用户选择退出。
///
/// 必须在主线程运行：macOS 要求 UI 操作在主线程，Windows 也要求托盘图标所属
/// 线程持有消息循环。因此同步逻辑全部放在后台线程，主线程专职跑这个循环。
pub fn run(status: TrayStatus, mut callbacks: TrayCallbacks) -> anyhow::Result<()> {
    use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
    use tray_icon::TrayIconBuilder;

    // 平台初始化需先于托盘构建：macOS 上 NSApp 未完成启动时创建的状态项
    // 不会响应点击。
    init_platform_app();

    let s0 = (callbacks.current_settings)();

    let menu = Menu::new();
    // 首项显示状态，不可点击，仅作信息展示。
    let status_item = MenuItem::new(status.summary(), false, None);
    // 配对的两端各给一个入口。此前只有"显示配对码"，加入方**只能**去命令行敲
    // `clipsync pair <码>`——而这是个托盘常驻的图形程序，多数用户根本不会开
    // 终端，等于配对只做了一半。
    let pair_item = MenuItem::new("显示配对码…（本机等待对方加入）", true, None);
    let join_item = MenuItem::new("输入配对码…（加入对方）", true, None);

    // 开关文案强调"发送"：这两项只拦截发出，收到的内容不受影响
    // （引擎只在 on_local_change 检查，on_remote 不检查）。写成"同步图片"
    // 会让人以为关掉后也不再接收。
    let send_images_item = CheckMenuItem::new("发送图片到其它设备", true, s0.send_images, None);
    let send_files_item = CheckMenuItem::new("发送文件到其它设备", true, s0.send_files, None);
    let compress_item = CheckMenuItem::new("传输前自动压缩", true, s0.compress, None);

    // 预设档用一组 CheckMenuItem 手工做成单选：muda 没有原生 radio 项，
    // 点击后由我们把同组其它项取消勾选。
    // 预设档 + 末尾的「自定义…」。自定义项做成普通菜单项而非勾选项：
    // 它是个动作（弹输入框），不是一个可勾选的状态。当前值不在任何预设档里
    // 时，标签会带上实际数值，让用户一眼看出"现在是自定义的多少"。
    let max_items: Vec<CheckMenuItem> = MAX_BYTES_PRESETS
        .iter()
        .map(|(label, v)| CheckMenuItem::new(*label, true, s0.max_bytes == *v, None))
        .collect();
    let max_custom = MenuItem::new(custom_label_bytes(s0.max_bytes), true, None);
    let max_menu = Submenu::new("单次大小上限", true);
    {
        let mut items: Vec<&dyn tray_icon::menu::IsMenuItem> =
            max_items.iter().map(|i| i as &dyn tray_icon::menu::IsMenuItem).collect();
        items.push(&max_custom);
        max_menu
            .append_items(&items)
            .map_err(|e| anyhow::anyhow!("构建上限子菜单失败: {e}"))?;
    }

    let rate_items: Vec<CheckMenuItem> = UPLOAD_LIMIT_PRESETS
        .iter()
        .map(|(label, v)| CheckMenuItem::new(*label, true, s0.upload_limit == *v, None))
        .collect();
    let rate_custom = MenuItem::new(custom_label_rate(s0.upload_limit), true, None);
    let rate_menu = Submenu::new("发送限速", true);
    {
        let mut items: Vec<&dyn tray_icon::menu::IsMenuItem> =
            rate_items.iter().map(|i| i as &dyn tray_icon::menu::IsMenuItem).collect();
        items.push(&rate_custom);
        rate_menu
            .append_items(&items)
            .map_err(|e| anyhow::anyhow!("构建限速子菜单失败: {e}"))?;
    }

    let port_item = MenuItem::new(format!("同步端口：{}…", s0.listen_port), true, None);

    // 已配对设备：列出每台及其在线状态，点击可解除配对。
    let peers_menu = Submenu::new("已配对设备", true);
    let mut peer_items = rebuild_peer_menu(&peers_menu, &[], &(callbacks.current_peers)())?;

    // 日志：出问题时用户唯一能自查的东西，入口要好找。
    let verbose_item = CheckMenuItem::new("详细日志（排查问题用）", true, s0.verbose_log, None);
    let log_dir_item = MenuItem::new("打开日志文件夹…", true, None);

    let pause_item = CheckMenuItem::new("暂停同步", true, status.is_paused(), None);
    let autostart_item =
        CheckMenuItem::new("开机自启", true, crate::autostart::is_enabled(), None);
    let quit_item = MenuItem::new("退出 ClipSync", true, None);

    menu.append_items(&[
        &status_item,
        &PredefinedMenuItem::separator(),
        &pair_item,
        &join_item,
        &peers_menu,
        &PredefinedMenuItem::separator(),
        &send_images_item,
        &send_files_item,
        &max_menu,
        &rate_menu,
        &compress_item,
        &port_item,
        &PredefinedMenuItem::separator(),
        &pause_item,
        &autostart_item,
        &verbose_item,
        &log_dir_item,
        &PredefinedMenuItem::separator(),
        &quit_item,
    ])
    .map_err(|e| anyhow::anyhow!("构建托盘菜单失败: {e}"))?;

    let mut current_icon = IconState::of(&status);
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(status.summary())
        .with_icon(make_icon(current_icon)?)
        .build()
        .map_err(|e| anyhow::anyhow!("创建托盘图标失败: {e}"))?;

    let menu_rx = MenuEvent::receiver();
    let mut last_summary = status.summary();

    loop {
        // 让平台处理其自身的窗口/菜单消息。
        pump_platform_events();

        // 处理菜单点击。
        while let Ok(event) = menu_rx.try_recv() {
            let action = if event.id == pause_item.id() {
                Some(TrayAction::TogglePause)
            } else if event.id == autostart_item.id() {
                Some(TrayAction::ToggleAutostart)
            } else if event.id == pair_item.id() {
                Some(TrayAction::ShowPairingCode)
            } else if event.id == join_item.id() {
                Some(TrayAction::EnterPairingCode)
            } else if event.id == quit_item.id() {
                Some(TrayAction::Quit)
            } else if event.id == send_images_item.id() {
                Some(TrayAction::ToggleSendImages)
            } else if event.id == send_files_item.id() {
                Some(TrayAction::ToggleSendFiles)
            } else if event.id == compress_item.id() {
                Some(TrayAction::ToggleCompress)
            } else if event.id == max_custom.id() {
                Some(TrayAction::PromptMaxBytes)
            } else if event.id == rate_custom.id() {
                Some(TrayAction::PromptUploadLimit)
            } else if event.id == port_item.id() {
                Some(TrayAction::PromptListenPort)
            } else if event.id == verbose_item.id() {
                Some(TrayAction::ToggleVerboseLog)
            } else if event.id == log_dir_item.id() {
                Some(TrayAction::OpenLogDir)
            } else if let Some((_, device)) =
                peer_items.iter().find(|(i, _)| i.id() == &event.id)
            {
                Some(TrayAction::Unpair(device.clone()))
            } else {
                // 两组预设档：按 id 找到被点的那一项。
                max_items
                    .iter()
                    .position(|i| i.id() == &event.id)
                    .map(|idx| TrayAction::SetMaxBytes(MAX_BYTES_PRESETS[idx].1))
                    .or_else(|| {
                        rate_items
                            .iter()
                            .position(|i| i.id() == &event.id)
                            .map(|idx| TrayAction::SetUploadLimit(UPLOAD_LIMIT_PRESETS[idx].1))
                    })
            };

            if let Some(a) = action {
                let keep_running = (callbacks.on_action)(a);
                if !keep_running {
                    return Ok(());
                }
                // 勾选状态一律以**实际生效值**为准，而不是按点击取反：
                // 保存失败或值被夹取时，界面不会与真实状态脱节。
                pause_item.set_checked(status.is_paused());
                autostart_item.set_checked(crate::autostart::is_enabled());

                let s = (callbacks.current_settings)();
                send_images_item.set_checked(s.send_images);
                send_files_item.set_checked(s.send_files);
                compress_item.set_checked(s.compress);
                // 预设档做成单选：只勾中与当前值相等的那项。
                for (item, (_, v)) in max_items.iter().zip(MAX_BYTES_PRESETS) {
                    item.set_checked(s.max_bytes == *v);
                }
                for (item, (_, v)) in rate_items.iter().zip(UPLOAD_LIMIT_PRESETS) {
                    item.set_checked(s.upload_limit == *v);
                }
                // 自定义项的标签带着当前值，改完要跟着变；端口项同理。
                max_custom.set_text(custom_label_bytes(s.max_bytes));
                rate_custom.set_text(custom_label_rate(s.upload_limit));
                port_item.set_text(format!("同步端口：{}…", s.listen_port));
                verbose_item.set_checked(s.verbose_log);
                // 解除配对会改变设备列表，重建一次。
                peer_items =
                    rebuild_peer_menu(&peers_menu, &peer_items, &(callbacks.current_peers)())?;
            }
        }

        // 中枢若已停止，同步实际已经不工作了——必须让界面如实反映，
        // 否则用户对着一个绿图标怎么也想不通"为什么复制过不去"。
        if !status.is_hub_dead() && !(callbacks.hub_alive)() {
            status.set_hub_dead();
        }

        // 状态变化时刷新图标与文字。
        let summary = status.summary();
        if summary != last_summary {
            status_item.set_text(&summary);
            let _ = tray.set_tooltip(Some(&summary));
            last_summary = summary;
            // 汇总变了意味着连接数变了，设备子菜单里的 ●/○ 也该跟着变。
            peer_items = rebuild_peer_menu(&peers_menu, &peer_items, &(callbacks.current_peers)())?;
        }
        let icon_state = IconState::of(&status);
        if icon_state != current_icon {
            if let Ok(icon) = make_icon(icon_state) {
                let _ = tray.set_icon(Some(icon));
            }
            current_icon = icon_state;
        }

        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// 处理平台原生消息，使托盘图标与菜单能够响应。
#[cfg(windows)]
fn pump_platform_events() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
    };

    // SAFETY: 标准的非阻塞消息泵。PeekMessage 取不到消息时立即返回 0。
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

#[cfg(target_os = "macos")]
fn pump_platform_events() {
    use objc2_app_kit::{NSApplication, NSEventMask};
    use objc2_foundation::{MainThreadMarker, NSDate, NSDefaultRunLoopMode};

    // 托盘必须在主线程运行（见 `run` 的文档）。非主线程时静默返回而不 panic：
    // 事件泵取不到事件只会让菜单无响应，不该让整个程序崩溃。
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);

    // distantPast 作为超时点表示"绝不等待"：有事件就取走，没有立即返回 None。
    // 这样循环不会阻塞，主线程仍能按 200ms 节奏刷新图标与处理菜单事件。
    // SAFETY: NSDefaultRunLoopMode 是 AppKit 导出的常量字符串，读取始终有效。
    let mode = unsafe { NSDefaultRunLoopMode };
    while let Some(event) = app.nextEventMatchingMask_untilDate_inMode_dequeue(
        NSEventMask::Any,
        Some(&NSDate::distantPast()),
        mode,
        true,
    ) {
        app.sendEvent(&event);
    }
}

/// macOS 专用：进入事件循环前初始化 NSApp。
///
/// 两件事缺一不可：
///   - `Accessory` 激活策略——托盘程序不应在 Dock 里占一个图标。
///   - `finishLaunching`——不调用则 AppKit 未完成启动流程，菜单点击无响应。
#[cfg(target_os = "macos")]
fn init_platform_app() {
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
    use objc2_foundation::MainThreadMarker;

    let Some(mtm) = MainThreadMarker::new() else {
        tracing::warn!("托盘未在主线程启动，菜单可能无响应");
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    app.finishLaunching();
}

#[cfg(not(target_os = "macos"))]
fn init_platform_app() {}

#[cfg(not(any(windows, target_os = "macos")))]
fn pump_platform_events() {
    // 其它平台无需额外的消息泵。
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_reflects_state() {
        let s = TrayStatus::new(0);
        assert!(s.summary().contains("尚未配对"));

        let s = TrayStatus::new(2);
        assert!(s.summary().contains("未连接"));

        s.set_connected_ids(["dev-a".to_string()].into_iter().collect());
        assert!(s.summary().contains("已连接 1 / 2"));

        s.set_paused(true);
        assert!(s.summary().contains("已暂停"), "暂停状态应优先展示");
    }

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

    /// 中枢停止后，界面不得再显示"一切正常"。
    ///
    /// 同步已经彻底不工作了，而托盘图标还是绿的、菜单还写着"已连接 N 台"
    /// ——用户对着这个界面永远想不通"为什么复制过不去"。
    #[test]
    fn dead_hub_overrides_healthy_looking_state() {
        let s = TrayStatus::new(2);
        s.set_connected_ids(["dev-a".to_string()].into_iter().collect());
        assert_eq!(IconState::of(&s), IconState::Connected);

        s.set_hub_dead();
        assert_eq!(
            IconState::of(&s),
            IconState::Broken,
            "中枢已死时不能还显示已连接"
        );
        assert!(
            s.summary().contains("同步已停止"),
            "文字也要如实说明，实际: {}",
            s.summary()
        );
    }

    /// 故障优先于暂停：两者都成立时该显示故障，暂停是用户自己知道的事。
    #[test]
    fn broken_takes_precedence_over_paused() {
        let s = TrayStatus::new(1);
        s.set_paused(true);
        s.set_hub_dead();
        assert_eq!(IconState::of(&s), IconState::Broken);
    }

    #[test]
    fn broken_icon_is_visually_distinct() {
        let broken = draw_clipboard(IconState::Broken);
        for other in [IconState::Connected, IconState::Disconnected, IconState::Paused] {
            assert_ne!(broken, draw_clipboard(other), "故障图标应与 {other:?} 有区别");
        }
    }

    #[test]
    fn pause_state_is_shared_across_clones() {
        let s = TrayStatus::new(1);
        let clone = s.clone();
        s.set_paused(true);
        assert!(
            clone.is_paused(),
            "克隆出的句柄应看到同一份暂停状态（中枢与托盘共享）"
        );
    }
}
