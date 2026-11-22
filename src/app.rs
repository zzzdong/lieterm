//! zzzui 应用层：连接对话框（本地 / SSH）+ 终端画面 + 菜单栏 + 后台事件泵。
//!
//! 启动即打开一个模态对话框：两个选项卡（本地终端 / SSH），本地终端用下拉选择 shell。
//! 菜单栏的「文件 / 会话 / 帮助」下拉与 shell 下拉都走 zzzui 的 `Menu` 浮层；
//! 终端像素走 `Runtime::set_background_image`，状态栏是普通 Widget 声明。

use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use zzzui::{
    element::{
        AlignItems, Cursor, Decl, DragPayload, FlexDirection, InteractionState, JustifyContent,
        Length, LocalKey, NodeKey, PointerEvents, PositionType, ROOT_KEY, RawCommand, Style,
        VisualOverrides, child_key,
    },
    geom::{Color, Corners, Edges, Size},
    platform::{AppHandler, WindowContext, WindowId, WindowOpener},
    view::{OverlayView, ViewNode},
    widget::{
        BgToken, BorderToken, Button, Center, Column, Dialog, Family, Key, KeyCapture, Label, Menu,
        RadiusToken, Row, Scroll, Shade, Tabs, TextField, TextToken, UiTheme, UpdateCtx, Widget,
    },
};

use crate::{
    config::{self, Config, Macro, SessionProfile, macro_bytes},
    keys, local, sftp,
    sftp::SftpAction,
    ssh,
    ssh::{AuthOptions, ConnectOptions},
    term::{self, STATUS_HEIGHT, Terminal},
    transfer::{self, Direction, Probe, Transfer, TransferEvent},
};

/// 菜单栏高度（逻辑像素）。
const MENUBAR_HEIGHT: f64 = 28.0;
/// 会话标签栏高度（zzzui `Tabs` 用 `Comp(ControlHeight::Xxl)` = 48）。
const TAB_BAR_HEIGHT: f64 = 48.0;
/// 声明槽位：对话框 / 菜单栏根 / 菜单浮层 / shell 下拉 / 标签栏。
const DIALOG_SLOT: LocalKey = 900;
const MENUBAR_SLOT: LocalKey = 901;
const MENU_POPUP_BASE: LocalKey = 910;
const SHELL_POPUP_SLOT: LocalKey = 920;
/// SSH 认证方式下拉的浮层槽位。
const AUTH_POPUP_SLOT: LocalKey = 921;
/// 连接对话框里输入框 / 下拉框的统一宽度，以及标签列宽度。
const FIELD_WIDTH: f64 = 300.0;
const LABEL_WIDTH: f64 = 72.0;
const TAB_BAR_SLOT: LocalKey = 970;
/// 标签栏内每个标签占 2 个槽位（选中 / 关闭）；标签右键菜单。
const TAB_ENTRY_BASE: LocalKey = 2000;
const TAB_MENU_SLOT: LocalKey = 990;
/// SFTP 侧边栏宽度与目录项槽位起点。
const SIDEBAR_WIDTH: f64 = 280.0;
const SFTP_ENTRY_BASE: LocalKey = 930;
/// 快速连接侧边栏宽度；每个会话行占 1 个槽位，右键菜单单独一个槽位。
const QUICK_WIDTH: f64 = 260.0;
const QUICK_ENTRY_BASE: LocalKey = 1000;
const QUICK_MENU_SLOT: LocalKey = 991;
/// 会话 / 宏管理浮层与列表项槽位。
const SESSIONS_SLOT: LocalKey = 940;
const MACROS_SLOT: LocalKey = 941;
const SESSION_ENTRY_BASE: LocalKey = 950;
const MACRO_ENTRY_BASE: LocalKey = 960;
/// 终端右键菜单与 rz 上传路径对话框。
const CONTEXT_SLOT: LocalKey = 980;
const UPLOAD_SLOT: LocalKey = 981;

/// SSH 认证方式下拉选项：0 = 密码，1 = 密钥。
const AUTH_OPTIONS: &[&str] = &["密码", "密钥"];

/// 菜单栏定义：(标题, 选项)。选项序号与 `on_menu` 的分支一一对应。
const MENUS: &[(&str, &[&str])] = &[
    ("文件", &["新建连接", "退出"]),
    (
        "会话",
        &[
            "快速连接侧边栏",
            "清屏",
            "断开",
            "SFTP 侧边栏",
            "已保存会话…",
            "宏…",
        ],
    ),
    (
        "终端",
        &[
            "深色配色",
            "浅色配色",
            "Solarized 配色",
            "编码 UTF-8",
            "编码 GBK",
            "编码 GB18030",
        ],
    ),
    ("帮助", &["关于"]),
];

// 本地 shell 选项在 Root 构建时按操作系统探测（`local::available_shells`）。

/// 会话页布局几何：菜单栏 / 标签栏 / 快速连接面板 / SFTP 侧栏共同决定终端视口。
///
/// 声明树（`Root::view`）、命中测试（`cell_at` / `in_terminal`）与光栅化
/// （`on_frame` 的尺寸和偏移）必须共用这一份推导：任一侧漏掉侧栏宽度，
/// 终端像素与行列坐标就会错位。
#[derive(Clone, Copy, Debug, PartialEq)]
struct Geometry {
    /// 终端视口左边界（快速连接面板右侧）。
    left: f64,
    /// 终端视口右边界（SFTP 侧栏左侧）。
    right: f64,
    /// 终端视口上边界（菜单栏 + 标签栏下方）。
    top: f64,
    /// 终端视口下边界（状态栏上方）。
    bottom: f64,
}

impl Geometry {
    fn new(width: f64, height: f64, has_tabs: bool, quick_open: bool, sftp_open: bool) -> Self {
        let width = width.max(1.0);
        let height = height.max(1.0);
        let tab_height = if has_tabs { TAB_BAR_HEIGHT } else { 0.0 };
        let left = if quick_open { QUICK_WIDTH } else { 0.0 };
        let right = (width - if sftp_open { SIDEBAR_WIDTH } else { 0.0 }).max(left + 1.0);
        let top = MENUBAR_HEIGHT + tab_height;
        let bottom = (height - STATUS_HEIGHT).max(top + 1.0);
        Self {
            left,
            right,
            top,
            bottom,
        }
    }

    fn width(self) -> f64 {
        (self.right - self.left).max(1.0)
    }

    fn height(self) -> f64 {
        (self.bottom - self.top).max(1.0)
    }

    fn contains(self, x: f64, y: f64) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }

    /// 终端列数：右侧留 8 逻辑像素余量，避免最后半列被裁。
    fn cols(self, cell_width: f64) -> usize {
        ((self.width() - 8.0) / cell_width).floor().max(1.0) as usize
    }

    fn rows(self, cell_height: f64) -> usize {
        (self.height() / cell_height).floor().max(1.0) as usize
    }
}

zzzui::key_enum! {
    enum Act {
        Host,
        Port,
        User,
        Password,
        Connect,
        Cancel,
        Tabs,
        NewTab,
        Shell,
        MenuFile,
        MenuSession,
        MenuHelp,
        Identity,
        SftpPath,
        SftpLocal,
        SftpUp,
        SftpRefresh,
        SftpEnter,
        SftpUpload,
        SftpDownload,
        SftpCancel,
        SftpClose,
        SessionName,
        SessionConnect,
        SessionSave,
        SessionDelete,
        SessionClose,
        MacroName,
        MacroText,
        MacroEnter,
        MacroNew,
        MacroSave,
        MacroRun,
        MacroDelete,
        MacroClose,
        UploadPath,
        UploadAccept,
        UploadCancel,
        // SSH 认证方式下拉（0 = 密码，1 = 密钥）。
        AuthMode,
        // 快速连接侧边栏：内联编辑器字段与动作。
        QuickName,
        QuickHost,
        QuickPort,
        QuickUser,
        QuickIdentity,
        QuickNew,
        QuickSave,
        QuickCancel,
    }
}

pub enum UiEvent {
    Input(Vec<u8>),
    Resize {
        cols: u32,
        rows: u32,
        px_width: u32,
        px_height: u32,
    },
}

pub enum SshEvent {
    Connected(String),
    Data(Vec<u8>),
    Error(String),
    Closed,
    /// SFTP 连接成功并拿到首屏目录。
    SftpReady {
        path: String,
        entries: Vec<SftpEntry>,
    },
    /// SFTP 列目录完成。
    SftpListing {
        path: String,
        entries: Vec<SftpEntry>,
    },
    /// SFTP 操作提示 / 错误（不切换主会话状态）。
    SftpNotice(String),
    SftpError(String),
    /// 大文件传输进度；`active == false` 表示本次传输结束（成功 / 取消 / 失败）。
    SftpProgress {
        done: u64,
        total: u64,
        active: bool,
    },
}

pub enum UiAction {
    /// 新建一个 SSH 标签（输入通道与终端由应用层创建）。
    Connect {
        options: ConnectOptions,
    },
    /// 新建一个本地 shell 标签（pwsh / cmd / bash），不经过 SSH。
    ConnectLocal {
        shell: String,
    },
    /// 切换到第 `index` 个标签。
    SelectTab(usize),
    /// 关闭第 `index` 个标签。
    CloseTab(usize),
    /// 复制第 `index` 个标签：SSH 直接重连，本地 shell 重新启动。
    DuplicateTab(usize),
    /// 清屏（菜单「会话 > 清屏」）。
    Clear,
    /// 断开当前会话并重新打开连接对话框。
    Disconnect,
    /// 关闭窗口。
    Quit,
    Scroll(f64),
    /// 打开 SFTP 侧边栏（独立 SSH 连接）。
    SftpOpen {
        options: AuthOptions,
    },
    /// 转交给当前 SFTP 任务的请求。
    SftpAction(SftpAction),
    SftpClose,
    /// 复制当前选中文本到系统剪贴板。
    Copy,
    /// 把系统剪贴板文本粘贴到活动终端。
    Paste,
    /// 选中活动终端的可见区域。
    SelectAll,
    /// 开始一次鼠标选择（坐标是窗口逻辑像素）。
    SelectStart {
        x: f64,
        y: f64,
    },
    /// 拖动中更新选择终点。
    SelectUpdate {
        x: f64,
        y: f64,
    },
    /// 单击清除已有选择。
    SelectClear,
    /// 远端 rz 请求接收文件：用给定本地路径开始发送。
    UploadAccept {
        path: String,
    },
    /// 取消远端 rz 上传并通知对端。
    UploadCancel,
    /// 切换终端配色（立即应用到所有标签）。
    SetScheme(String),
    /// 切换终端编码（现有标签重置解码器，新标签沿用）。
    SetEncoding(String),
}

/// 远端目录项（UI 与 SFTP 任务之间的值类型）。
#[derive(Clone, Debug, Default)]
pub struct SftpEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

/// SFTP 侧边栏状态（由后台事件更新，Widget 每帧读取）。
#[derive(Clone, Debug, Default)]
pub struct SftpState {
    pub open: bool,
    pub path: String,
    pub entries: Vec<SftpEntry>,
    pub selected: Option<usize>,
    /// 上传源文件 / 下载目标文件。
    pub local_path: String,
    pub notice: String,
    /// 正在进行的传输进度（None = 空闲）。
    pub progress: Option<TransferProgress>,
}

/// 一次 SFTP 传输的字节进度。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TransferProgress {
    pub done: u64,
    /// 总大小；远端/本地取不到时为 0（UI 只显示已传字节）。
    pub total: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    /// 没有会话（启动后的默认状态，连接对话框打开）。
    #[default]
    Idle,
    Connecting,
    Session,
    Failed,
}

type InputSlot = Arc<Mutex<Option<UnboundedSender<UiEvent>>>>;

/// 标签标识：事件与动作都带它，避免后台任务把数据送错标签。
pub type TabId = u64;

/// 标签栏渲染用的只读摘要（标题由远端 OSC / 连接标签派生）。
#[derive(Clone, Debug)]
pub struct TabSummary {
    pub title: String,
}

pub struct Shared {
    pub screen: Screen,
    pub width: f64,
    pub height: f64,
    /// 连接状态原文（不含尺寸装饰）。
    pub label: String,
    /// 状态栏最终显示文本。
    pub status: String,
    /// 是否允许系统 IME（默认关闭：终端里输入 ASCII 命令时不该被拼音组合）。
    pub ime: bool,
    /// 连接对话框是否打开（启动时为 true；连接建立后关闭）。
    pub dialog_open: bool,
    /// 快速连接侧边栏是否展开（全局，不属于单个标签）。
    pub quick_open: bool,
    /// SFTP 侧边栏状态（当前活动标签的投影）。
    pub sftp: SftpState,
    /// 当前活动标签的输入通道（按键 / 宏 / resize 都走它）。
    pub input: InputSlot,
    /// 所有标签的摘要（标签栏逐项渲染）。
    pub tabs: Vec<TabSummary>,
    /// 活动标签在 `tabs` 里的下标。
    pub active_tab: usize,
    /// 远端 rz 正在等待用户填写本地文件路径。
    pub upload_open: bool,
    pub upload_path: String,
    pub upload_notice: String,
    /// ZMODEM 传输进行中：屏蔽普通按键，避免污染协议流。
    pub transfer_active: bool,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            screen: Screen::Idle,
            width: 800.0,
            height: 600.0,
            label: String::new(),
            status: String::new(),
            ime: false,
            dialog_open: true,
            quick_open: false,
            sftp: SftpState::default(),
            input: Arc::new(Mutex::new(None)),
            tabs: Vec::new(),
            active_tab: 0,
            upload_open: false,
            upload_path: String::new(),
            upload_notice: String::new(),
            transfer_active: false,
        }
    }
}

/// 后台任务用它把数据到达事件送到 UI 线程，并请求一次重画。
///
/// 每个标签一个 `Bridge`：`tab` 随事件一起入队，UI 侧据此路由到对应终端。
#[derive(Clone)]
pub struct Bridge {
    tab: TabId,
    events: UnboundedSender<(TabId, SshEvent)>,
    redraw: Arc<Mutex<Option<(WindowOpener, WindowId)>>>,
}

impl Bridge {
    pub fn new(
        tab: TabId,
        events: UnboundedSender<(TabId, SshEvent)>,
        redraw: Arc<Mutex<Option<(WindowOpener, WindowId)>>>,
    ) -> Self {
        Self {
            tab,
            events,
            redraw,
        }
    }

    pub fn notify(&self, event: SshEvent) {
        let _ = self.events.send((self.tab, event));
        if let Ok(slot) = self.redraw.lock()
            && let Some((opener, id)) = slot.as_ref()
        {
            opener.redraw(*id);
        }
    }
    /// 测试用：只收事件、不需要窗口的 Bridge（内存 SSH 服务器集成测试）。
    #[cfg(test)]
    pub(crate) fn from_events(tab: TabId, events: UnboundedSender<(TabId, SshEvent)>) -> Self {
        Self {
            tab,
            events,
            redraw: Arc::new(Mutex::new(None)),
        }
    }
}

struct Root {
    shared: Arc<Mutex<Shared>>,
    actions: std_mpsc::Sender<UiAction>,

    host: String,
    port: String,
    user: String,
    password: String,
    identity: String,
    /// SSH 认证方式：0 = 密码，1 = 密钥（决定对话框展示哪些字段）。
    auth: usize,
    tab: usize,
    shell: usize,
    /// 本机探测到的本地 shell：(显示名, 启动路径)。
    shells: Vec<(String, String)>,
    open_menu: Option<usize>,
    shell_open: bool,
    /// SSH 认证方式下拉是否展开。
    auth_open: bool,
    /// 快速连接侧边栏的内联编辑器：是否打开、正在编辑的会话下标（None = 新建）。
    quick_editing: bool,
    quick_edit_index: Option<usize>,
    quick_notice: String,
    /// 侧边栏当前选中的会话（单击只选中，双击才连接）。
    quick_selected: Option<usize>,
    /// 会话行右键菜单：被点的会话下标与弹出位置。
    quick_menu: Option<(usize, f64, f64)>,
    /// 上一次单击的会话与时间，用于识别双击。
    quick_last_click: Option<(usize, Instant)>,

    /// 会话与宏的本地配置（`~/.config/lieterm/config.json`）。
    config: Config,
    config_path: PathBuf,
    sessions_open: bool,
    macros_open: bool,
    /// 会话管理：名称输入与当前选中项。
    session_name: String,
    session_selected: Option<usize>,
    /// 宏管理：编辑字段与当前选中项。
    macro_name: String,
    macro_text: String,
    macro_enter: bool,
    macro_selected: Option<usize>,
    /// 两个管理浮层共用的操作提示。
    manager_notice: String,
    /// 终端右键菜单的位置（逻辑像素）。
    context_menu: Option<(f64, f64)>,
    /// 标签右键菜单：被点的标签下标与弹出位置。
    tab_menu: Option<(usize, f64, f64)>,
    /// 当前这次拖动是否被终端选择接管。
    selecting: bool,
}

impl Widget for Root {
    fn view(&self, theme: &UiTheme) -> ViewNode {
        let shared = self.shared.lock().expect("shared lock");
        let width = shared.width.max(1.0);
        let height = shared.height.max(1.0);
        let menubar = self.menubar_view(theme);
        let tab_bar = (!shared.tabs.is_empty()).then(|| self.tab_bar_view(theme, &shared));
        let tab_height = if tab_bar.is_some() {
            TAB_BAR_HEIGHT
        } else {
            0.0
        };

        if shared.screen == Screen::Session {
            let geom = Geometry::new(
                width,
                height,
                !shared.tabs.is_empty(),
                shared.quick_open,
                shared.sftp.open,
            );
            let terminal = KeyCapture::new()
                .size(geom.width(), geom.height())
                .ime(shared.ime)
                .view(theme);
            let body_width = (width - geom.left).max(1.0);
            let mut body = if shared.sftp.open {
                Row::new()
                    .width(body_width)
                    .height(geom.height())
                    .view(theme)
                    .with_children([
                        self.sftp_sidebar_view(theme, geom.height(), &shared.sftp),
                        terminal,
                    ])
            } else {
                terminal
            };
            // 快速连接面板在最左侧，SFTP 侧边栏紧随其后，终端占剩余宽度。
            if shared.quick_open {
                body = Row::new()
                    .width(width)
                    .height(geom.height())
                    .view(theme)
                    .with_children([self.quick_connect_view(theme, geom.height()), body]);
            }
            let mut children = vec![menubar];
            children.extend(tab_bar);
            children.push(body);
            children.push(self.status_view(theme, &shared));
            Column::new()
                .width(width)
                .height(height)
                .view(theme)
                .with_children(children)
        } else {
            let hint = if shared.screen == Screen::Connecting {
                "正在连接…".to_owned()
            } else if shared.label.is_empty() {
                "文件 > 新建连接".to_owned()
            } else {
                shared.label.clone()
            };
            let content = Center::new()
                .grow(1.0)
                .view(theme)
                .with_children([Label::new(hint).view(theme)]);
            let mut children = vec![menubar];
            children.extend(tab_bar);
            if shared.quick_open {
                let area = (height - MENUBAR_HEIGHT - tab_height).max(1.0);
                children.push(
                    Row::new()
                        .width(width)
                        .height(area)
                        .view(theme)
                        .with_children([self.quick_connect_view(theme, area), content]),
                );
            } else {
                children.push(content);
            }
            Column::new()
                .width(width)
                .height(height)
                .background(theme.bg(BgToken::Page))
                .view(theme)
                .with_children(children)
        }
    }

    fn overlays(&self, owner: NodeKey, theme: &UiTheme) -> Vec<OverlayView> {
        let shared = self.shared.lock().expect("shared lock");
        // 管理浮层打开时先收起连接对话框，关闭管理后对话框自动回来。
        let dialog_open = shared.dialog_open && !self.sessions_open && !self.macros_open;
        let upload = shared
            .upload_open
            .then(|| (shared.upload_path.clone(), shared.upload_notice.clone()));
        let mut out = Vec::new();
        if dialog_open {
            out.push(self.dialog(owner, theme, &shared));
        }
        drop(shared);

        if let Some((path, notice)) = upload {
            out.push(self.upload_dialog(owner, theme, &path, &notice));
        }

        if self.sessions_open {
            out.push(self.session_manager(owner, theme));
        }
        if self.macros_open {
            out.push(self.macro_manager(owner, theme));
        }

        if let Some(index) = self.open_menu {
            let (_, items) = MENUS[index];
            out.push(
                Menu::new(items.iter().copied())
                    .anchored(menu_title_key(owner, index))
                    .key(child_key(owner, MENU_POPUP_BASE + index as LocalKey))
                    .width(180.0)
                    .build(owner, theme),
            );
        }

        if self.shell_open && dialog_open {
            out.push(
                Menu::new(self.shells.iter().map(|(name, _)| name.as_str()))
                    .anchored(child_key(owner, Act::Shell.local()))
                    .key(child_key(owner, SHELL_POPUP_SLOT))
                    .selected(self.shell)
                    .width(FIELD_WIDTH)
                    .build(owner, theme),
            );
        }

        if self.auth_open && dialog_open && self.tab == 1 {
            out.push(
                Menu::new(AUTH_OPTIONS.iter().copied())
                    .anchored(child_key(owner, Act::AuthMode.local()))
                    .key(child_key(owner, AUTH_POPUP_SLOT))
                    .selected(self.auth)
                    .width(FIELD_WIDTH)
                    .build(owner, theme),
            );
        }

        if let Some((x, y)) = self.context_menu {
            out.push(
                Menu::new(["复制", "粘贴", "全选"])
                    .at(x, y)
                    .key(child_key(owner, CONTEXT_SLOT))
                    .width(150.0)
                    .build(owner, theme),
            );
        }

        if let Some((_, x, y)) = self.tab_menu {
            out.push(
                Menu::new(["复制会话", "关闭标签页"])
                    .at(x, y)
                    .key(child_key(owner, TAB_MENU_SLOT))
                    .width(150.0)
                    .build(owner, theme),
            );
        }

        if let Some((_, x, y)) = self.quick_menu {
            out.push(
                Menu::new(["连接", "编辑", "复制", "删除"])
                    .at(x, y)
                    .key(child_key(owner, QUICK_MENU_SLOT))
                    .width(130.0)
                    .build(owner, theme),
            );
        }
        out
    }

    fn on_command(&mut self, owner: NodeKey, command: &RawCommand, ctx: &mut UpdateCtx) -> bool {
        let consumed = self.on_command_inner(owner, command, ctx);
        if consumed {
            ctx.request_frame();
        }
        consumed
    }

    /// 会话中允许从终端区域发起拖动：拖动事件随后被解释为文本选择。
    fn drag_payload(&self) -> Option<DragPayload> {
        let shared = self.shared.lock().ok()?;
        (shared.screen == Screen::Session
            && !shared.dialog_open
            && !self.sessions_open
            && !self.macros_open
            && self.open_menu.is_none()
            && !self.shell_open
            && !shared.upload_open)
            .then(|| DragPayload::text(""))
    }
}
impl Root {
    fn on_command_inner(
        &mut self,
        owner: NodeKey,
        command: &RawCommand,
        ctx: &mut UpdateCtx,
    ) -> bool {
        // 1. 浮层关闭请求：对话框 / 管理浮层 / 菜单栏下拉 / shell 下拉各自只关自己。
        if let RawCommand::OverlayDismiss { key, .. } = command {
            if self.sessions_open && *key == child_key(owner, SESSIONS_SLOT) {
                self.sessions_open = false;
                self.manager_notice.clear();
                ctx.mark_dirty();
                return true;
            }
            if self.macros_open && *key == child_key(owner, MACROS_SLOT) {
                self.macros_open = false;
                self.manager_notice.clear();
                ctx.mark_dirty();
                return true;
            }
            if self.context_menu.is_some() && *key == child_key(owner, CONTEXT_SLOT) {
                self.context_menu = None;
                ctx.mark_dirty();
                return true;
            }
            if self.tab_menu.is_some() && *key == child_key(owner, TAB_MENU_SLOT) {
                self.tab_menu = None;
                ctx.mark_dirty();
                return true;
            }
            if self.quick_menu.is_some() && *key == child_key(owner, QUICK_MENU_SLOT) {
                self.quick_menu = None;
                ctx.mark_dirty();
                return true;
            }
            if *key == child_key(owner, UPLOAD_SLOT) {
                let _ = self.actions.send(UiAction::UploadCancel);
                ctx.mark_dirty();
                return true;
            }
            if *key == child_key(owner, DIALOG_SLOT) {
                self.set_dialog(false);
                self.shell_open = false;
                self.auth_open = false;
                ctx.mark_dirty();
                return true;
            }
            if self.shell_open && *key == child_key(owner, SHELL_POPUP_SLOT) {
                self.shell_open = false;
                ctx.mark_dirty();
                return true;
            }
            if self.auth_open && *key == child_key(owner, AUTH_POPUP_SLOT) {
                self.auth_open = false;
                ctx.mark_dirty();
                return true;
            }
            if let Some(index) = self.open_menu
                && *key == child_key(owner, MENU_POPUP_BASE + index as LocalKey)
            {
                self.open_menu = None;
                ctx.mark_dirty();
                return true;
            }
        }

        // 2. 文本输入（对话框里的 SSH 字段）。
        if let RawCommand::TextChanged { value, .. } = command {
            match command.key::<Act>(owner) {
                Some(Act::Host) => self.host = value.to_string(),
                Some(Act::Port) => self.port = value.to_string(),
                Some(Act::User) => self.user = value.to_string(),
                Some(Act::Password) => self.password = value.to_string(),
                Some(Act::Identity) => self.identity = value.to_string(),
                Some(Act::SftpPath) => {
                    if let Ok(mut shared) = self.shared.lock() {
                        shared.sftp.path = value.to_string();
                    }
                }
                Some(Act::SftpLocal) => {
                    if let Ok(mut shared) = self.shared.lock() {
                        shared.sftp.local_path = value.to_string();
                    }
                }
                Some(Act::SessionName) => self.session_name = value.to_string(),
                Some(Act::QuickName) => self.session_name = value.to_string(),
                Some(Act::QuickHost) => self.host = value.to_string(),
                Some(Act::QuickPort) => self.port = value.to_string(),
                Some(Act::QuickUser) => self.user = value.to_string(),
                Some(Act::QuickIdentity) => self.identity = value.to_string(),
                Some(Act::MacroName) => self.macro_name = value.to_string(),
                Some(Act::MacroText) => self.macro_text = value.to_string(),
                Some(Act::UploadPath) => {
                    if let Ok(mut shared) = self.shared.lock() {
                        shared.upload_path = value.to_string();
                    }
                }
                _ => return false,
            }
            ctx.mark_dirty();
            return true;
        }

        // 会话中的按键：编码成 xterm 序列写入 PTY（passive 面把原始按键交上来）。
        if let RawCommand::Key {
            name,
            text,
            ctrl,
            shift,
            alt,
            ..
        } = command
        {
            let (screen, input, transfer_active) = {
                let shared = self.shared.lock().expect("shared lock");
                (
                    shared.screen,
                    Arc::clone(&shared.input),
                    shared.transfer_active,
                )
            };
            if screen != Screen::Session {
                return false;
            }
            // ZMODEM 传输期间普通按键会污染协议流，直接吞掉；Esc 取消传输。
            if transfer_active {
                if name.as_ref() == "Escape" {
                    let _ = self.actions.send(UiAction::UploadCancel);
                }
                return true;
            }
            if *ctrl && *shift {
                match name.as_ref() {
                    "c" | "C" => {
                        let _ = self.actions.send(UiAction::Copy);
                        return true;
                    }
                    "v" | "V" => {
                        let _ = self.actions.send(UiAction::Paste);
                        return true;
                    }
                    "a" | "A" => {
                        let _ = self.actions.send(UiAction::SelectAll);
                        return true;
                    }
                    _ => {}
                }
            }
            let bytes = keys::encode(name, text.as_deref(), *ctrl, *shift, *alt);
            if !bytes.is_empty()
                && let Ok(slot) = input.lock()
                && let Some(sender) = slot.as_ref()
            {
                let _ = sender.send(UiEvent::Input(bytes));
            }
            return true;
        }

        // 终端拖动选择：只接管从终端区域开始的拖动。
        match command {
            RawCommand::DragStart {
                start_x, start_y, ..
            } => {
                if self.in_terminal(*start_x, *start_y) {
                    self.selecting = true;
                    self.context_menu = None;
                    let _ = self.actions.send(UiAction::SelectStart {
                        x: *start_x,
                        y: *start_y,
                    });
                    return true;
                }
                self.selecting = false;
                return false;
            }
            RawCommand::DragMove { x, y, .. } if self.selecting => {
                let _ = self.actions.send(UiAction::SelectUpdate { x: *x, y: *y });
                return true;
            }
            RawCommand::DragEnd { x, y, .. } if self.selecting => {
                self.selecting = false;
                let _ = self.actions.send(UiAction::SelectUpdate { x: *x, y: *y });
                return true;
            }
            _ => {}
        }

        // 标签右键：弹出「复制会话 / 关闭标签页」。
        let tab_count = self
            .shared
            .lock()
            .map(|shared| shared.tabs.len())
            .unwrap_or(0);
        if let RawCommand::ContextClick { x, y, .. } = command {
            let key = command.node_key();
            let hit = (0..tab_count).find(|index| {
                let select = TAB_ENTRY_BASE + *index as LocalKey * 2;
                key == Some(child_key(owner, select)) || key == Some(child_key(owner, select + 1))
            });
            if let Some(index) = hit {
                self.tab_menu = Some((index, *x, *y));
                self.context_menu = None;
                self.open_menu = None;
                self.shell_open = false;
                self.auth_open = false;
                ctx.mark_dirty();
                return true;
            }

            // 快速连接会话行右键：选中并弹出「连接 / 编辑 / 复制 / 删除」。
            let quick_open = self.shared.lock().is_ok_and(|shared| shared.quick_open);
            let quick_hit = if quick_open {
                (0..self.config.sessions.len()).find(|index| {
                    key == Some(child_key(owner, QUICK_ENTRY_BASE + *index as LocalKey))
                })
            } else {
                None
            };
            if let Some(index) = quick_hit {
                self.quick_selected = Some(index);
                self.quick_menu = Some((index, *x, *y));
                self.quick_last_click = None;
                self.tab_menu = None;
                self.context_menu = None;
                self.open_menu = None;
                self.shell_open = false;
                self.auth_open = false;
                ctx.mark_dirty();
                return true;
            }
        }
        if self.tab_menu.is_some()
            && let Some(item) = Menu::selection(child_key(owner, TAB_MENU_SLOT), 2, command)
            && let Some((index, _, _)) = self.tab_menu.take()
        {
            let action = if item == 0 {
                UiAction::DuplicateTab(index)
            } else {
                UiAction::CloseTab(index)
            };
            let _ = self.actions.send(action);
            ctx.mark_dirty();
            return true;
        }
        if self.quick_menu.is_some()
            && let Some(item) = Menu::selection(child_key(owner, QUICK_MENU_SLOT), 4, command)
            && let Some((index, _, _)) = self.quick_menu.take()
        {
            match item {
                0 => {
                    self.quick_connect(index, ctx);
                }
                1 => self.quick_edit(index),
                2 => self.quick_duplicate(index),
                _ => self.quick_delete(index),
            }
            ctx.mark_dirty();
            return true;
        }

        // 终端右键：在指针位置弹出复制 / 粘贴 / 全选。
        if let RawCommand::ContextClick { x, y, .. } = command
            && self.in_terminal(*x, *y)
        {
            self.context_menu = Some((*x, *y));
            self.open_menu = None;
            self.shell_open = false;
            ctx.mark_dirty();
            return true;
        }
        if self.context_menu.is_some()
            && let Some(item) = Menu::selection(child_key(owner, CONTEXT_SLOT), 3, command)
        {
            self.context_menu = None;
            let action = match item {
                0 => UiAction::Copy,
                1 => UiAction::Paste,
                _ => UiAction::SelectAll,
            };
            let _ = self.actions.send(action);
            ctx.mark_dirty();
            return true;
        }

        // 终端回滚：滚轮不依赖点击，先于 Click 判断。
        if let RawCommand::Wheel { delta, .. } = command {
            let _ = self.actions.send(UiAction::Scroll(*delta));
            return true;
        }

        let RawCommand::Click { .. } = command else {
            return false;
        };

        // 标签栏：具名槽位解析；关闭按钮与选中互不干扰。
        for index in 0..tab_count {
            let select = TAB_ENTRY_BASE + index as LocalKey * 2;
            if command.node_key() == Some(child_key(owner, select)) {
                let _ = self.actions.send(UiAction::SelectTab(index));
                ctx.mark_dirty();
                return true;
            }
            if command.node_key() == Some(child_key(owner, select + 1)) {
                let _ = self.actions.send(UiAction::CloseTab(index));
                ctx.mark_dirty();
                return true;
            }
        }

        // SFTP 目录项：具名槽位不落在 Act 枚举里，按索引自己解析。
        let sftp_click = {
            let shared = self.shared.lock().expect("shared lock");
            if shared.sftp.open && shared.screen == Screen::Session {
                command.node_key().and_then(|key| {
                    shared
                        .sftp
                        .entries
                        .iter()
                        .enumerate()
                        .find(|(index, _)| sftp_entry_key(owner, *index) == key)
                        .map(|(index, entry)| (index, entry.clone(), shared.sftp.path.clone()))
                })
            } else {
                None
            }
        };
        if let Some((index, entry, path)) = sftp_click {
            let action = if index == 0 {
                Some(sftp::SftpAction::List(sftp::parent_path(&path)))
            } else if entry.is_dir {
                Some(sftp::SftpAction::List(sftp::join_remote(
                    &path,
                    &entry.name,
                )))
            } else {
                if let Ok(mut shared) = self.shared.lock() {
                    shared.sftp.selected = Some(index);
                    if shared.sftp.local_path.is_empty() {
                        shared.sftp.local_path = default_local_path(&entry.name);
                    }
                }
                None
            };
            if let Some(action) = action {
                let _ = self.actions.send(UiAction::SftpAction(action));
            }
            ctx.mark_dirty();
            return true;
        }

        // 管理浮层里的会话 / 宏列表行：同样用具名槽位按索引解析。
        if (self.sessions_open || self.macros_open)
            && let Some(key) = command.node_key()
        {
            if self.sessions_open
                && let Some(index) = (0..self.config.sessions.len())
                    .find(|index| session_entry_key(owner, *index) == key)
            {
                self.session_selected = Some(index);
                self.session_name = self.config.sessions[index].name.clone();
                self.manager_notice.clear();
                ctx.mark_dirty();
                return true;
            }
            if self.macros_open
                && let Some(index) = (0..self.config.macros.len())
                    .find(|index| macro_entry_key(owner, *index) == key)
            {
                let entry = self.config.macros[index].clone();
                self.macro_selected = Some(index);
                self.macro_name = entry.name;
                self.macro_text = entry.text;
                self.macro_enter = entry.enter;
                self.manager_notice.clear();
                ctx.mark_dirty();
                return true;
            }
        }

        // 快速连接会话行：单击选中，双击才连接。
        let quick_open = self.shared.lock().is_ok_and(|shared| shared.quick_open);
        if quick_open && let Some(key) = command.node_key() {
            let hit = (0..self.config.sessions.len())
                .find(|index| key == child_key(owner, QUICK_ENTRY_BASE + *index as LocalKey));
            if let Some(index) = hit {
                let double = self.quick_last_click.is_some_and(|(last, at)| {
                    last == index && at.elapsed() < Duration::from_millis(400)
                });
                self.quick_selected = Some(index);
                self.quick_last_click = (!double).then(|| (index, Instant::now()));
                if double {
                    self.quick_connect(index, ctx);
                }
                ctx.mark_dirty();
                return true;
            }
        }

        // 3. 菜单栏选项（先于标题判断：菜单打开时点击的是行）。
        if let Some(index) = self.open_menu {
            let (_, items) = MENUS[index];
            if let Some(item) = Menu::selection(
                child_key(owner, MENU_POPUP_BASE + index as LocalKey),
                items.len(),
                command,
            ) {
                self.open_menu = None;
                self.on_menu(index, item, ctx);
                return true;
            }
        }

        // 4. shell / 认证方式下拉选项。
        if self.shell_open
            && let Some(index) = Menu::selection(
                child_key(owner, SHELL_POPUP_SLOT),
                self.shells.len(),
                command,
            )
        {
            self.shell = index;
            self.shell_open = false;
            ctx.mark_dirty();
            return true;
        }
        if self.auth_open
            && let Some(index) = Menu::selection(
                child_key(owner, AUTH_POPUP_SLOT),
                AUTH_OPTIONS.len(),
                command,
            )
        {
            self.auth = index;
            self.auth_open = false;
            ctx.mark_dirty();
            return true;
        }

        // 5. 菜单栏标题。
        match command.key::<Act>(owner) {
            Some(Act::MenuFile) => {
                self.open_menu = (self.open_menu != Some(0)).then_some(0);
                self.shell_open = false;
                ctx.mark_dirty();
                return true;
            }
            Some(Act::MenuSession) => {
                self.open_menu = (self.open_menu != Some(1)).then_some(1);
                self.shell_open = false;
                ctx.mark_dirty();
                return true;
            }
            Some(Act::MenuHelp) => {
                self.open_menu = (self.open_menu != Some(2)).then_some(2);
                self.shell_open = false;
                ctx.mark_dirty();
                return true;
            }
            _ => {}
        }

        // 6. 对话框：选项卡 / 认证方式 / shell 按钮 / 连接 / 取消。
        if let Some(index) = Tabs::selection(child_key(owner, Act::Tabs.local()), 2, command) {
            self.tab = index;
            self.shell_open = false;
            self.auth_open = false;
            ctx.mark_dirty();
            return true;
        }
        match command.key::<Act>(owner) {
            Some(Act::Shell) => {
                self.shell_open = !self.shell_open;
                self.auth_open = false;
                self.open_menu = None;
                ctx.mark_dirty();
                return true;
            }
            // 认证方式用下拉选择，和 shell 选择一样由外层声明浮层。
            Some(Act::AuthMode) => {
                self.auth_open = !self.auth_open;
                self.shell_open = false;
                self.open_menu = None;
                ctx.mark_dirty();
                return true;
            }
            Some(Act::Connect) => {
                self.connect(ctx);
                return true;
            }
            Some(Act::Cancel) => {
                self.set_dialog(false);
                self.shell_open = false;
                self.auth_open = false;
                ctx.mark_dirty();
                return true;
            }
            // 标签栏右侧的「+」：打开连接对话框，建新标签。
            Some(Act::NewTab) => {
                self.set_dialog(true);
                self.shell_open = false;
                self.auth_open = false;
                self.open_menu = None;
                ctx.mark_dirty();
                return true;
            }
            Some(Act::SftpUp) => {
                let path = self
                    .shared
                    .lock()
                    .map(|shared| shared.sftp.path.clone())
                    .unwrap_or_default();
                let _ = self
                    .actions
                    .send(UiAction::SftpAction(sftp::SftpAction::List(
                        sftp::parent_path(&path),
                    )));
                return true;
            }
            Some(Act::SftpRefresh) | Some(Act::SftpEnter) => {
                let path = self
                    .shared
                    .lock()
                    .map(|shared| shared.sftp.path.clone())
                    .unwrap_or_default();
                let _ = self
                    .actions
                    .send(UiAction::SftpAction(sftp::SftpAction::List(path)));
                return true;
            }
            Some(Act::SftpUpload) => {
                let (local, path) = {
                    let shared = self.shared.lock().expect("shared lock");
                    (shared.sftp.local_path.clone(), shared.sftp.path.clone())
                };
                if !local.is_empty() {
                    let name = std::path::Path::new(&local)
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "upload".to_owned());
                    let _ = self
                        .actions
                        .send(UiAction::SftpAction(sftp::SftpAction::Upload {
                            local,
                            remote: sftp::join_remote(&path, &name),
                        }));
                }
                return true;
            }
            Some(Act::SftpDownload) => {
                let (remote, local) = {
                    let mut shared = self.shared.lock().expect("shared lock");
                    let Some(index) = shared.sftp.selected else {
                        return true;
                    };
                    let Some(entry) = shared.sftp.entries.get(index).cloned() else {
                        return true;
                    };
                    if entry.is_dir {
                        return true;
                    }
                    let remote = sftp::join_remote(&shared.sftp.path, &entry.name);
                    if shared.sftp.local_path.is_empty() {
                        shared.sftp.local_path = default_local_path(&entry.name);
                    }
                    (remote, shared.sftp.local_path.clone())
                };
                let _ = self
                    .actions
                    .send(UiAction::SftpAction(sftp::SftpAction::Download {
                        remote,
                        local,
                    }));
                return true;
            }
            Some(Act::SftpCancel) => {
                let _ = self
                    .actions
                    .send(UiAction::SftpAction(sftp::SftpAction::Cancel));
                return true;
            }
            Some(Act::SftpClose) => {
                if let Ok(mut shared) = self.shared.lock() {
                    shared.sftp.open = false;
                    shared.sftp.notice.clear();
                    shared.sftp.progress = None;
                }
                let _ = self.actions.send(UiAction::SftpClose);
                ctx.mark_dirty();
                return true;
            }
            // —— 会话管理 ——
            Some(Act::SessionConnect) => {
                let Some(index) = self.session_selected else {
                    self.manager_notice = "请先在列表中选择一个会话".to_owned();
                    ctx.mark_dirty();
                    return true;
                };
                let Some(profile) = self.config.sessions.get(index).cloned() else {
                    return true;
                };
                self.host = profile.host;
                self.port = profile.port.to_string();
                self.user = profile.user;
                self.identity = profile.identity;
                // 密码不落盘：载入后必须重新输入；有密钥路径时默认切到密钥模式。
                self.password.clear();
                self.auth = if self.identity.trim().is_empty() {
                    0
                } else {
                    1
                };
                self.session_name = profile.name;
                self.tab = 1;
                self.shell_open = false;
                self.sessions_open = false;
                self.manager_notice.clear();
                self.set_dialog(true);
                ctx.mark_dirty();
                return true;
            }
            Some(Act::SessionSave) => {
                let name = self.session_name.trim().to_owned();
                if name.is_empty() {
                    self.manager_notice = "请先填写会话名称".to_owned();
                } else {
                    self.config.upsert_session(SessionProfile {
                        name: name.clone(),
                        host: self.host.trim().to_owned(),
                        port: self.port.trim().parse().unwrap_or(22),
                        user: self.user.trim().to_owned(),
                        // 密码模式的密钥字段不落盘，避免载入后被误当成密钥登录。
                        identity: if self.auth == 1 {
                            self.identity.trim().to_owned()
                        } else {
                            String::new()
                        },
                    });
                    self.session_selected = self
                        .config
                        .sessions
                        .iter()
                        .position(|session| session.name == name);
                    self.manager_notice = match self.save_config() {
                        Ok(()) => format!("已保存会话 {name}"),
                        Err(err) => err,
                    };
                }
                ctx.mark_dirty();
                return true;
            }
            Some(Act::SessionDelete) => {
                let Some(index) = self.session_selected else {
                    self.manager_notice = "请先在列表中选择一个会话".to_owned();
                    ctx.mark_dirty();
                    return true;
                };
                let name = self
                    .config
                    .sessions
                    .get(index)
                    .map(|session| session.name.clone())
                    .unwrap_or_default();
                if self.config.remove_session(index) {
                    self.session_selected = (!self.config.sessions.is_empty())
                        .then(|| index.min(self.config.sessions.len() - 1));
                    self.session_name.clear();
                    self.manager_notice = match self.save_config() {
                        Ok(()) => format!("已删除会话 {name}"),
                        Err(err) => err,
                    };
                }
                ctx.mark_dirty();
                return true;
            }
            Some(Act::SessionClose) => {
                self.sessions_open = false;
                self.manager_notice.clear();
                ctx.mark_dirty();
                return true;
            }
            // —— 宏管理 ——
            Some(Act::MacroEnter) => {
                self.macro_enter = !self.macro_enter;
                ctx.mark_dirty();
                return true;
            }
            Some(Act::MacroNew) => {
                self.macro_selected = None;
                self.macro_name.clear();
                self.macro_text.clear();
                self.macro_enter = false;
                self.manager_notice = "填写名称和内容后点保存".to_owned();
                ctx.mark_dirty();
                return true;
            }
            Some(Act::MacroSave) => {
                let name = self.macro_name.trim().to_owned();
                if name.is_empty() {
                    self.manager_notice = "请先填写宏名称".to_owned();
                } else {
                    let entry = Macro {
                        name: name.clone(),
                        text: self.macro_text.clone(),
                        enter: self.macro_enter,
                    };
                    if let Some(index) = self.macro_selected {
                        self.config.replace_macro(index, entry);
                    } else {
                        self.config.upsert_macro(entry);
                    }
                    self.macro_selected = self
                        .config
                        .macros
                        .iter()
                        .position(|macros| macros.name == name);
                    self.manager_notice = match self.save_config() {
                        Ok(()) => format!("已保存宏 {name}"),
                        Err(err) => err,
                    };
                }
                ctx.mark_dirty();
                return true;
            }
            Some(Act::MacroRun) => {
                if self.macro_text.is_empty() {
                    self.manager_notice = "宏内容为空".to_owned();
                    ctx.mark_dirty();
                    return true;
                }
                let (screen, input) = {
                    let shared = self.shared.lock().expect("shared lock");
                    (shared.screen, Arc::clone(&shared.input))
                };
                if screen != Screen::Session {
                    self.manager_notice = "宏只能在已连接的会话中运行".to_owned();
                    ctx.mark_dirty();
                    return true;
                }
                let bytes = macro_bytes(&self.macro_text, self.macro_enter);
                let sent = input.lock().is_ok_and(|slot| {
                    slot.as_ref()
                        .is_some_and(|sender| sender.send(UiEvent::Input(bytes)).is_ok())
                });
                if sent {
                    self.macros_open = false;
                    self.manager_notice.clear();
                } else {
                    self.manager_notice = "发送失败：当前没有输入通道".to_owned();
                }
                ctx.mark_dirty();
                return true;
            }
            Some(Act::MacroDelete) => {
                let Some(index) = self.macro_selected else {
                    self.manager_notice = "请先在列表中选择一个宏".to_owned();
                    ctx.mark_dirty();
                    return true;
                };
                let name = self
                    .config
                    .macros
                    .get(index)
                    .map(|entry| entry.name.clone())
                    .unwrap_or_default();
                if self.config.remove_macro(index) {
                    self.macro_selected = (!self.config.macros.is_empty())
                        .then(|| index.min(self.config.macros.len() - 1));
                    self.macro_name.clear();
                    self.macro_text.clear();
                    self.macro_enter = false;
                    self.manager_notice = match self.save_config() {
                        Ok(()) => format!("已删除宏 {name}"),
                        Err(err) => err,
                    };
                }
                ctx.mark_dirty();
                return true;
            }
            Some(Act::MacroClose) => {
                self.macros_open = false;
                self.manager_notice.clear();
                ctx.mark_dirty();
                return true;
            }
            // —— 快速连接侧边栏 ——
            Some(Act::QuickNew) => {
                self.quick_editing = true;
                self.quick_edit_index = None;
                self.host.clear();
                self.port = "22".to_owned();
                self.user.clear();
                self.identity.clear();
                self.password.clear();
                self.session_name.clear();
                self.quick_notice = "填写后点「保存」".to_owned();
                ctx.mark_dirty();
                return true;
            }
            Some(Act::QuickSave) => {
                let name = self.session_name.trim().to_owned();
                if name.is_empty() || self.host.trim().is_empty() {
                    self.quick_notice = "名称和主机不能为空".to_owned();
                } else {
                    let profile = SessionProfile {
                        name: name.clone(),
                        host: self.host.trim().to_owned(),
                        port: self.port.trim().parse().unwrap_or(22),
                        user: self.user.trim().to_owned(),
                        identity: self.identity.trim().to_owned(),
                    };
                    if let Some(index) = self.quick_edit_index
                        && let Some(slot) = self.config.sessions.get_mut(index)
                    {
                        *slot = profile;
                    } else {
                        self.config.upsert_session(profile);
                    }
                    self.quick_editing = false;
                    self.quick_edit_index = None;
                    self.quick_notice = match self.save_config() {
                        Ok(()) => format!("已保存连接 {name}"),
                        Err(err) => err,
                    };
                }
                ctx.mark_dirty();
                return true;
            }
            Some(Act::QuickCancel) => {
                self.quick_editing = false;
                self.quick_edit_index = None;
                self.quick_notice.clear();
                ctx.mark_dirty();
                return true;
            }
            // —— ZMODEM 上传路径 ——
            Some(Act::UploadAccept) => {
                let path = self
                    .shared
                    .lock()
                    .map(|shared| shared.upload_path.trim().to_owned())
                    .unwrap_or_default();
                if !path.is_empty() {
                    let _ = self.actions.send(UiAction::UploadAccept { path });
                }
                ctx.mark_dirty();
                return true;
            }
            Some(Act::UploadCancel) => {
                let _ = self.actions.send(UiAction::UploadCancel);
                ctx.mark_dirty();
                return true;
            }
            _ => {}
        }

        // 终端里单击：清掉上一次的选择（焦点仍交给 KeyCapture）。
        //
        // 运行时产出的 `Click` 一定带 `key: Some(..)`（命中节点的 key），不能靠
        // `key: None` 识别终端。这里已经走到所有具名控件之后，剩下的点击只可能
        // 落在终端画布或空白区，清选择即可。
        let in_session = self
            .shared
            .lock()
            .is_ok_and(|shared| shared.screen == Screen::Session);
        if in_session && !self.sessions_open && !self.macros_open {
            let _ = self.actions.send(UiAction::SelectClear);
        }

        false
    }

    /// 快速连接侧边栏：会话列表一键连接，并提供内联的新建 / 编辑 / 删除。
    fn quick_connect_view(&self, theme: &UiTheme, height: f64) -> ViewNode {
        let mut children: Vec<ViewNode> = vec![
            Row::new()
                .justify(JustifyContent::SpaceBetween)
                .view(theme)
                .with_children([
                    Label::new("快速连接").size(14.0).view(theme),
                    Button::new("新建").with_key(Act::QuickNew).view(theme),
                ]),
        ];

        if self.quick_editing {
            children.push(self.quick_editor(theme));
        }

        let mut rows = Vec::new();
        for (index, profile) in self.config.sessions.iter().enumerate().take(100) {
            rows.push(self.quick_row(theme, index, profile));
        }
        if rows.is_empty() {
            rows.push(self.manager_hint(theme, "还没有保存的连接，点「新建」添加"));
        }
        children.push(Scroll::new().grow(1.0).view(theme).with_children(rows));

        let hint = if self.quick_notice.is_empty() {
            "单击选择 · 双击连接 · 右键操作".to_owned()
        } else {
            self.quick_notice.clone()
        };
        children.push(
            Label::new(hint)
                .size(12.0)
                .color(theme.text(TextToken::Secondary))
                .view(theme),
        );

        Column::new()
            .width(QUICK_WIDTH)
            .height(height)
            .background(theme.bg(BgToken::Container))
            .padding(8.0)
            .gap(6.0)
            .view(theme)
            .with_children(children)
    }

    /// 侧边栏里的内联表单：名称 / 主机 / 端口 / 用户 / 密钥。
    fn quick_editor(&self, theme: &UiTheme) -> ViewNode {
        let width = QUICK_WIDTH - 16.0 - 36.0 - 6.0;
        let fields = [
            ("名称", &self.session_name, Act::QuickName),
            ("主机", &self.host, Act::QuickHost),
            ("端口", &self.port, Act::QuickPort),
            ("用户", &self.user, Act::QuickUser),
            ("密钥", &self.identity, Act::QuickIdentity),
        ];
        let mut rows = Vec::new();
        for (label, value, act) in fields {
            rows.push(Self::quick_field(theme, label, value, act, width));
        }
        rows.push(Row::new().gap(6.0).view(theme).with_children([
            Button::new("保存").with_key(Act::QuickSave).view(theme),
            Button::new("取消").with_key(Act::QuickCancel).view(theme),
        ]));
        Column::new().gap(6.0).view(theme).with_children(rows)
    }

    fn quick_field(theme: &UiTheme, label: &str, value: &str, act: Act, width: f64) -> ViewNode {
        let mut style = Style::default();
        style.layout.direction = FlexDirection::Row;
        style.layout.align_items = AlignItems::Center;
        style.layout.width = Length::px(36.0);
        let caption = ViewNode::new(Decl::div(ROOT_KEY).with_style(style)).child(passthrough(
            Label::new(label)
                .size(12.0)
                .color(theme.text(TextToken::Secondary))
                .view(theme),
        ));
        Row::new().gap(6.0).view(theme).with_children([
            caption,
            TextField::new(value.to_owned())
                .width(width)
                .with_key(act)
                .view(theme),
        ])
    }

    /// 一行会话：单击选中、双击连接，右键弹出「连接 / 编辑 / 复制 / 删除」。
    fn quick_row(&self, theme: &UiTheme, index: usize, profile: &SessionProfile) -> ViewNode {
        let slot = QUICK_ENTRY_BASE + index as LocalKey;
        let selected = self.quick_selected == Some(index);
        let target = if profile.user.is_empty() {
            format!("{}:{}", profile.host, profile.port)
        } else {
            format!("{}@{}:{}", profile.user, profile.host, profile.port)
        };

        let mut style = Style::default();
        style.layout.direction = FlexDirection::Column;
        style.layout.justify_content = JustifyContent::Center;
        style.layout.height = Length::px(38.0);
        style.layout.padding = Edges::new(0.0, 8.0, 0.0, 8.0);
        style.visual.cursor = Cursor::Pointer;
        style.visual.corner_radius = Corners::all(theme.radius(RadiusToken::Small));
        if selected {
            style.visual.background = theme.bg(BgToken::ContainerHover);
        } else {
            style.visual = style.visual.with_override(
                InteractionState::Hovered,
                VisualOverrides {
                    background: Some(theme.bg(BgToken::ContainerHover)),
                    ..VisualOverrides::default()
                },
            );
        }

        let name_color = if selected {
            theme.brand(Shade::Base)
        } else {
            theme.text(TextToken::Primary)
        };
        ViewNode::new(Decl::div(child_key(ROOT_KEY, slot)).with_style(style))
            .with_slot(Some(slot))
            .with_children([
                passthrough(
                    Label::new(profile.name.clone())
                        .color(name_color)
                        .view(theme),
                ),
                passthrough(
                    Label::new(target)
                        .size(11.0)
                        .color(theme.text(TextToken::Secondary))
                        .view(theme),
                ),
            ])
    }

    /// 从一个保存的会话直接发起连接；认证失败时对话框会带着已填好的表单重开。
    fn quick_connect(&mut self, index: usize, ctx: &mut UpdateCtx) -> bool {
        let Some(profile) = self.config.sessions.get(index).cloned() else {
            return false;
        };
        self.host = profile.host;
        self.port = profile.port.to_string();
        self.user = profile.user;
        self.identity = profile.identity;
        self.password.clear();
        self.auth = if self.identity.trim().is_empty() {
            0
        } else {
            1
        };
        self.session_name = profile.name;
        self.tab = 1;
        self.connect(ctx);
        true
    }

    /// 把已保存会话载入内联编辑器。
    fn quick_edit(&mut self, index: usize) {
        let Some(profile) = self.config.sessions.get(index).cloned() else {
            return;
        };
        self.quick_editing = true;
        self.quick_edit_index = Some(index);
        self.host = profile.host;
        self.port = profile.port.to_string();
        self.user = profile.user;
        self.identity = profile.identity;
        self.session_name = profile.name;
        self.quick_notice.clear();
    }

    /// 复制已保存会话（自动生成不重名的「副本」）。
    fn quick_duplicate(&mut self, index: usize) {
        let Some(profile) = self.config.sessions.get(index).cloned() else {
            return;
        };
        let name = self.unique_session_name(&profile.name);
        self.config.upsert_session(SessionProfile {
            name: name.clone(),
            ..profile
        });
        self.quick_selected = self.config.sessions.iter().position(|s| s.name == name);
        self.quick_last_click = None;
        self.quick_notice = match self.save_config() {
            Ok(()) => format!("已复制连接 {name}"),
            Err(err) => err,
        };
    }

    fn unique_session_name(&self, base: &str) -> String {
        let mut name = format!("{base} 副本");
        let mut n = 2;
        while self.config.sessions.iter().any(|s| s.name == name) {
            name = format!("{base} 副本{n}");
            n += 1;
        }
        name
    }

    /// 删除已保存会话，并修正选中 / 编辑下标。
    fn quick_delete(&mut self, index: usize) {
        if index >= self.config.sessions.len() {
            return;
        }
        let name = self.config.sessions[index].name.clone();
        self.config.remove_session(index);
        if self.quick_edit_index == Some(index) {
            self.quick_editing = false;
            self.quick_edit_index = None;
        } else if let Some(editing) = self.quick_edit_index
            && editing > index
        {
            self.quick_edit_index = Some(editing - 1);
        }
        match self.quick_selected {
            Some(sel) if sel == index => self.quick_selected = None,
            Some(sel) if sel > index => self.quick_selected = Some(sel - 1),
            _ => {}
        }
        self.quick_last_click = None;
        self.quick_notice = match self.save_config() {
            Ok(()) => format!("已删除连接 {name}"),
            Err(err) => err,
        };
    }

    /// SFTP 侧边栏：远端路径 + 目录列表 + 本地文件 + 上传/下载按钮。
    fn sftp_sidebar_view(&self, theme: &UiTheme, height: f64, sftp: &SftpState) -> ViewNode {
        let mut children: Vec<ViewNode> = vec![
            Label::new("SFTP").size(14.0).view(theme),
            TextField::new(sftp.path.clone())
                .width(SIDEBAR_WIDTH - 24.0)
                .with_key(Act::SftpPath)
                .view(theme),
            Row::new().gap(6.0).view(theme).with_children([
                Button::new("↑ 上级").with_key(Act::SftpUp).view(theme),
                Button::new("刷新").with_key(Act::SftpRefresh).view(theme),
                Button::new("进入").with_key(Act::SftpEnter).view(theme),
            ]),
        ];

        let mut rows = Vec::new();
        for (index, entry) in sftp.entries.iter().enumerate().take(100) {
            let selected = sftp.selected == Some(index);
            let mut style = Style::default();
            style.layout.direction = FlexDirection::Row;
            style.layout.align_items = AlignItems::Center;
            style.layout.height = Length::px(22.0);
            style.layout.padding = Edges::new(0.0, 6.0, 0.0, 6.0);
            style.visual.cursor = Cursor::Pointer;
            style.visual.background = if selected {
                theme.brand(Shade::Base)
            } else {
                Color::TRANSPARENT
            };
            if selected {
                style.visual.color = Color::WHITE;
            } else if !entry.is_dir {
                style.visual = style.visual.with_override(
                    InteractionState::Hovered,
                    VisualOverrides {
                        background: Some(theme.bg(BgToken::ContainerHover)),
                        ..VisualOverrides::default()
                    },
                );
            }
            let suffix = if entry.is_dir {
                "/".to_owned()
            } else {
                format!("  {}", entry.size)
            };
            let text = if index == 0 {
                "..".to_owned()
            } else {
                format!("{}{}", entry.name, suffix)
            };
            rows.push(
                ViewNode::new(
                    Decl::div(child_key(ROOT_KEY, SFTP_ENTRY_BASE + index as LocalKey))
                        .with_style(style),
                )
                .with_slot(Some(SFTP_ENTRY_BASE + index as LocalKey))
                .child(passthrough(Label::new(text).size(12.0).view(theme))),
            );
        }
        children.push(Scroll::new().grow(1.0).view(theme).with_children(rows));

        children.push(
            TextField::new(sftp.local_path.clone())
                .width(SIDEBAR_WIDTH - 24.0)
                .with_key(Act::SftpLocal)
                .view(theme),
        );
        if let Some(progress) = &sftp.progress {
            let text = if progress.total > 0 {
                format!(
                    "传输中 {} / {}",
                    format_bytes(progress.done),
                    format_bytes(progress.total)
                )
            } else {
                format!("传输中 {}", format_bytes(progress.done))
            };
            children.push(
                Label::new(text)
                    .size(12.0)
                    .color(theme.text(TextToken::Secondary))
                    .view(theme),
            );
        }
        let mut buttons = vec![
            Button::new("上传").with_key(Act::SftpUpload).view(theme),
            Button::new("下载").with_key(Act::SftpDownload).view(theme),
        ];
        if sftp.progress.is_some() {
            buttons.push(Button::new("取消").with_key(Act::SftpCancel).view(theme));
        }
        buttons.push(Button::new("关闭").with_key(Act::SftpClose).view(theme));
        children.push(Row::new().gap(6.0).view(theme).with_children(buttons));
        if !sftp.notice.is_empty() {
            children.push(
                Label::new(sftp.notice.clone())
                    .size(12.0)
                    .color(theme.text(TextToken::Secondary))
                    .view(theme),
            );
        }

        Column::new()
            .width(SIDEBAR_WIDTH)
            .height(height)
            .background(theme.bg(BgToken::Container))
            .padding(8.0)
            .gap(6.0)
            .view(theme)
            .with_children(children)
    }

    /// 已保存会话管理浮层：列表选择 + 名称 + 连接 / 保存当前 / 删除 / 关闭。
    fn session_manager(&self, owner: NodeKey, theme: &UiTheme) -> OverlayView {
        let mut rows = Vec::new();
        for (index, profile) in self.config.sessions.iter().enumerate().take(100) {
            let target = if profile.user.is_empty() {
                format!("{}  {}:{}", profile.name, profile.host, profile.port)
            } else {
                format!(
                    "{}  {}@{}:{}",
                    profile.name, profile.user, profile.host, profile.port
                )
            };
            rows.push(self.manager_row(
                theme,
                SESSION_ENTRY_BASE + index as LocalKey,
                self.session_selected == Some(index),
                target,
            ));
        }
        if rows.is_empty() {
            rows.push(self.manager_hint(theme, "还没有保存的会话"));
        }

        let content = Column::new().gap(10.0).view(theme).with_children([
            Row::new().gap(8.0).view(theme).with_children([
                Label::new("名称").view(theme),
                TextField::new(self.session_name.clone())
                    .width(340.0)
                    .with_key(Act::SessionName)
                    .view(theme),
            ]),
            Scroll::new().height(200.0).view(theme).with_children(rows),
            Label::new("保存当前：把上方 SSH 表单里的主机 / 端口 / 用户 / 密钥写进配置")
                .size(12.0)
                .color(theme.text(TextToken::Secondary))
                .view(theme),
            self.manager_notice_view(theme),
        ]);

        Dialog::new("已保存会话")
            .key(child_key(owner, SESSIONS_SLOT))
            .width(520.0)
            .content(content)
            .actions([
                Button::new("连接")
                    .with_key(Act::SessionConnect)
                    .view(theme),
                Button::new("保存当前")
                    .with_key(Act::SessionSave)
                    .view(theme),
                Button::new("删除选中")
                    .with_key(Act::SessionDelete)
                    .view(theme),
                Button::new("关闭").with_key(Act::SessionClose).view(theme),
            ])
            .build(owner, theme)
    }

    /// 宏管理浮层：列表选择 + 名称 / 内容 / 回车开关 + 新建 / 保存 / 运行 / 删除 / 关闭。
    fn macro_manager(&self, owner: NodeKey, theme: &UiTheme) -> OverlayView {
        let mut rows = Vec::new();
        for (index, entry) in self.config.macros.iter().enumerate().take(100) {
            let inline = entry.text.replace(['\n', '\r'], " ");
            let preview: String = inline.chars().take(36).collect();
            let suffix = if entry.enter { "  ↵" } else { "" };
            rows.push(self.manager_row(
                theme,
                MACRO_ENTRY_BASE + index as LocalKey,
                self.macro_selected == Some(index),
                format!("{}  {}{}", entry.name, preview, suffix),
            ));
        }
        if rows.is_empty() {
            rows.push(self.manager_hint(theme, "还没有宏，点「新建」开始"));
        }

        let content = Column::new().gap(10.0).view(theme).with_children([
            Row::new().gap(8.0).view(theme).with_children([
                Label::new("名称").view(theme),
                TextField::new(self.macro_name.clone())
                    .width(340.0)
                    .with_key(Act::MacroName)
                    .view(theme),
            ]),
            Row::new().gap(8.0).view(theme).with_children([
                Label::new("内容").view(theme),
                TextField::new(self.macro_text.clone())
                    .width(340.0)
                    .with_key(Act::MacroText)
                    .view(theme),
            ]),
            Row::new().gap(8.0).view(theme).with_children([
                Button::new(if self.macro_enter {
                    "自动回车：开"
                } else {
                    "自动回车：关"
                })
                .with_key(Act::MacroEnter)
                .view(theme),
                Label::new("支持 \\n \\r \\t \\e \\xNN 转义")
                    .size(12.0)
                    .color(theme.text(TextToken::Secondary))
                    .view(theme),
            ]),
            Scroll::new().height(180.0).view(theme).with_children(rows),
            self.manager_notice_view(theme),
        ]);

        Dialog::new("宏")
            .key(child_key(owner, MACROS_SLOT))
            .width(520.0)
            .content(content)
            .actions([
                Button::new("新建").with_key(Act::MacroNew).view(theme),
                Button::new("保存").with_key(Act::MacroSave).view(theme),
                Button::new("运行").with_key(Act::MacroRun).view(theme),
                Button::new("删除选中")
                    .with_key(Act::MacroDelete)
                    .view(theme),
                Button::new("关闭").with_key(Act::MacroClose).view(theme),
            ])
            .build(owner, theme)
    }

    /// 管理浮层里的一行（选中态 / 悬停态与 SFTP 目录项一致）。
    fn manager_row(
        &self,
        theme: &UiTheme,
        slot: LocalKey,
        selected: bool,
        text: String,
    ) -> ViewNode {
        let mut style = Style::default();
        style.layout.direction = FlexDirection::Row;
        style.layout.align_items = AlignItems::Center;
        style.layout.height = Length::px(22.0);
        style.layout.padding = Edges::new(0.0, 6.0, 0.0, 6.0);
        style.visual.cursor = Cursor::Pointer;
        style.visual.background = if selected {
            theme.brand(Shade::Base)
        } else {
            Color::TRANSPARENT
        };
        if selected {
            style.visual.color = Color::WHITE;
        } else {
            style.visual = style.visual.with_override(
                InteractionState::Hovered,
                VisualOverrides {
                    background: Some(theme.bg(BgToken::ContainerHover)),
                    ..VisualOverrides::default()
                },
            );
        }
        ViewNode::new(Decl::div(child_key(ROOT_KEY, slot)).with_style(style))
            .with_slot(Some(slot))
            .child(passthrough(Label::new(text).size(12.0).view(theme)))
    }

    fn manager_hint(&self, theme: &UiTheme, text: &str) -> ViewNode {
        Label::new(text)
            .size(12.0)
            .color(theme.text(TextToken::Secondary))
            .view(theme)
    }

    fn manager_notice_view(&self, theme: &UiTheme) -> ViewNode {
        Label::new(self.manager_notice.clone())
            .size(12.0)
            .color(theme.text(TextToken::Secondary))
            .view(theme)
    }

    /// 连接参数：终端与 SFTP 都用它，避免两边字段不一致。
    fn auth_options(&self) -> AuthOptions {
        let identity = self.identity.trim();
        AuthOptions {
            host: self.host.clone(),
            port: self.port.parse().unwrap_or(22),
            user: self.user.clone(),
            // 只提交当前单选模式对应的凭据，避免切换模式后仍用隐藏字段登录。
            password: if self.auth == 0 {
                self.password.clone()
            } else {
                String::new()
            },
            identity: (self.auth == 1 && !identity.is_empty()).then(|| identity.to_owned()),
            known_hosts: None,
        }
    }
    fn set_dialog(&self, open: bool) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.dialog_open = open;
        }
    }

    fn set_label(&self, text: impl Into<String>) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.label = text.into();
        }
    }

    /// 落盘管理浮层里的会话 / 宏，返回可直接展示的提示。
    fn save_config(&self) -> Result<(), String> {
        self.config
            .save(&self.config_path)
            .map_err(|err| format!("保存配置失败：{err}"))
    }

    fn on_menu(&mut self, menu: usize, item: usize, ctx: &mut UpdateCtx) {
        match (menu, item) {
            // 文件 > 新建连接
            (0, 0) => {
                self.set_dialog(true);
                ctx.mark_dirty();
            }
            // 文件 > 退出
            (0, 1) => {
                let _ = self.actions.send(UiAction::Quit);
            }
            // 会话 > 快速连接侧边栏
            (1, 0) => {
                let open = self
                    .shared
                    .lock()
                    .map(|shared| shared.quick_open)
                    .unwrap_or(false);
                if let Ok(mut shared) = self.shared.lock() {
                    shared.quick_open = !open;
                }
                self.quick_notice.clear();
                ctx.mark_dirty();
            }
            // 会话 > 清屏
            (1, 1) => {
                let _ = self.actions.send(UiAction::Clear);
            }
            // 会话 > 断开
            (1, 2) => {
                let _ = self.actions.send(UiAction::Disconnect);
            }
            // 会话 > SFTP 侧边栏
            (1, 3) => {
                let open = self
                    .shared
                    .lock()
                    .map(|shared| shared.sftp.open)
                    .unwrap_or(false);
                if open {
                    if let Ok(mut shared) = self.shared.lock() {
                        shared.sftp.open = false;
                        shared.sftp.notice.clear();
                    }
                    let _ = self.actions.send(UiAction::SftpClose);
                } else {
                    if let Ok(mut shared) = self.shared.lock() {
                        shared.sftp.open = true;
                        shared.sftp.notice = "正在连接 SFTP…".to_owned();
                    }
                    let _ = self.actions.send(UiAction::SftpOpen {
                        options: self.auth_options(),
                    });
                }
                ctx.mark_dirty();
            }
            // 帮助 > 关于
            (3, 0) => {
                self.set_label("lieterm 0.1 · zzzui 终端试作");
                ctx.mark_dirty();
            }
            // 终端 > 配色（深色 / 浅色 / Solarized）
            (2, 0..=2) => {
                let scheme = ["dark", "light", "solarized"][item];
                self.config.terminal.color_scheme = scheme.to_owned();
                self.set_label(
                    self.save_config()
                        .err()
                        .unwrap_or_else(|| format!("已切换配色：{scheme}")),
                );
                let _ = self.actions.send(UiAction::SetScheme(scheme.to_owned()));
                ctx.mark_dirty();
            }
            // 终端 > 编码（UTF-8 / GBK / GB18030）
            (2, 3..=5) => {
                let encoding = ["utf-8", "gbk", "gb18030"][item - 3];
                self.config.terminal.encoding = encoding.to_owned();
                self.set_label(
                    self.save_config()
                        .err()
                        .unwrap_or_else(|| format!("已切换编码：{encoding}")),
                );
                let _ = self
                    .actions
                    .send(UiAction::SetEncoding(encoding.to_owned()));
                ctx.mark_dirty();
            }
            // 会话 > 已保存会话…
            (1, 4) => {
                self.sessions_open = true;
                self.macros_open = false;
                self.shell_open = false;
                self.manager_notice.clear();
                ctx.mark_dirty();
            }
            // 会话 > 宏…
            (1, 5) => {
                self.macros_open = true;
                self.sessions_open = false;
                self.shell_open = false;
                self.manager_notice.clear();
                ctx.mark_dirty();
            }
            _ => {}
        }
    }

    fn connect(&mut self, ctx: &mut UpdateCtx) {
        self.shell_open = false;
        self.auth_open = false;
        self.open_menu = None;
        let mut shared = self.shared.lock().expect("shared lock");
        if self.tab == 0 {
            let index = self.shell.min(self.shells.len().saturating_sub(1));
            let shell = self.shells[index].1.clone();
            let _ = self.actions.send(UiAction::ConnectLocal { shell });
        } else {
            let options = ConnectOptions {
                host: self.host.clone(),
                port: self.port.parse().unwrap_or(22),
                user: self.user.clone(),
                password: self.password.clone(),
                identity: (!self.identity.trim().is_empty())
                    .then(|| self.identity.trim().to_owned()),
                known_hosts: None,
                cols: 80,
                rows: 24,
            };
            let _ = self.actions.send(UiAction::Connect { options });
        }
        shared.screen = Screen::Connecting;
        shared.dialog_open = false;
        shared.label = "正在连接…".to_owned();
        drop(shared);
        ctx.mark_dirty();
    }

    fn menubar_view(&self, theme: &UiTheme) -> ViewNode {
        let mut row = Style::default();
        row.layout.direction = FlexDirection::Row;
        row.layout.align_items = AlignItems::Center;
        row.layout.justify_content = JustifyContent::Start;
        row.layout.height = Length::px(MENUBAR_HEIGHT);
        row.layout.padding = Edges::symmetric(0.0, 6.0);
        row.layout.gap = Size::new(2.0, 2.0);
        row.layout.border = Edges::new(0.0, 0.0, 1.0, 0.0);
        // tdesign head menu：容器底色 + 级别 1 分隔线，跟随主题切换。
        row.visual.background = theme.bg(BgToken::Container);
        row.visual.border_color = theme.border(BorderToken::Level1);

        let mut bar = ViewNode::new(Decl::div(child_key(ROOT_KEY, MENUBAR_SLOT)).with_style(row));
        for (index, (label, _)) in MENUS.iter().enumerate() {
            bar = bar.child(self.menu_title(theme, index, label));
        }
        bar
    }

    /// 会话标签栏：自绘标签（选中 / 关闭具名槽位）+ 右侧新建按钮。
    fn tab_bar_view(&self, theme: &UiTheme, shared: &Shared) -> ViewNode {
        let plus = Button::new("+").with_key(Act::NewTab).view(theme);

        let mut row = Style::default();
        row.layout.direction = FlexDirection::Row;
        row.layout.align_items = AlignItems::Center;
        row.layout.height = Length::px(TAB_BAR_HEIGHT);
        row.layout.width = Length::px(shared.width.max(1.0));
        row.layout.gap = Size::new(2.0, 0.0);
        row.layout.border = Edges::new(0.0, 0.0, 1.0, 0.0);
        row.visual.border_color = theme.border(BorderToken::Level1);
        // 与标签自己的 Container 底色一致：标签右侧空白不被画成另一块颜色。
        row.visual.background = theme.bg(BgToken::Container);

        let mut children = Vec::new();
        for (index, tab) in shared.tabs.iter().enumerate() {
            children.push(self.tab_item(theme, index, &tab.title, index == shared.active_tab));
        }
        children.push(plus);

        ViewNode::new(Decl::div(child_key(ROOT_KEY, TAB_BAR_SLOT)).with_style(row))
            .with_children(children)
    }

    /// 单个标签：具名槽位（选中 / 关闭）使点击不依赖 zzzui 的行 key 推导。
    fn tab_item(&self, theme: &UiTheme, index: usize, title: &str, active: bool) -> ViewNode {
        let select_slot = TAB_ENTRY_BASE + index as LocalKey * 2;
        let close_slot = select_slot + 1;

        let mut tab = Style::default();
        tab.layout.direction = FlexDirection::Row;
        tab.layout.align_items = AlignItems::Center;
        tab.layout.height = Length::px(TAB_BAR_HEIGHT);
        tab.layout.padding = Edges::new(0.0, 8.0, 0.0, 12.0);
        tab.layout.gap = Size::new(4.0, 0.0);
        tab.visual.cursor = Cursor::Pointer;
        tab.visual = tab.visual.with_override(
            InteractionState::Hovered,
            VisualOverrides {
                background: Some(theme.bg(BgToken::ContainerHover)),
                ..VisualOverrides::default()
            },
        );

        let color = if active {
            theme.brand(Shade::Base)
        } else {
            theme.text(TextToken::Primary)
        };
        let select = passthrough(
            Label::new(title.to_owned())
                .size(13.0)
                .color(color)
                .view(theme),
        );

        let mut close_style = Style::default();
        close_style.layout.direction = FlexDirection::Row;
        close_style.layout.align_items = AlignItems::Center;
        close_style.layout.justify_content = JustifyContent::Center;
        close_style.layout.width = Length::px(16.0);
        close_style.layout.height = Length::px(16.0);
        close_style.visual.cursor = Cursor::Pointer;
        close_style.visual.corner_radius = Corners::all(theme.radius(RadiusToken::Small));
        close_style.visual = close_style.visual.with_override(
            InteractionState::Hovered,
            VisualOverrides {
                background: Some(theme.bg(BgToken::ContainerHover)),
                color: Some(theme.text(TextToken::Primary)),
                ..VisualOverrides::default()
            },
        );
        let close =
            ViewNode::new(Decl::div(child_key(ROOT_KEY, close_slot)).with_style(close_style))
                .with_slot(Some(close_slot))
                .child(passthrough(
                    Label::new("×")
                        .size(12.0)
                        .color(theme.text(TextToken::Secondary))
                        .view(theme),
                ));

        // 选中下划线：不参与命中，点击穿透到标签本体。
        let mut indicator = Style::default();
        indicator.layout.position = PositionType::Absolute;
        indicator.layout.inset = Edges::new(f64::NAN, 0.0, 0.0, 0.0);
        indicator.layout.height = Length::px(3.0);
        indicator.visual.background = if active {
            theme.brand(Shade::Base)
        } else {
            Color::TRANSPARENT
        };
        indicator.visual.pointer_events = PointerEvents::None;

        ViewNode::new(Decl::div(child_key(ROOT_KEY, select_slot)).with_style(tab))
            .with_slot(Some(select_slot))
            .with_children([
                select,
                close,
                ViewNode::new(Decl::div(ROOT_KEY).with_style(indicator)),
            ])
    }

    fn menu_title(&self, theme: &UiTheme, index: usize, label: &str) -> ViewNode {
        let act = match index {
            0 => Act::MenuFile,
            1 => Act::MenuSession,
            _ => Act::MenuHelp,
        };
        let active = self.open_menu == Some(index);
        let mut style = Style::default();
        style.layout.direction = FlexDirection::Row;
        style.layout.align_items = AlignItems::Center;
        style.layout.height = Length::px(22.0);
        style.layout.padding = Edges::symmetric(0.0, 10.0);
        style.visual.cursor = Cursor::Pointer;
        style.visual.corner_radius = Corners::all(theme.radius(RadiusToken::Small));
        // tdesign：默认正文色，展开时用品牌色；hover / 展开底色取容器级 token。
        let color = if active {
            theme.text(TextToken::Brand)
        } else {
            theme.text(TextToken::Primary)
        };
        style.visual.background = if active {
            theme.bg(BgToken::ContainerHover)
        } else {
            Color::TRANSPARENT
        };
        style.visual.color = color;
        if !active {
            style.visual = style.visual.with_override(
                InteractionState::Hovered,
                VisualOverrides {
                    background: Some(theme.bg(BgToken::ContainerHover)),
                    ..VisualOverrides::default()
                },
            );
        }
        // `Label` 会自行套用 `TextToken::Primary`，必须显式传色，否则深色底上
        // 会出现「黑字黑底」，展开态的品牌色也不会生效。
        ViewNode::new(Decl::div(child_key(ROOT_KEY, act.local())).with_style(style))
            .with_slot(Some(act.local()))
            .child(passthrough(Label::new(label).color(color).view(theme)))
    }

    fn status_view(&self, theme: &UiTheme, shared: &Shared) -> ViewNode {
        Column::new()
            .height(STATUS_HEIGHT)
            .background(theme.bg(BgToken::Container))
            .padding_edges(Edges::new(5.0, 8.0, 0.0, 8.0))
            .view(theme)
            .with_children([Label::new(shared.status.clone())
                .size(13.0)
                .color(theme.text(TextToken::Secondary))
                .view(theme)])
    }

    /// 表单下拉框：与 `TextField` 同高同宽，点击展开 `Menu` 浮层。
    ///
    /// 用 `with_slot` 而不是裸 `child_key`：浮层内容的 key 重绑只认具名槽位，
    /// 否则它会被当成匿名节点、父级无法用对应的 `Act` 认领。
    fn select_field(
        &self,
        theme: &UiTheme,
        act: Act,
        options: &[&str],
        selected: usize,
        open: bool,
    ) -> ViewNode {
        let key = child_key(ROOT_KEY, act.local());
        let mut style = Style::default();
        style.layout.direction = FlexDirection::Row;
        style.layout.align_items = AlignItems::Center;
        style.layout.justify_content = JustifyContent::SpaceBetween;
        style.layout.width = Length::px(FIELD_WIDTH);
        style.layout.height = Length::px(32.0);
        style.layout.padding = Edges::new(0.0, 10.0, 0.0, 10.0);
        style.layout.border = Edges::all(1.0);
        style.visual.corner_radius = Corners::all(3.0);
        style.visual.background = theme.bg(BgToken::SpecialComponent);
        style.visual.border_color = if open {
            theme.brand(Shade::Base)
        } else {
            theme.border(BorderToken::Level2)
        };
        style.visual.cursor = Cursor::Pointer;
        if !open {
            style.visual = style.visual.with_override(
                InteractionState::Hovered,
                VisualOverrides {
                    border_color: Some(theme.brand(Shade::Base)),
                    ..VisualOverrides::default()
                },
            );
        }
        let mut arrow = Decl::icon(child_key(ROOT_KEY, 0), zzzui::element::Icon::ChevronDown);
        if let Decl::Icon { size, .. } = &mut arrow {
            *size = 12.0;
        }
        let mut arrow_style = Style::default();
        arrow_style.visual.color = theme.text(TextToken::Secondary);
        let text = options
            .get(selected.min(options.len().saturating_sub(1)))
            .copied()
            .unwrap_or_default();
        ViewNode::new(Decl::div(key).with_style(style))
            .with_slot(Some(act.local()))
            .child(passthrough(Label::new(text).view(theme)))
            .child(passthrough(ViewNode::new(arrow.with_style(arrow_style))))
    }

    /// 表单标签：固定宽度 + 右对齐，让各行输入框的起点一致。
    fn form_label(text: &str, theme: &UiTheme) -> ViewNode {
        let mut style = Style::default();
        style.layout.direction = FlexDirection::Row;
        style.layout.align_items = AlignItems::Center;
        style.layout.justify_content = JustifyContent::End;
        style.layout.width = Length::px(LABEL_WIDTH);
        style.layout.height = Length::px(32.0);
        ViewNode::new(Decl::div(ROOT_KEY).with_style(style)).child(Label::new(text).view(theme))
    }

    fn dialog(&self, owner: NodeKey, theme: &UiTheme, shared: &Shared) -> OverlayView {
        let mut body = vec![
            Tabs::new(["本地终端", "SSH"])
                .selected(self.tab)
                .with_key(Act::Tabs)
                .view(theme),
        ];

        if self.tab == 0 {
            let shells: Vec<&str> = self.shells.iter().map(|(name, _)| name.as_str()).collect();
            // Column 默认交叉轴拉伸，必须显式限宽，Center 才有居中效果。
            let form = Column::new()
                .gap(8.0)
                .width(FIELD_WIDTH)
                .view(theme)
                .with_children([
                    Label::new("本地 Shell").view(theme),
                    self.select_field(theme, Act::Shell, &shells, self.shell, self.shell_open),
                ]);
            body.push(Center::new().view(theme).with_children([form]));
        } else {
            let mut ssh_fields = vec![
                Row::new().gap(8.0).view(theme).with_children([
                    Self::form_label("主机", theme),
                    TextField::new(self.host.clone())
                        .width(FIELD_WIDTH)
                        .with_key(Act::Host)
                        .view(theme),
                ]),
                Row::new().gap(8.0).view(theme).with_children([
                    Self::form_label("端口", theme),
                    TextField::new(self.port.clone())
                        .width(FIELD_WIDTH)
                        .with_key(Act::Port)
                        .view(theme),
                ]),
                Row::new().gap(8.0).view(theme).with_children([
                    Self::form_label("用户", theme),
                    TextField::new(self.user.clone())
                        .width(FIELD_WIDTH)
                        .with_key(Act::User)
                        .view(theme),
                ]),
                // 认证方式用下拉选择，后面的字段随选择切换：只展示当前方式需要的属性。
                Row::new().gap(8.0).view(theme).with_children([
                    Self::form_label("认证方式", theme),
                    self.select_field(
                        theme,
                        Act::AuthMode,
                        AUTH_OPTIONS,
                        self.auth,
                        self.auth_open,
                    ),
                ]),
            ];
            if self.auth == 0 {
                ssh_fields.push(
                    Row::new().gap(8.0).view(theme).with_children([
                        Self::form_label("密码", theme),
                        TextField::new(self.password.clone())
                            .width(FIELD_WIDTH)
                            .password(true)
                            .with_key(Act::Password)
                            .view(theme),
                    ]),
                );
            } else {
                ssh_fields.push(
                    Row::new().gap(8.0).view(theme).with_children([
                        Self::form_label("密钥", theme),
                        TextField::new(self.identity.clone())
                            .width(FIELD_WIDTH)
                            .with_key(Act::Identity)
                            .view(theme),
                    ]),
                );
                ssh_fields.push(
                    Row::new().gap(8.0).view(theme).with_children([
                        Self::form_label("", theme),
                        Label::new("留空则自动尝试默认密钥或 ssh config")
                            .size(12.0)
                            .color(theme.text(TextToken::Secondary))
                            .view(theme),
                    ]),
                );
            }
            // 标签列 + 间距 + 字段：固定宽度后由外层 Center 水平居中。
            let form = Column::new()
                .gap(8.0)
                .width(LABEL_WIDTH + 8.0 + FIELD_WIDTH)
                .view(theme)
                .with_children(ssh_fields);
            body.push(Center::new().view(theme).with_children([form]));
        }

        // 连接失败后对话框重新打开，空页上的提示会被遮罩盖住：把原因放进对话框。
        if shared.screen == Screen::Failed && !shared.label.is_empty() {
            body.push(
                Label::new(shared.label.clone())
                    .size(12.0)
                    .color(theme.functional(Family::Error, Shade::Base))
                    .view(theme),
            );
        }

        Dialog::new("新建连接")
            .key(child_key(owner, DIALOG_SLOT))
            .width(520.0)
            .content(Column::new().gap(12.0).view(theme).with_children(body))
            .actions([
                Button::new("取消").with_key(Act::Cancel).view(theme),
                Button::new("连接").with_key(Act::Connect).view(theme),
            ])
            .build(owner, theme)
    }

    /// rz 上传：远端请求接收文件时让用户填写本地路径。
    fn upload_dialog(
        &self,
        owner: NodeKey,
        theme: &UiTheme,
        path: &str,
        notice: &str,
    ) -> OverlayView {
        let content = Column::new().gap(10.0).view(theme).with_children([
            Label::new(notice.to_owned())
                .size(12.0)
                .color(theme.text(TextToken::Secondary))
                .view(theme),
            Row::new().gap(8.0).view(theme).with_children([
                Label::new("本地文件").view(theme),
                TextField::new(path.to_owned())
                    .width(360.0)
                    .with_key(Act::UploadPath)
                    .view(theme),
            ]),
            Label::new("支持绝对路径与 ~/ 开头的路径")
                .size(12.0)
                .color(theme.text(TextToken::Secondary))
                .view(theme),
        ]);

        Dialog::new("ZMODEM 上传")
            .key(child_key(owner, UPLOAD_SLOT))
            .width(520.0)
            .content(content)
            .actions([
                Button::new("取消").with_key(Act::UploadCancel).view(theme),
                Button::new("发送").with_key(Act::UploadAccept).view(theme),
            ])
            .build(owner, theme)
    }

    /// 指针是否落在终端查看区（排除菜单栏、标签栏、状态栏与两侧侧栏）。
    fn in_terminal(&self, x: f64, y: f64) -> bool {
        let Ok(shared) = self.shared.lock() else {
            return false;
        };
        if shared.screen != Screen::Session {
            return false;
        }
        Geometry::new(
            shared.width,
            shared.height,
            !shared.tabs.is_empty(),
            shared.quick_open,
            shared.sftp.open,
        )
        .contains(x, y)
    }
}

/// 展开 `~/` 前缀（rz 上传路径输入）。
fn expand_path(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(path)
}

/// 让纯装饰子节点不参与命中：点击穿透到父级（父级才带 `Act::*` 的 key）。
fn passthrough(mut node: ViewNode) -> ViewNode {
    match &mut node.decl {
        Decl::Text { style, .. } | Decl::Icon { style, .. } => {
            style.visual.pointer_events = PointerEvents::None;
        }
        _ => {}
    }
    node
}

/// 第 `index` 个目录项的声明 key。
fn sftp_entry_key(owner: NodeKey, index: usize) -> NodeKey {
    child_key(owner, SFTP_ENTRY_BASE + index as LocalKey)
}

/// 会话管理列表第 `index` 行的声明 key。
fn session_entry_key(owner: NodeKey, index: usize) -> NodeKey {
    child_key(owner, SESSION_ENTRY_BASE + index as LocalKey)
}

/// 宏管理列表第 `index` 行的声明 key。
fn macro_entry_key(owner: NodeKey, index: usize) -> NodeKey {
    child_key(owner, MACRO_ENTRY_BASE + index as LocalKey)
}

/// 字节数 → 便于阅读的尺寸（用于 SFTP 传输进度）。
fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// 下载时给一个可用的本地默认路径。
fn default_local_path(name: &str) -> String {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    if home.is_empty() {
        name.to_owned()
    } else {
        std::path::PathBuf::from(home)
            .join(name)
            .to_string_lossy()
            .into_owned()
    }
}

/// 第 `index` 个菜单标题的声明 key（和 `view` 里的 `Act::Menu*` 对齐）。
fn menu_title_key(owner: NodeKey, index: usize) -> NodeKey {
    let act = match index {
        0 => Act::MenuFile,
        1 => Act::MenuSession,
        _ => Act::MenuHelp,
    };
    child_key(owner, act.local())
}

/// 标签的连接来源：复制标签时据此重建会话，不必再走连接对话框。
#[derive(Clone, Debug)]
enum TabSource {
    Ssh(ConnectOptions),
    Local(String),
}

/// 一个会话标签的完整状态：终端、连接状态、SFTP 通道互不共享。
struct Tab {
    id: TabId,
    /// 建立该标签时使用的连接参数（SSH 凭据只保存在内存里）。
    source: Option<TabSource>,
    terminal: Terminal,
    screen: Screen,
    label: String,
    status: String,
    sftp: SftpState,
    sftp_actions: Option<UnboundedSender<SftpAction>>,
    input: InputSlot,
    /// ZMODEM 探测缓存：只保留可能是起始标记前缀的尾部字节。
    zmodem_probe: Vec<u8>,
    /// 进行中的 rz / sz 会话。
    zmodem: Option<Transfer>,
    /// 远端 rz 已就绪、等待用户填写本地路径。
    upload_pending: bool,
    /// 传输提示（显示在状态栏）。
    transfer_notice: Option<String>,
}

impl Tab {
    /// 标签栏标题：优先远端 OSC 标题，其次连接标签，最后默认名；过长截断。
    fn title(&self) -> String {
        let remote = self.terminal.title();
        let text = if remote != "lieterm" {
            remote
        } else if self.label.is_empty() {
            "新标签".to_owned()
        } else {
            self.label.clone()
        };
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut chars = text.chars();
        let short: String = chars.by_ref().take(24).collect();
        if chars.next().is_some() {
            format!("{short}…")
        } else {
            short
        }
    }

    /// 把字节写到这个标签的输入通道（按键 / 宏 / 协议应答共用）。
    fn send_input(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if let Ok(slot) = self.input.lock()
            && let Some(sender) = slot.as_ref()
        {
            let _ = sender.send(UiEvent::Input(bytes.to_vec()));
        }
    }

    /// 处理一段远端输出：普通输出进终端，ZMODEM 头之后进传输状态机。
    /// 返回需要写回 PTY 的协议字节。
    fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        // 1. 传输中：全部交给状态机。
        if self.zmodem.is_some() {
            return self.feed_transfer(bytes);
        }

        // 2. 探测起始标记；尾部只保留可能构成标记前缀的字节。
        self.zmodem_probe.extend_from_slice(bytes);
        match transfer::detect(&self.zmodem_probe) {
            Probe::None => {
                let keep = transfer::pending_suffix_len(&self.zmodem_probe);
                let split = self.zmodem_probe.len() - keep;
                self.terminal.advance(&self.zmodem_probe[..split]);
                self.zmodem_probe.drain(..split);
                Vec::new()
            }
            Probe::Partial(from) => {
                self.terminal.advance(&self.zmodem_probe[..from]);
                self.zmodem_probe.drain(..from);
                Vec::new()
            }
            Probe::Start { from, direction } => {
                self.terminal.advance(&self.zmodem_probe[..from]);
                let rest = self.zmodem_probe[from..].to_vec();
                self.zmodem_probe.clear();
                self.start_transfer(direction, &rest)
            }
        }
    }

    fn feed_transfer(&mut self, bytes: &[u8]) -> Vec<u8> {
        let Some(transfer) = self.zmodem.as_mut() else {
            return Vec::new();
        };
        let wire = transfer.feed(bytes);
        let events = std::mem::take(&mut transfer.events);
        let done = transfer.is_done();
        self.apply_transfer_events(events);
        if done {
            self.zmodem = None;
            self.upload_pending = false;
        }
        wire
    }

    fn start_transfer(&mut self, direction: Direction, rest: &[u8]) -> Vec<u8> {
        let started = match direction {
            Direction::Download => Transfer::download(),
            Direction::Upload => Transfer::upload(),
        };
        let mut transfer = match started {
            Ok(transfer) => transfer,
            Err(err) => {
                self.transfer_notice = Some(format!("ZMODEM 启动失败：{err}"));
                return Vec::new();
            }
        };
        let wire = transfer.feed(rest);
        let events = std::mem::take(&mut transfer.events);
        self.apply_transfer_events(events);
        let done = transfer.is_done();
        if done {
            return wire;
        }
        self.upload_pending = direction == Direction::Upload;
        self.zmodem = Some(transfer);
        wire
    }

    fn apply_transfer_events(&mut self, events: Vec<TransferEvent>) {
        for event in events {
            match event {
                TransferEvent::Info(text) => self.transfer_notice = Some(text),
                TransferEvent::Aborted(text) => {
                    self.transfer_notice = Some(format!("传输失败：{text}"));
                }
                TransferEvent::Finished => match &mut self.transfer_notice {
                    Some(text) => text.push_str("（完成）"),
                    None => self.transfer_notice = Some("传输完成".to_owned()),
                },
            }
        }
    }

    /// 用户填写路径后开始发送；返回要写回 PTY 的字节。
    fn begin_upload(&mut self, path: PathBuf) -> Result<Vec<u8>, String> {
        let Some(transfer) = self.zmodem.as_mut() else {
            return Err("没有等待中的上传".to_owned());
        };
        let wire = transfer.begin_upload(&path);
        let events = std::mem::take(&mut transfer.events);
        let done = transfer.is_done();
        self.apply_transfer_events(events);
        if wire.is_ok() {
            self.upload_pending = false;
        }
        if done {
            self.zmodem = None;
        }
        wire
    }

    /// 取消当前传输，返回对端需要的取消序列。
    fn cancel_transfer(&mut self) -> Vec<u8> {
        self.zmodem = None;
        self.upload_pending = false;
        self.transfer_notice = Some("传输已取消".to_owned());
        transfer::CANCEL.to_vec()
    }
}

pub struct LietermApp {
    shared: Arc<Mutex<Shared>>,
    actions_tx: std_mpsc::Sender<UiAction>,
    actions_rx: std_mpsc::Receiver<UiAction>,
    events_tx: UnboundedSender<(TabId, SshEvent)>,
    events_rx: UnboundedReceiver<(TabId, SshEvent)>,
    runtime: tokio::runtime::Handle,
    redraw: Arc<Mutex<Option<(WindowOpener, WindowId)>>>,
    /// 所有会话标签（按标签栏顺序）。
    tabs: Vec<Tab>,
    /// 活动标签（`None` 表示还没有会话）。
    active: Option<TabId>,
    next_tab_id: TabId,
    /// 新建标签用的终端样式（字体 / 配色 / 编码，与启动终端一致）。
    style: term::TermStyle,
    cell_width: f64,
    cell_height: f64,
    last_viewport: (f64, f64),
    focus_pending: bool,
    /// 远程标题同步（只在变化时调 `set_title`）。
    last_title: String,
    cursor_visible: bool,
    next_blink: Instant,
    /// `LIETERM_LOCAL` 指定的本地 shell：首帧自动进入会话（试验 / 演示用）。
    pending_local: Option<String>,
    /// 上传提示的上一帧开合状态：只在打开瞬间填默认路径。
    upload_was_open: bool,
}

impl LietermApp {
    pub fn new(
        terminal: Terminal,
        runtime: tokio::runtime::Handle,
        pending_local: Option<String>,
    ) -> Self {
        let shared = Arc::new(Mutex::new(Shared {
            width: 800.0,
            height: 600.0,
            // 终端默认关 IME：`LIETERM_IME=1` 时才允许拼音输入。
            ime: std::env::var_os("LIETERM_IME").is_some(),
            ..Shared::default()
        }));
        let (actions_tx, actions_rx) = std_mpsc::channel();
        let (events_tx, events_rx) = unbounded_channel();
        let redraw = Arc::new(Mutex::new(None));
        Self {
            shared,
            actions_tx,
            actions_rx,
            events_tx,
            events_rx,
            runtime,
            redraw,
            tabs: Vec::new(),
            active: None,
            next_tab_id: 1,
            style: terminal.style(),
            cell_width: terminal.cell_width(),
            cell_height: terminal.cell_height(),
            last_viewport: (0.0, 0.0),
            focus_pending: false,
            last_title: String::new(),
            cursor_visible: true,
            next_blink: Instant::now() + Duration::from_millis(530),
            pending_local,
            upload_was_open: false,
        }
    }

    fn active_index(&self) -> Option<usize> {
        let id = self.active?;
        self.tabs.iter().position(|tab| tab.id == id)
    }

    fn active_tab(&self) -> Option<&Tab> {
        self.active_index().and_then(|index| self.tabs.get(index))
    }

    fn active_tab_mut(&mut self) -> Option<&mut Tab> {
        let index = self.active_index()?;
        self.tabs.get_mut(index)
    }

    fn tab_mut(&mut self, id: TabId) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|tab| tab.id == id)
    }

    /// 每个标签一个 Bridge：事件带标签 id，后台任务不会串台。
    fn bridge_for(&self, tab: TabId) -> Bridge {
        Bridge::new(tab, self.events_tx.clone(), Arc::clone(&self.redraw))
    }

    /// 新建标签并设为活动；尺寸沿用当前活动标签，首标签用 80×24。
    fn open_tab(&mut self) -> TabId {
        self.capture_tab_sftp();
        let (cols, rows) = self
            .active_tab()
            .map(|tab| (tab.terminal.cols(), tab.terminal.rows()))
            .unwrap_or((80, 24));
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let mut terminal = Terminal::new(cols, rows, self.style.clone());
        terminal.set_cursor_visible(self.cursor_visible);
        self.tabs.push(Tab {
            id,
            source: None,
            terminal,
            screen: Screen::Connecting,
            label: String::new(),
            status: String::new(),
            sftp: SftpState::default(),
            sftp_actions: None,
            input: Arc::new(Mutex::new(None)),
            zmodem_probe: Vec::new(),
            zmodem: None,
            upload_pending: false,
            transfer_notice: None,
        });
        self.active = Some(id);
        // 新标签需要按当前视口重新计算行列（不能沿用上次的 viewport 缓存）。
        self.last_viewport = (0.0, 0.0);
        self.show_tab_sftp();
        id
    }

    /// 把活动标签投影到 `Shared`（Root 只读 `Shared`，不持有标签）。
    ///
    /// `Shared.sftp` 例外：活动标签的侧边栏由 Root 直接编辑，切换标签时才双向搬运。
    fn sync(&self) {
        let summaries: Vec<TabSummary> = self
            .tabs
            .iter()
            .map(|tab| TabSummary { title: tab.title() })
            .collect();
        let active_tab = self.active_index().unwrap_or(0);
        let mut shared = self.shared.lock().expect("shared lock");
        shared.tabs = summaries;
        shared.active_tab = active_tab;
        match self.active_tab() {
            Some(tab) => {
                shared.screen = tab.screen;
                shared.label = tab.label.clone();
                shared.status = tab.status.clone();
                shared.input = Arc::clone(&tab.input);
                shared.transfer_active = tab.zmodem.is_some();
                shared.upload_open = tab.upload_pending && tab.zmodem.is_some();
                if shared.upload_open {
                    shared.upload_notice =
                        "远端 rz 请求接收文件，请填写本地文件路径（取消会通知对端）".to_owned();
                }
            }
            None => {
                shared.screen = Screen::Idle;
                shared.label.clear();
                shared.status.clear();
                shared.sftp = SftpState::default();
                shared.input = Arc::new(Mutex::new(None));
                shared.transfer_active = false;
                shared.upload_open = false;
            }
        }
    }

    /// 切换标签前，把 Root 编辑过的侧边栏状态收回当前活动标签。
    fn capture_tab_sftp(&mut self) {
        let Some(index) = self.active_index() else {
            return;
        };
        let sftp = self
            .shared
            .lock()
            .map(|shared| shared.sftp.clone())
            .unwrap_or_default();
        self.tabs[index].sftp = sftp;
    }

    /// 切换标签后，把新活动标签的侧边栏状态投影给 Root。
    fn show_tab_sftp(&self) {
        let sftp = self
            .active_tab()
            .map(|tab| tab.sftp.clone())
            .unwrap_or_default();
        if let Ok(mut shared) = self.shared.lock() {
            shared.sftp = sftp;
        }
    }

    /// 活动标签的 SFTP 事件要立刻反映到侧边栏（Root 读 `Shared`）。
    fn push_active_sftp(&self, id: TabId) {
        if self.active != Some(id) {
            return;
        }
        let sftp = self
            .tabs
            .iter()
            .find(|tab| tab.id == id)
            .map(|tab| tab.sftp.clone());
        if let Some(sftp) = sftp
            && let Ok(mut shared) = self.shared.lock()
        {
            shared.sftp = sftp;
        }
    }

    fn refresh_status(&mut self) {
        let Some(index) = self.active_index() else {
            return;
        };
        let status = {
            let tab = &self.tabs[index];
            let mut status = format!(
                "{}  |  {}×{}  |  {}",
                tab.terminal.title(),
                tab.terminal.cols(),
                tab.terminal.rows(),
                tab.label
            );
            if let Some(notice) = &tab.transfer_notice {
                status.push_str("  |  ");
                status.push_str(notice);
            }
            status
        };
        self.tabs[index].status = status.clone();
        if let Ok(mut shared) = self.shared.lock() {
            shared.status = status;
        }
    }

    /// 启动本地 shell：与 SSH 共用 input 通道与事件，只是不再经过网络。
    fn start_local(&mut self, shell: String, ctx: &mut WindowContext<'_>) {
        let id = self.open_tab();
        let (input_tx, input_rx) = unbounded_channel();
        let (cols, rows) = if let Some(tab) = self.tab_mut(id) {
            tab.label = format!("正在启动 {shell}…");
            tab.source = Some(TabSource::Local(shell.clone()));
            if let Ok(mut slot) = tab.input.lock() {
                *slot = Some(input_tx.clone());
            }
            (
                u16::try_from(tab.terminal.cols()).unwrap_or(u16::MAX),
                u16::try_from(tab.terminal.rows()).unwrap_or(u16::MAX),
            )
        } else {
            (80, 24)
        };
        if let Ok(mut shared) = self.shared.lock() {
            shared.dialog_open = false;
        }
        local::spawn(&shell, cols, rows, input_rx, self.bridge_for(id));
        // 试验模式：启动后自动下发一条命令，方便无键盘交互地验证渲染链路。
        if let Ok(command) = std::env::var("LIETERM_LOCAL_CMD") {
            let _ = input_tx.send(UiEvent::Input(format!("{command}\r\n").into_bytes()));
        }
        self.sync();
        ctx.invalidate();
    }

    fn connect_ssh(&mut self, mut options: ConnectOptions) {
        let id = self.open_tab();
        let (input_tx, input_rx) = unbounded_channel();
        let label = format!(
            "正在连接 {}@{}:{}…",
            options.user, options.host, options.port
        );
        if let Some(tab) = self.tab_mut(id) {
            options.cols = tab.terminal.cols() as u32;
            options.rows = tab.terminal.rows() as u32;
            tab.label = label;
            tab.source = Some(TabSource::Ssh(options.clone()));
            if let Ok(mut slot) = tab.input.lock() {
                *slot = Some(input_tx);
            }
        }
        if let Ok(mut shared) = self.shared.lock() {
            shared.dialog_open = false;
        }
        ssh::spawn(&self.runtime, options, input_rx, self.bridge_for(id));
        self.sync();
    }

    /// 复制标签：SSH 会话复用同一组参数直接重连，本地 shell 重启同一程序。
    fn duplicate_tab(&mut self, index: usize, ctx: &mut WindowContext<'_>) {
        let Some(source) = self.tabs.get(index).and_then(|tab| tab.source.clone()) else {
            return;
        };
        match source {
            TabSource::Ssh(options) => self.connect_ssh(options),
            TabSource::Local(shell) => self.start_local(shell, ctx),
        }
    }

    fn select_tab(&mut self, index: usize) {
        self.capture_tab_sftp();
        if let Some(tab) = self.tabs.get_mut(index) {
            self.active = Some(tab.id);
            tab.terminal.set_cursor_visible(true);
            self.focus_pending = true;
            self.cursor_visible = true;
            self.next_blink = Instant::now() + Duration::from_millis(530);
        }
        self.show_tab_sftp();
        self.sync();
    }

    /// 关闭标签：断开它的 SFTP / 输入通道；最后一个标签关闭后回到空页 + 连接对话框。
    fn close_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        self.capture_tab_sftp();
        let id = self.tabs[index].id;
        let mut tab = self.tabs.remove(index);
        if let Some(actions) = tab.sftp_actions.take() {
            let _ = actions.send(SftpAction::Close);
        }
        if let Ok(mut slot) = tab.input.lock() {
            *slot = None;
        }
        drop(tab);

        if self.tabs.is_empty() {
            self.active = None;
            self.focus_pending = false;
            self.last_viewport = (0.0, 0.0);
            if let Ok(mut shared) = self.shared.lock() {
                shared.dialog_open = true;
            }
        } else if self.active == Some(id) {
            self.active = Some(self.tabs[index.min(self.tabs.len() - 1)].id);
            self.focus_pending = true;
        }
        self.show_tab_sftp();
        self.sync();
    }

    /// 上传提示打开的第一帧填入默认路径；之后保留用户编辑内容。
    fn prepare_upload_prompt(&mut self) {
        let open = self
            .active_tab()
            .is_some_and(|tab| tab.upload_pending && tab.zmodem.is_some());
        if open
            && !self.upload_was_open
            && let Ok(mut shared) = self.shared.lock()
        {
            shared.upload_path = transfer::download_dir()
                .join("upload.bin")
                .display()
                .to_string();
        }
        self.upload_was_open = open;
    }

    /// 指针坐标 → 终端视口行列。`clamp` 为真时越界坐标钳制到边缘（拖动选择用）。
    fn cell_at(&self, x: f64, y: f64, clamp: bool) -> Option<(usize, usize)> {
        let tab = self.active_tab()?;
        if tab.screen != Screen::Session {
            return None;
        }
        let geom = {
            let shared = self.shared.lock().ok()?;
            Geometry::new(
                shared.width,
                shared.height,
                !shared.tabs.is_empty(),
                shared.quick_open,
                shared.sftp.open,
            )
        };
        if !clamp && !geom.contains(x, y) {
            return None;
        }
        let col = ((x - geom.left) / self.cell_width).floor();
        let row = ((y - geom.top) / self.cell_height).floor();
        let max_col = tab.terminal.cols().saturating_sub(1) as f64;
        let max_row = tab.terminal.rows().saturating_sub(1) as f64;
        Some((
            row.clamp(0.0, max_row) as usize,
            col.clamp(0.0, max_col) as usize,
        ))
    }

    fn disconnect(&mut self) {
        if let Some(index) = self.active_index() {
            self.close_tab(index);
        }
    }
}

impl AppHandler for LietermApp {
    fn build_ui(&mut self, ctx: &mut WindowContext<'_>) {
        let config_path = config::default_path();
        let config = Config::load(&config_path);
        let root = Root {
            shared: Arc::clone(&self.shared),
            actions: self.actions_tx.clone(),
            host: String::new(),
            port: "22".to_owned(),
            user: String::new(),
            password: String::new(),
            identity: String::new(),
            auth: 0,
            tab: 0,
            shell: 0,
            shells: local::available_shells(),
            open_menu: None,
            shell_open: false,
            auth_open: false,
            quick_editing: false,
            quick_edit_index: None,
            quick_notice: String::new(),
            quick_selected: None,
            quick_menu: None,
            quick_last_click: None,
            config,
            config_path,
            sessions_open: false,
            macros_open: false,
            session_name: String::new(),
            session_selected: None,
            macro_name: String::new(),
            macro_text: String::new(),
            macro_enter: false,
            macro_selected: None,
            manager_notice: String::new(),
            context_menu: None,
            tab_menu: None,
            selecting: false,
        };
        ctx.runtime.app_mut().mount(root);
    }

    fn window_opened(&mut self, ctx: &mut WindowContext<'_>) {
        if let Ok(mut slot) = self.redraw.lock() {
            *slot = Some((ctx.opener.clone(), ctx.id));
        }
    }

    fn on_frame(&mut self, ctx: &mut WindowContext<'_>) {
        if let Some(shell) = self.pending_local.take() {
            self.start_local(shell, ctx);
        }

        let mut dirty = false;
        while let Ok(action) = self.actions_rx.try_recv() {
            match action {
                UiAction::Connect { options } => {
                    self.connect_ssh(options);
                    dirty = true;
                }
                UiAction::ConnectLocal { shell } => {
                    self.start_local(shell, ctx);
                    dirty = true;
                }
                UiAction::SelectTab(index) => {
                    self.select_tab(index);
                    dirty = true;
                }
                UiAction::CloseTab(index) => {
                    self.close_tab(index);
                    dirty = true;
                }
                UiAction::DuplicateTab(index) => {
                    self.duplicate_tab(index, ctx);
                    dirty = true;
                }
                UiAction::Clear => {
                    if let Some(tab) = self.active_tab_mut() {
                        tab.terminal.advance(b"\x1b[2J\x1b[H");
                        // 屏幕内容已清空，旧选区不能继续留在空白格上。
                        tab.terminal.clear_selection();
                    }
                    dirty = true;
                }
                UiAction::SetScheme(name) => {
                    let theme = term::TermTheme::from_name(&name);
                    self.style.theme = theme.clone();
                    for tab in &mut self.tabs {
                        tab.terminal.set_theme(theme.clone());
                    }
                    dirty = true;
                }
                UiAction::SetEncoding(name) => {
                    let encoding = term::TextEncoding::from_name(&name);
                    self.style.encoding = encoding;
                    for tab in &mut self.tabs {
                        tab.terminal.set_encoding(encoding);
                    }
                    dirty = true;
                }
                UiAction::Disconnect => {
                    self.disconnect();
                    dirty = true;
                }
                UiAction::Quit => {
                    ctx.opener.close(ctx.id);
                    return;
                }
                UiAction::Scroll(delta) => {
                    let lines = (-(delta / self.cell_height)).round() as i32;
                    if lines != 0
                        && let Some(tab) = self.active_tab_mut()
                    {
                        tab.terminal.scroll(lines);
                        dirty = true;
                    }
                }
                UiAction::SftpOpen { options } => {
                    if let Some(index) = self.active_index() {
                        let (actions, rx) = unbounded_channel();
                        let id = self.tabs[index].id;
                        sftp::spawn(&self.runtime, options, rx, self.bridge_for(id));
                        self.tabs[index].sftp_actions = Some(actions);
                    }
                    dirty = true;
                }
                UiAction::SftpAction(action) => {
                    if let Some(tab) = self.active_tab()
                        && let Some(actions) = &tab.sftp_actions
                    {
                        let _ = actions.send(action);
                    }
                }
                UiAction::SftpClose => {
                    if let Some(tab) = self.active_tab_mut()
                        && let Some(actions) = tab.sftp_actions.take()
                    {
                        let _ = actions.send(SftpAction::Close);
                    }
                    dirty = true;
                }
                UiAction::Copy => {
                    if let Some(tab) = self.active_tab()
                        && let Some(text) = tab.terminal.selection_text()
                    {
                        ctx.set_clipboard(&text);
                    }
                    dirty = true;
                }
                UiAction::Paste => {
                    if let Some(text) = ctx.clipboard()
                        && let Some(tab) = self.active_tab()
                    {
                        let bytes = tab.terminal.paste_bytes(&text);
                        tab.send_input(&bytes);
                    }
                    dirty = true;
                }
                UiAction::SelectAll => {
                    if let Some(tab) = self.active_tab_mut() {
                        tab.terminal.select_all();
                    }
                    dirty = true;
                }
                UiAction::SelectStart { x, y } => {
                    if let Some((row, col)) = self.cell_at(x, y, false)
                        && let Some(tab) = self.active_tab_mut()
                    {
                        tab.terminal.start_selection(row, col);
                    }
                    dirty = true;
                }
                UiAction::SelectUpdate { x, y } => {
                    if let Some((row, col)) = self.cell_at(x, y, true)
                        && let Some(tab) = self.active_tab_mut()
                    {
                        tab.terminal.update_selection(row, col);
                    }
                    dirty = true;
                }
                UiAction::SelectClear => {
                    if let Some(tab) = self.active_tab_mut() {
                        tab.terminal.clear_selection();
                    }
                    dirty = true;
                }
                UiAction::UploadAccept { path } => {
                    let path = expand_path(&path);
                    if let Some(tab) = self.active_tab_mut() {
                        match tab.begin_upload(path) {
                            Ok(wire) => tab.send_input(&wire),
                            Err(err) => {
                                tab.transfer_notice = Some(format!("上传失败：{err}"));
                            }
                        }
                    }
                    dirty = true;
                }
                UiAction::UploadCancel => {
                    if let Some(tab) = self.active_tab_mut() {
                        let wire = tab.cancel_transfer();
                        tab.send_input(&wire);
                    }
                    dirty = true;
                }
            }
        }

        while let Ok((id, event)) = self.events_rx.try_recv() {
            let active = self.active == Some(id);
            match event {
                SshEvent::Connected(label) => {
                    if let Some(tab) = self.tab_mut(id) {
                        tab.label = label;
                        tab.screen = Screen::Session;
                    }
                    if active {
                        if let Ok(mut shared) = self.shared.lock() {
                            shared.dialog_open = false;
                        }
                        self.focus_pending = true;
                    }
                    dirty = true;
                }
                SshEvent::Data(bytes) => {
                    if let Some(tab) = self.tab_mut(id) {
                        // 普通输出 / ZMODEM 协议流统一入口；协议应答与终端查询
                        // （DA1、DSR、颜色查询）都要写回 PTY。
                        let wire = tab.feed(&bytes);
                        let responses = tab.terminal.take_responses();
                        tab.send_input(&responses);
                        tab.send_input(&wire);
                    }
                    dirty = true;
                }
                SshEvent::Error(message) => {
                    if let Some(tab) = self.tab_mut(id) {
                        tab.label = message;
                        tab.screen = Screen::Failed;
                    }
                    // 只有活动标签出错才弹对话框；后台标签保留自己的标签页。
                    if active && let Ok(mut shared) = self.shared.lock() {
                        shared.screen = Screen::Failed;
                        shared.dialog_open = true;
                    }
                    dirty = true;
                }
                SshEvent::Closed => {
                    if let Some(tab) = self.tab_mut(id)
                        && tab.screen == Screen::Session
                    {
                        tab.label.push_str("  (连接已关闭)");
                    }
                    dirty = true;
                }
                SshEvent::SftpReady { path, entries } => {
                    if let Some(tab) = self.tab_mut(id) {
                        tab.sftp.path = path.clone();
                        tab.sftp.entries = entries;
                        tab.sftp.selected = None;
                        tab.sftp.notice = format!("SFTP 目录：{path}");
                    }
                    self.push_active_sftp(id);
                    dirty = true;
                }
                SshEvent::SftpListing { path, entries } => {
                    if let Some(tab) = self.tab_mut(id) {
                        tab.sftp.path = path.clone();
                        tab.sftp.selected = None;
                        tab.sftp.notice =
                            format!("{}（{} 项）", path, entries.len().saturating_sub(1));
                        tab.sftp.entries = entries;
                    }
                    self.push_active_sftp(id);
                    dirty = true;
                }
                SshEvent::SftpNotice(message) => {
                    if let Some(tab) = self.tab_mut(id) {
                        tab.sftp.notice = message;
                    }
                    self.push_active_sftp(id);
                    dirty = true;
                }
                SshEvent::SftpError(message) => {
                    if let Some(tab) = self.tab_mut(id) {
                        tab.sftp.notice = format!("错误：{message}");
                        tab.sftp.progress = None;
                    }
                    self.push_active_sftp(id);
                    dirty = true;
                }
                SshEvent::SftpProgress {
                    done,
                    total,
                    active,
                } => {
                    if let Some(tab) = self.tab_mut(id) {
                        tab.sftp.progress = active.then_some(TransferProgress { done, total });
                    }
                    self.push_active_sftp(id);
                    dirty = true;
                }
            }
        }

        // 远程标题（OSC 0/2）→ 窗口标题；只在变化时下发（取活动标签）。
        let title = self
            .active_tab()
            .map_or_else(|| "lieterm".to_owned(), Tab::title);
        if title != self.last_title {
            self.last_title = title.clone();
            ctx.set_title(&format!("{title} — lieterm"));
        }

        // 终端光标是自绘的：用 0.5s 周期的重绘驱动闪烁（没有光标也算一帧静止）。
        let in_session = self
            .active_tab()
            .is_some_and(|tab| tab.screen == Screen::Session);
        if in_session {
            let now = Instant::now();
            if now >= self.next_blink {
                self.cursor_visible = !self.cursor_visible;
                let visible = self.cursor_visible;
                if let Some(tab) = self.active_tab_mut() {
                    tab.terminal.set_cursor_visible(visible);
                }
                self.next_blink = now + Duration::from_millis(530);
                dirty = true;
            }
            // 宿主级定时帧：到点由平台自动重绘，不再依赖后台线程 `redraw`。
            ctx.wake_in((self.next_blink - now).as_secs_f64().max(0.0));
        }

        let viewport = ctx.runtime.viewport();
        let scale = ctx.runtime.scale();
        let quick_open = self.shared.lock().is_ok_and(|shared| shared.quick_open);
        let geom = Geometry::new(
            viewport.width,
            viewport.height,
            !self.tabs.is_empty(),
            quick_open,
            self.active_tab().is_some_and(|tab| tab.sftp.open),
        );
        let term_width = geom.width();
        let term_height = geom.height();
        if (term_width, term_height) != self.last_viewport {
            self.last_viewport = (term_width, term_height);
            let cols = geom.cols(self.cell_width).clamp(20, 400);
            let rows = geom.rows(self.cell_height).clamp(6, 200);
            let px_width = (term_width * scale).round().max(1.0) as u32;
            let px_height = (term_height * scale).round().max(1.0) as u32;
            // 所有标签一起 resize：切换过去时行列已经正确。
            for tab in &mut self.tabs {
                tab.terminal.resize(cols, rows);
                if let Ok(slot) = tab.input.lock()
                    && let Some(sender) = slot.as_ref()
                {
                    let _ = sender.send(UiEvent::Resize {
                        cols: cols as u32,
                        rows: rows as u32,
                        px_width,
                        px_height,
                    });
                }
            }
            if let Ok(mut shared) = self.shared.lock() {
                shared.width = viewport.width;
                shared.height = viewport.height;
            }
            dirty = true;
        }

        if dirty {
            let px_width = ctx.runtime.frame().width();
            let px_height = ctx.runtime.frame().height();
            let top_inset = geom.top * scale;
            let render_start = Instant::now();
            // 只光栅化活动标签；后台标签保留自己的终端状态，切回来再画。
            let image = if let Some(tab) = self.active_tab_mut() {
                tab.terminal
                    .render(px_width, px_height, scale, top_inset, geom.left * scale);
                tab.terminal.image()
            } else {
                None
            };
            let render_ms = render_start.elapsed().as_secs_f64() * 1000.0;
            if std::env::var_os("LIETERM_TRACE").is_some()
                && let Some(tab) = self.active_tab()
            {
                eprintln!(
                    "[lieterm] tab={} frame={}x{} image={:?} received={} screen_lines={} cols={} rows={} render={render_ms:.2}ms",
                    tab.id,
                    px_width,
                    px_height,
                    image.as_ref().map(|image| (image.width, image.height)),
                    tab.terminal.received(),
                    tab.terminal.screen_lines(),
                    tab.terminal.cols(),
                    tab.terminal.rows()
                );
            }
            // 整窗底图 + 状态栏声明；像素与声明各自报告失效。
            ctx.runtime.set_background_image(image);
            self.refresh_status();
            self.prepare_upload_prompt();
            self.sync();
            ctx.invalidate();
        }

        if self.focus_pending {
            // 声明重建发生在本帧的 `run_frame` 里：`KeyCapture` 还没进实体表时
            // 找不到捕获面，再排一帧即可。
            if ctx.runtime.focus_passive() {
                self.focus_pending = false;
            } else {
                ctx.opener.redraw(ctx.id);
            }
        }
    }
}

#[cfg(test)]
mod tests;
