//! zzzui 应用层：连接对话框（本地 / SSH）+ 终端画面 + 菜单栏 + 后台事件泵。
//!
//! 启动即打开一个模态对话框：两个选项卡（本地终端 / SSH），本地终端用下拉选择 shell。
//! 菜单栏的「文件 / 会话 / 帮助」下拉与 shell 下拉都走 zzzui 的 `Menu` 浮层；
//! 终端像素走 `Runtime::set_background_image`，状态栏是普通 Widget 声明。

use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use zzzui::{
    element::{
        AlignItems, Cursor, Decl, FlexDirection, InteractionState, JustifyContent, Length,
        LocalKey, NodeKey, PointerEvents, ROOT_KEY, RawCommand, Style, VisualOverrides, child_key,
    },
    geom::{Color, Corners, Edges, Size},
    platform::{AppHandler, WindowContext, WindowId, WindowOpener},
    view::{OverlayView, ViewNode},
    widget::{
        BgToken, BorderToken, Button, Center, Column, Dialog, Key, KeyCapture, Label, Menu, Row,
        Scroll, Shade, Tabs, TextField, TextToken, UiTheme, UpdateCtx, Widget,
    },
};

use crate::{
    keys, local, sftp,
    sftp::SftpAction,
    ssh,
    ssh::{AuthOptions, ConnectOptions},
    term::{self, STATUS_HEIGHT, Terminal},
};

/// 菜单栏高度（逻辑像素）。
const MENUBAR_HEIGHT: f64 = 28.0;
/// 声明槽位：对话框 / 菜单栏根 / 菜单浮层 / shell 下拉。
const DIALOG_SLOT: LocalKey = 900;
const MENUBAR_SLOT: LocalKey = 901;
const MENU_POPUP_BASE: LocalKey = 910;
const SHELL_POPUP_SLOT: LocalKey = 920;
/// SFTP 侧边栏宽度与目录项槽位起点。
const SIDEBAR_WIDTH: f64 = 280.0;
const SFTP_ENTRY_BASE: LocalKey = 930;

/// 菜单栏定义：(标题, 选项)。选项序号与 `on_menu` 的分支一一对应。
const MENUS: &[(&str, &[&str])] = &[
    ("文件", &["新建连接", "退出"]),
    ("会话", &["清屏", "断开", "SFTP 侧边栏"]),
    ("帮助", &["关于"]),
];

/// 本地 shell 选项：(显示名, 命令)。
const SHELLS: &[(&str, &str)] = &[
    ("PowerShell 7", "pwsh.exe"),
    ("Windows PowerShell", "powershell.exe"),
    ("Git Bash", r"C:\Program Files\Git\bin\bash.exe"),
    ("命令提示符", "cmd.exe"),
];

zzzui::key_enum! {
    enum Act {
        Host,
        Port,
        User,
        Password,
        Connect,
        Cancel,
        Tabs,
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
        SftpClose,
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
}

pub enum UiAction {
    Connect {
        options: ConnectOptions,
        input: UnboundedReceiver<UiEvent>,
    },
    /// 启动本地 shell（pwsh / cmd / bash），不经过 SSH。
    ConnectLocal {
        shell: String,
    },
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
    /// SFTP 侧边栏状态。
    pub sftp: SftpState,
    pub input: InputSlot,
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
            sftp: SftpState::default(),
            input: Arc::new(Mutex::new(None)),
        }
    }
}

/// 后台任务用它把数据到达事件送到 UI 线程，并请求一次重画。
#[derive(Clone)]
pub struct Bridge {
    events: UnboundedSender<SshEvent>,
    redraw: Arc<Mutex<Option<(WindowOpener, WindowId)>>>,
}

impl Bridge {
    pub fn new(
        events: UnboundedSender<SshEvent>,
        redraw: Arc<Mutex<Option<(WindowOpener, WindowId)>>>,
    ) -> Self {
        Self { events, redraw }
    }

    pub fn notify(&self, event: SshEvent) {
        let _ = self.events.send(event);
        if let Ok(slot) = self.redraw.lock()
            && let Some((opener, id)) = slot.as_ref()
        {
            opener.redraw(*id);
        }
    }
    /// 测试用：只收事件、不需要窗口的 Bridge（内存 SSH 服务器集成测试）。
    #[cfg(test)]
    pub(crate) fn from_events(events: UnboundedSender<SshEvent>) -> Self {
        Self {
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
    tab: usize,
    shell: usize,
    open_menu: Option<usize>,
    shell_open: bool,
}

impl Widget for Root {
    fn view(&self, theme: &UiTheme) -> ViewNode {
        let shared = self.shared.lock().expect("shared lock");
        let width = shared.width.max(1.0);
        let height = shared.height.max(1.0);
        let menubar = self.menubar_view(theme);

        if shared.screen == Screen::Session {
            let area = (height - MENUBAR_HEIGHT - STATUS_HEIGHT).max(1.0);
            let capture_width = if shared.sftp.open {
                (width - SIDEBAR_WIDTH).max(1.0)
            } else {
                width
            };
            let terminal = KeyCapture::new()
                .size(capture_width, area)
                .ime(shared.ime)
                .view(theme);
            let body = if shared.sftp.open {
                Row::new()
                    .width(width)
                    .height(area)
                    .view(theme)
                    .with_children([self.sftp_sidebar_view(theme, area, &shared.sftp), terminal])
            } else {
                terminal
            };
            Column::new()
                .width(width)
                .height(height)
                .view(theme)
                .with_children([menubar, body, self.status_view(theme, &shared)])
        } else {
            let hint = if shared.screen == Screen::Connecting {
                "正在连接…".to_owned()
            } else if shared.label.is_empty() {
                "文件 > 新建连接".to_owned()
            } else {
                shared.label.clone()
            };
            Column::new()
                .width(width)
                .height(height)
                .background(theme.bg(BgToken::Page))
                .view(theme)
                .with_children([
                    menubar,
                    Center::new()
                        .grow(1.0)
                        .view(theme)
                        .with_children([Label::new(hint).view(theme)]),
                ])
        }
    }

    fn overlays(&self, owner: NodeKey, theme: &UiTheme) -> Vec<OverlayView> {
        let shared = self.shared.lock().expect("shared lock");
        let dialog_open = shared.dialog_open;
        let mut out = Vec::new();
        if dialog_open {
            out.push(self.dialog(owner, theme, &shared));
        }
        drop(shared);

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
                Menu::new(SHELLS.iter().map(|(name, _)| *name))
                    .anchored(child_key(owner, Act::Shell.local()))
                    .key(child_key(owner, SHELL_POPUP_SLOT))
                    .selected(self.shell)
                    .width(220.0)
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
}
impl Root {
    fn on_command_inner(
        &mut self,
        owner: NodeKey,
        command: &RawCommand,
        ctx: &mut UpdateCtx,
    ) -> bool {
        // 1. 浮层关闭请求：对话框 / 菜单栏下拉 / shell 下拉各自只关自己。
        if let RawCommand::OverlayDismiss { key, .. } = command {
            if *key == child_key(owner, DIALOG_SLOT) {
                self.set_dialog(false);
                self.shell_open = false;
                ctx.mark_dirty();
                return true;
            }
            if self.shell_open && *key == child_key(owner, SHELL_POPUP_SLOT) {
                self.shell_open = false;
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
            let (screen, input) = {
                let shared = self.shared.lock().expect("shared lock");
                (shared.screen, Arc::clone(&shared.input))
            };
            if screen != Screen::Session {
                return false;
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

        // 终端回滚：滚轮不依赖点击，先于 Click 判断。
        if let RawCommand::Wheel { delta, .. } = command {
            let _ = self.actions.send(UiAction::Scroll(*delta));
            return true;
        }

        let RawCommand::Click { .. } = command else {
            return false;
        };

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

        // 4. shell 下拉选项。
        if self.shell_open
            && let Some(index) =
                Menu::selection(child_key(owner, SHELL_POPUP_SLOT), SHELLS.len(), command)
        {
            self.shell = index;
            self.shell_open = false;
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

        // 6. 对话框：选项卡 / shell 按钮 / 连接 / 取消。
        if let Some(index) = Tabs::selection(child_key(owner, Act::Tabs.local()), 2, command) {
            self.tab = index;
            self.shell_open = false;
            ctx.mark_dirty();
            return true;
        }
        match command.key::<Act>(owner) {
            Some(Act::Shell) => {
                self.shell_open = !self.shell_open;
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
            Some(Act::SftpClose) => {
                if let Ok(mut shared) = self.shared.lock() {
                    shared.sftp.open = false;
                    shared.sftp.notice.clear();
                }
                let _ = self.actions.send(UiAction::SftpClose);
                ctx.mark_dirty();
                return true;
            }
            _ => {}
        }

        false
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
        children.push(Row::new().gap(6.0).view(theme).with_children([
            Button::new("上传").with_key(Act::SftpUpload).view(theme),
            Button::new("下载").with_key(Act::SftpDownload).view(theme),
            Button::new("关闭").with_key(Act::SftpClose).view(theme),
        ]));
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

    /// 连接参数：终端与 SFTP 都用它，避免两边字段不一致。
    fn auth_options(&self) -> AuthOptions {
        let identity = self.identity.trim();
        AuthOptions {
            host: self.host.clone(),
            port: self.port.parse().unwrap_or(22),
            user: self.user.clone(),
            password: self.password.clone(),
            identity: (!identity.is_empty()).then(|| identity.to_owned()),
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
            // 会话 > 清屏
            (1, 0) => {
                let _ = self.actions.send(UiAction::Clear);
            }
            // 会话 > 断开
            (1, 1) => {
                let _ = self.actions.send(UiAction::Disconnect);
            }
            // 会话 > SFTP 侧边栏
            (1, 2) => {
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
            (2, 0) => {
                self.set_label("lieterm 0.1 · zzzui 终端试作");
                ctx.mark_dirty();
            }
            _ => {}
        }
    }

    fn connect(&mut self, ctx: &mut UpdateCtx) {
        self.shell_open = false;
        self.open_menu = None;
        let mut shared = self.shared.lock().expect("shared lock");
        if self.tab == 0 {
            let command = SHELLS[self.shell.min(SHELLS.len() - 1)].1;
            let _ = self.actions.send(UiAction::ConnectLocal {
                shell: resolve_shell(command),
            });
        } else {
            let (input_tx, input_rx) = unbounded_channel();
            if let Ok(mut slot) = shared.input.lock() {
                *slot = Some(input_tx);
            }
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
            let _ = self.actions.send(UiAction::Connect {
                options,
                input: input_rx,
            });
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
        // 终端应用固定深色菜单栏：与终端背景/状态栏一致，也不受主题切换影响。
        row.visual.background = Color::rgb(0x2b, 0x2b, 0x2b);
        row.visual.border_color = Color::rgb(0x3a, 0x3a, 0x3a);

        let mut bar = ViewNode::new(Decl::div(child_key(ROOT_KEY, MENUBAR_SLOT)).with_style(row));
        for (index, (label, _)) in MENUS.iter().enumerate() {
            bar = bar.child(self.menu_title(theme, index, label));
        }
        bar
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
        style.visual.corner_radius = Corners::all(4.0);
        style.visual.background = if active {
            Color::rgb(0x3a, 0x3a, 0x3a)
        } else {
            Color::TRANSPARENT
        };
        style.visual.color = if active {
            theme.brand(Shade::Base)
        } else {
            Color::rgb(0xdd, 0xdd, 0xdd)
        };
        if !active {
            style.visual = style.visual.with_override(
                InteractionState::Hovered,
                VisualOverrides {
                    background: Some(Color::rgb(0x3a, 0x3a, 0x3a)),
                    ..VisualOverrides::default()
                },
            );
        }
        ViewNode::new(Decl::div(child_key(ROOT_KEY, act.local())).with_style(style))
            .with_slot(Some(act.local()))
            .child(passthrough(Label::new(label).view(theme)))
    }

    fn status_view(&self, theme: &UiTheme, shared: &Shared) -> ViewNode {
        Column::new()
            .height(STATUS_HEIGHT)
            .background(Color::rgb(0x28, 0x28, 0x28))
            .padding_edges(Edges::new(5.0, 8.0, 0.0, 8.0))
            .view(theme)
            .with_children([Label::new(shared.status.clone())
                .size(13.0)
                .color(Color::rgb(0xd0, 0xd0, 0xd0))
                .view(theme)])
    }

    /// 下拉框外观的 shell 选择器（点击展开 `Menu` 浮层）。
    ///
    /// 用 `with_slot` 而不是裸 `child_key`：浮层内容的 key 重绑只认具名槽位，
    /// 否则它会被当成匿名节点、父级无法用 `Act::Shell` 认领。
    fn shell_select(&self, theme: &UiTheme) -> ViewNode {
        let key = child_key(ROOT_KEY, Act::Shell.local());
        let active = self.shell_open;
        let mut style = Style::default();
        style.layout.direction = FlexDirection::Row;
        style.layout.align_items = AlignItems::Center;
        style.layout.justify_content = JustifyContent::SpaceBetween;
        style.layout.height = Length::px(32.0);
        style.layout.padding = Edges::new(0.0, 10.0, 0.0, 10.0);
        style.layout.border = Edges::all(1.0);
        style.visual.corner_radius = Corners::all(3.0);
        style.visual.background = theme.bg(BgToken::SpecialComponent);
        style.visual.border_color = if active {
            theme.brand(Shade::Base)
        } else {
            theme.border(BorderToken::Level2)
        };
        style.visual.cursor = Cursor::Pointer;
        if !active {
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
        ViewNode::new(Decl::div(key).with_style(style))
            .with_slot(Some(Act::Shell.local()))
            .child(passthrough(
                Label::new(SHELLS[self.shell.min(SHELLS.len() - 1)].0).view(theme),
            ))
            .child(passthrough(ViewNode::new(arrow.with_style(arrow_style))))
    }

    fn dialog(&self, owner: NodeKey, theme: &UiTheme, shared: &Shared) -> OverlayView {
        let mut body = vec![
            Tabs::new(["本地终端", "SSH"])
                .selected(self.tab)
                .with_key(Act::Tabs)
                .view(theme),
        ];

        if self.tab == 0 {
            body.push(Column::new().gap(8.0).view(theme).with_children([
                Label::new("本地 Shell").view(theme),
                self.shell_select(theme),
            ]));
        } else {
            body.push(
                Column::new().gap(8.0).view(theme).with_children([
                    Row::new().gap(8.0).view(theme).with_children([
                        Label::new("主机").view(theme),
                        TextField::new(self.host.clone())
                            .width(300.0)
                            .with_key(Act::Host)
                            .view(theme),
                    ]),
                    Row::new().gap(8.0).view(theme).with_children([
                        Label::new("端口").view(theme),
                        TextField::new(self.port.clone())
                            .width(300.0)
                            .with_key(Act::Port)
                            .view(theme),
                    ]),
                    Row::new().gap(8.0).view(theme).with_children([
                        Label::new("用户").view(theme),
                        TextField::new(self.user.clone())
                            .width(300.0)
                            .with_key(Act::User)
                            .view(theme),
                    ]),
                    Row::new().gap(8.0).view(theme).with_children([
                        Label::new("密码").view(theme),
                        TextField::new(self.password.clone())
                            .width(300.0)
                            .password(true)
                            .with_key(Act::Password)
                            .view(theme),
                    ]),
                    Row::new().gap(8.0).view(theme).with_children([
                        Label::new("密钥").view(theme),
                        TextField::new(self.identity.clone())
                            .width(300.0)
                            .with_key(Act::Identity)
                            .view(theme),
                    ]),
                    Label::new(
                        "密钥可留空：自动尝试 ~/.ssh/id_ed25519 等，或由 ~/.ssh/config 提供",
                    )
                    .size(12.0)
                    .color(theme.text(TextToken::Secondary))
                    .view(theme),
                ]),
            );
        }

        // 连接失败后对话框重新打开，空页上的提示会被遮罩盖住：把原因放进对话框。
        if shared.screen == Screen::Failed && !shared.label.is_empty() {
            body.push(
                Label::new(shared.label.clone())
                    .size(12.0)
                    .color(Color::rgb(0xd5, 0x49, 0x49))
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
/// 把命令名解析成可执行路径（找不到时原样返回，由 `local::spawn` 报错）。
fn resolve_shell(command: &str) -> String {
    let direct = std::path::Path::new(command);
    if direct.is_file() {
        return command.to_owned();
    }
    let Some(paths) = std::env::var_os("PATH") else {
        return command.to_owned();
    };
    std::env::split_paths(&paths)
        .map(|dir| dir.join(command))
        .find(|candidate| candidate.is_file())
        .map_or_else(
            || command.to_owned(),
            |path| path.to_string_lossy().into_owned(),
        )
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

pub struct LietermApp {
    shared: Arc<Mutex<Shared>>,
    actions_tx: std_mpsc::Sender<UiAction>,
    actions_rx: std_mpsc::Receiver<UiAction>,
    events_rx: UnboundedReceiver<SshEvent>,
    /// 当前 SFTP 任务的请求通道（`None` 表示未打开）。
    sftp_actions: Option<UnboundedSender<SftpAction>>,
    bridge: Bridge,
    terminal: Terminal,
    runtime: tokio::runtime::Handle,
    redraw: Arc<Mutex<Option<(WindowOpener, WindowId)>>>,
    last_viewport: (f64, f64),
    focus_pending: bool,
    /// 远程标题同步（只在变化时调 `set_title`）。
    last_title: String,
    cursor_visible: bool,
    next_blink: Instant,
    /// `LIETERM_LOCAL` 指定的本地 shell：首帧自动进入会话（试验 / 演示用）。
    pending_local: Option<String>,
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
        let bridge = Bridge::new(events_tx, Arc::clone(&redraw));
        Self {
            shared,
            actions_tx,
            actions_rx,
            events_rx,
            sftp_actions: None,
            bridge,
            terminal,
            runtime,
            redraw,
            last_viewport: (0.0, 0.0),
            focus_pending: false,
            last_title: String::new(),
            cursor_visible: true,
            next_blink: Instant::now() + Duration::from_millis(530),
            pending_local,
        }
    }

    fn label(&mut self, text: impl Into<String>) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.label = text.into();
        }
    }

    fn refresh_status(&mut self) {
        let label = self
            .shared
            .lock()
            .map(|shared| shared.label.clone())
            .unwrap_or_default();
        let status = format!(
            "{}  |  {}×{}  |  {}",
            self.terminal.title(),
            self.terminal.cols(),
            self.terminal.rows(),
            label
        );
        if let Ok(mut shared) = self.shared.lock() {
            shared.status = status;
        }
    }

    /// 启动本地 shell：与 SSH 共用 input 通道与事件，只是不再经过网络。
    fn start_local(&mut self, shell: String, ctx: &mut WindowContext<'_>) {
        let (input_tx, input_rx) = unbounded_channel();
        if let Ok(shared) = self.shared.lock()
            && let Ok(mut slot) = shared.input.lock()
        {
            *slot = Some(input_tx.clone());
        }
        self.label(format!("正在启动 {shell}…"));
        if let Ok(mut shared) = self.shared.lock() {
            shared.screen = Screen::Connecting;
            shared.dialog_open = false;
        }
        local::spawn(
            &shell,
            u16::try_from(self.terminal.cols()).unwrap_or(u16::MAX),
            u16::try_from(self.terminal.rows()).unwrap_or(u16::MAX),
            input_rx,
            self.bridge.clone(),
        );
        // 试验模式：启动后自动下发一条命令，方便无键盘交互地验证渲染链路。
        if let Ok(command) = std::env::var("LIETERM_LOCAL_CMD") {
            let _ = input_tx.send(UiEvent::Input(format!("{command}\r\n").into_bytes()));
        }
        ctx.invalidate();
    }

    fn disconnect(&mut self) {
        if let Some(actions) = self.sftp_actions.take() {
            let _ = actions.send(SftpAction::Close);
        }
        if let Ok(shared) = self.shared.lock()
            && let Ok(mut slot) = shared.input.lock()
        {
            *slot = None;
        }
        if let Ok(mut shared) = self.shared.lock() {
            shared.screen = Screen::Idle;
            shared.dialog_open = true;
            shared.label = String::new();
            shared.sftp = SftpState::default();
        }
        self.terminal.advance(b"\x1b[2J\x1b[H");
        self.focus_pending = false;
    }
}

impl AppHandler for LietermApp {
    fn build_ui(&mut self, ctx: &mut WindowContext<'_>) {
        let root = Root {
            shared: Arc::clone(&self.shared),
            actions: self.actions_tx.clone(),
            host: String::new(),
            port: "22".to_owned(),
            user: String::new(),
            password: String::new(),
            identity: String::new(),
            tab: 0,
            shell: 0,
            open_menu: None,
            shell_open: false,
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
                UiAction::Connect { mut options, input } => {
                    options.cols = self.terminal.cols() as u32;
                    options.rows = self.terminal.rows() as u32;
                    self.label(format!(
                        "正在连接 {}@{}:{}…",
                        options.user, options.host, options.port
                    ));
                    if let Ok(mut shared) = self.shared.lock() {
                        shared.screen = Screen::Connecting;
                        shared.dialog_open = false;
                    }
                    ssh::spawn(&self.runtime, options, input, self.bridge.clone());
                    dirty = true;
                }
                UiAction::ConnectLocal { shell } => {
                    self.start_local(shell, ctx);
                    dirty = true;
                }
                UiAction::Clear => {
                    self.terminal.advance(b"\x1b[2J\x1b[H");
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
                    let lines = (-(delta / term::CELL_HEIGHT)).round() as i32;
                    if lines != 0 {
                        self.terminal.scroll(lines);
                        dirty = true;
                    }
                }
                UiAction::SftpOpen { options } => {
                    let (actions, rx) = unbounded_channel();
                    sftp::spawn(&self.runtime, options, rx, self.bridge.clone());
                    self.sftp_actions = Some(actions);
                    dirty = true;
                }
                UiAction::SftpAction(action) => {
                    if let Some(actions) = &self.sftp_actions {
                        let _ = actions.send(action);
                    }
                }
                UiAction::SftpClose => {
                    if let Some(actions) = self.sftp_actions.take() {
                        let _ = actions.send(SftpAction::Close);
                    }
                    dirty = true;
                }
            }
        }

        while let Ok(event) = self.events_rx.try_recv() {
            match event {
                SshEvent::Connected(label) => {
                    self.label(label);
                    if let Ok(mut shared) = self.shared.lock() {
                        shared.screen = Screen::Session;
                        shared.dialog_open = false;
                    }
                    self.focus_pending = true;
                    dirty = true;
                }
                SshEvent::Data(bytes) => {
                    self.terminal.advance(&bytes);
                    dirty = true;
                }
                SshEvent::Error(message) => {
                    self.label(message);
                    if let Ok(mut shared) = self.shared.lock() {
                        shared.screen = Screen::Failed;
                        shared.dialog_open = true;
                    }
                    dirty = true;
                }
                SshEvent::Closed => {
                    if let Ok(mut shared) = self.shared.lock()
                        && shared.screen == Screen::Session
                    {
                        shared.label.push_str("  (连接已关闭)");
                    }
                    dirty = true;
                }
                SshEvent::SftpReady { path, entries } => {
                    if let Ok(mut shared) = self.shared.lock() {
                        shared.sftp.path = path.clone();
                        shared.sftp.entries = entries;
                        shared.sftp.selected = None;
                        shared.sftp.notice = format!("SFTP 目录：{path}");
                    }
                    dirty = true;
                }
                SshEvent::SftpListing { path, entries } => {
                    if let Ok(mut shared) = self.shared.lock() {
                        shared.sftp.path = path.clone();
                        shared.sftp.selected = None;
                        shared.sftp.notice =
                            format!("{}（{} 项）", path, entries.len().saturating_sub(1));
                        shared.sftp.entries = entries;
                    }
                    dirty = true;
                }
                SshEvent::SftpNotice(message) => {
                    if let Ok(mut shared) = self.shared.lock() {
                        shared.sftp.notice = message;
                    }
                    dirty = true;
                }
                SshEvent::SftpError(message) => {
                    if let Ok(mut shared) = self.shared.lock() {
                        shared.sftp.notice = format!("错误：{message}");
                    }
                    dirty = true;
                }
            }
        }

        // 远程标题（OSC 0/2）→ 窗口标题；只在变化时下发。
        let title = self.terminal.title();
        if title != self.last_title {
            self.last_title = title.clone();
            ctx.set_title(&format!("{title} — lieterm"));
        }

        // 终端光标是自绘的：用 0.5s 周期的重绘驱动闪烁（没有光标也算一帧静止）。
        let in_session = self
            .shared
            .lock()
            .map(|shared| shared.screen == Screen::Session)
            .unwrap_or(false);
        if in_session {
            let now = Instant::now();
            if now >= self.next_blink {
                self.cursor_visible = !self.cursor_visible;
                self.terminal.set_cursor_visible(self.cursor_visible);
                self.next_blink = now + Duration::from_millis(530);
                dirty = true;
            }
            // 宿主级定时帧：到点由平台自动重绘，不再依赖后台线程 `redraw`。
            ctx.wake_in((self.next_blink - now).as_secs_f64().max(0.0));
        }

        let viewport = ctx.runtime.viewport();
        let scale = ctx.runtime.scale();
        let sftp_inset = if self
            .shared
            .lock()
            .map(|shared| shared.sftp.open)
            .unwrap_or(false)
        {
            SIDEBAR_WIDTH
        } else {
            0.0
        };
        let term_width = (viewport.width - sftp_inset).max(1.0);
        let term_height = (viewport.height - MENUBAR_HEIGHT - STATUS_HEIGHT).max(1.0);
        if (term_width, term_height) != self.last_viewport {
            self.last_viewport = (term_width, term_height);
            let cols = ((term_width - 8.0) / self.terminal.cell_width()).floor() as usize;
            let cols = cols.clamp(20, 400);
            let rows = (term_height / term::CELL_HEIGHT).floor() as usize;
            let rows = rows.clamp(6, 200);
            self.terminal.resize(cols, rows);
            if let Ok(mut shared) = self.shared.lock() {
                shared.width = viewport.width;
                shared.height = viewport.height;
            }
            let px_width = (term_width * scale).round().max(1.0) as u32;
            let px_height = (term_height * scale).round().max(1.0) as u32;
            if let Ok(shared) = self.shared.lock()
                && let Ok(slot) = shared.input.lock()
                && let Some(sender) = slot.as_ref()
            {
                let _ = sender.send(UiEvent::Resize {
                    cols: cols as u32,
                    rows: rows as u32,
                    px_width,
                    px_height,
                });
            }
            dirty = true;
        }

        if dirty {
            let px_width = ctx.runtime.frame().width();
            let px_height = ctx.runtime.frame().height();
            let render_start = Instant::now();
            self.terminal.render(
                px_width,
                px_height,
                scale,
                MENUBAR_HEIGHT * scale,
                sftp_inset * scale,
            );
            let render_ms = render_start.elapsed().as_secs_f64() * 1000.0;
            if std::env::var_os("LIETERM_TRACE").is_some() {
                let image = self.terminal.image();
                eprintln!(
                    "[lieterm] frame={}x{} image={:?} received={} screen_lines={} cols={} rows={} render={render_ms:.2}ms",
                    px_width,
                    px_height,
                    image.as_ref().map(|image| (image.width, image.height)),
                    self.terminal.received(),
                    self.terminal.screen_lines(),
                    self.terminal.cols(),
                    self.terminal.rows()
                );
            }
            // 整窗底图 + 状态栏声明；像素与声明各自报告失效。
            ctx.runtime.set_background_image(self.terminal.image());
            self.refresh_status();
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
