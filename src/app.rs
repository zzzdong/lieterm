//! zzzui 应用层：登录面板 + 终端画面 + 后台 SSH 事件泵。

use std::sync::{Arc, Mutex, mpsc as std_mpsc};

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use zzzui::{
    element::{
        AlignItems, Decl, ElementId, ElementStore, ImageData, JustifyContent, Length, PositionType,
        ROOT_KEY, RawCommand, Style,
    },
    geom::Edges,
    platform::{AppHandler, WindowContext, WindowId, WindowOpener},
    view::ViewNode,
    widget::{Button, Column, Label, Row, TextField, UiTheme, UpdateCtx, Widget},
};

use crate::{
    keys,
    ssh::{self, ConnectOptions},
    term::{self, STATUS_HEIGHT, Terminal},
};

zzzui::key_enum! {
    enum Act {
        Host,
        Port,
        User,
        Password,
        Connect,
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
}

pub enum UiAction {
    Connect {
        options: ConnectOptions,
        input: UnboundedReceiver<UiEvent>,
    },
    Scroll(f64),
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    #[default]
    Login,
    Connecting,
    Session,
    Failed,
}

type InputSlot = Arc<Mutex<Option<UnboundedSender<UiEvent>>>>;

#[derive(Default)]
pub struct Shared {
    pub screen: Screen,
    pub width: f64,
    pub height: f64,
    /// 连接状态原文（不含尺寸装饰）。
    pub label: String,
    /// 状态栏最终显示文本。
    pub status: String,
    pub input: InputSlot,
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
}

struct Root {
    shared: Arc<Mutex<Shared>>,
    image: Arc<Mutex<Option<ImageData>>>,
    actions: std_mpsc::Sender<UiAction>,
    host: String,
    port: String,
    user: String,
    password: String,
}

impl Widget for Root {
    fn view(&self, theme: &UiTheme) -> ViewNode {
        let shared = self.shared.lock().expect("shared lock");
        if shared.screen == Screen::Session {
            self.session_view(&shared)
        } else {
            self.login_view(theme, &shared)
        }
    }

    fn on_command(
        &mut self,
        owner: zzzui::element::NodeKey,
        command: &RawCommand,
        ctx: &mut UpdateCtx,
    ) -> bool {
        match command {
            RawCommand::TextChanged { value, .. } => {
                match command.key::<Act>(owner) {
                    Some(Act::Host) => self.host = value.to_string(),
                    Some(Act::Port) => self.port = value.to_string(),
                    Some(Act::User) => self.user = value.to_string(),
                    Some(Act::Password) => self.password = value.to_string(),
                    _ => return false,
                }
                ctx.mark_dirty();
                return true;
            }
            RawCommand::Key {
                name,
                text,
                ctrl,
                shift,
                alt,
                ..
            } => {
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
            RawCommand::Wheel { delta, .. } => {
                let _ = self.actions.send(UiAction::Scroll(*delta));
                return true;
            }
            RawCommand::Click { .. } if command.key::<Act>(owner) == Some(Act::Connect) => {
                let port = self.port.parse::<u16>().unwrap_or(22);
                let options = ConnectOptions {
                    host: self.host.clone(),
                    port,
                    user: self.user.clone(),
                    password: self.password.clone(),
                    cols: 80,
                    rows: 24,
                };
                let (input_tx, input_rx) = unbounded_channel();
                if let Ok(shared) = self.shared.lock()
                    && let Ok(mut slot) = shared.input.lock()
                {
                    *slot = Some(input_tx);
                }
                let _ = self.actions.send(UiAction::Connect {
                    options,
                    input: input_rx,
                });
                return true;
            }
            _ => {}
        }
        false
    }
}

impl Root {
    fn login_view(&self, theme: &UiTheme, shared: &Shared) -> ViewNode {
        let status = if shared.label.is_empty() {
            "输入主机与用户名后点击连接".to_owned()
        } else {
            shared.label.clone()
        };
        let form = Column::new()
            .gap(10.0)
            .padding(24.0)
            .width(440.0)
            .view(theme)
            .with_children([
                Label::new("lieterm — SSH 终端").size(20.0).view(theme),
                Row::new().gap(8.0).view(theme).with_children([
                    Label::new("主机").view(theme),
                    TextField::new(self.host.clone())
                        .width(280.0)
                        .with_key(Act::Host)
                        .view(theme),
                ]),
                Row::new().gap(8.0).view(theme).with_children([
                    Label::new("端口").view(theme),
                    TextField::new(self.port.clone())
                        .width(280.0)
                        .with_key(Act::Port)
                        .view(theme),
                ]),
                Row::new().gap(8.0).view(theme).with_children([
                    Label::new("用户").view(theme),
                    TextField::new(self.user.clone())
                        .width(280.0)
                        .with_key(Act::User)
                        .view(theme),
                ]),
                Row::new().gap(8.0).view(theme).with_children([
                    Label::new("密码").view(theme),
                    TextField::new(self.password.clone())
                        .width(280.0)
                        .password(true)
                        .with_key(Act::Password)
                        .view(theme),
                ]),
                Button::new("连接").with_key(Act::Connect).view(theme),
                Label::new(status).view(theme),
            ]);
        let mut center_style = Style::default();
        center_style.layout.justify_content = JustifyContent::Center;
        center_style.layout.align_items = AlignItems::Center;
        center_style.layout.flex_grow = 1.0;
        ViewNode::new(Decl::div(ROOT_KEY).with_style(center_style)).child(form)
    }

    fn session_view(&self, shared: &Shared) -> ViewNode {
        let width = shared.width.max(1.0);
        let height = shared.height.max(1.0);
        let area_height = (height - STATUS_HEIGHT).max(1.0);
        let image = self
            .image
            .lock()
            .expect("image lock")
            .clone()
            .or_else(|| ImageData::new(1, 1, vec![0x1c, 0x1c, 0x1c, 0xff]))
            .expect("1x1 占位位图合法");

        let mut root_style = Style::default();
        root_style.layout.width = Length::px(width);
        root_style.layout.height = Length::px(height);

        let mut area_style = Style::default();
        area_style.layout.width = Length::px(width);
        area_style.layout.height = Length::px(area_height);

        let mut image_style = Style::default();
        image_style.layout.width = Length::px(width);
        image_style.layout.height = Length::px(area_height);

        // 透明输入框盖在整个终端区域上：它只借焦点与键盘，不画自己的文字。
        let mut capture_style = Style::default();
        capture_style.layout.position = PositionType::Absolute;
        capture_style.layout.inset = Edges::new(0.0, 0.0, 0.0, 0.0);

        let mut status_style = Style::default();
        status_style.layout.height = Length::px(STATUS_HEIGHT);
        status_style.visual.background = zzzui::geom::Color::rgb(0x28, 0x28, 0x28);
        status_style.visual.color = zzzui::geom::Color::rgb(0xd0, 0xd0, 0xd0);
        status_style.layout.padding = Edges::new(5.0, 8.0, 0.0, 8.0);

        ViewNode::new(Decl::div(ROOT_KEY).with_style(root_style)).with_children([
            ViewNode::new(Decl::div(ROOT_KEY + 1).with_style(area_style)).with_children([
                ViewNode::new(Decl::image(ROOT_KEY + 2, image).with_style(image_style)),
                ViewNode::new(
                    Decl::text_input(ROOT_KEY + 3, "")
                        .as_key_capture()
                        .with_style(capture_style),
                ),
            ]),
            ViewNode::new(Decl::text(ROOT_KEY + 4, shared.status.clone()).with_style(status_style)),
        ])
    }
}

pub struct LietermApp {
    shared: Arc<Mutex<Shared>>,
    image: Arc<Mutex<Option<ImageData>>>,
    actions_tx: std_mpsc::Sender<UiAction>,
    actions_rx: std_mpsc::Receiver<UiAction>,
    events_rx: UnboundedReceiver<SshEvent>,
    bridge: Bridge,
    terminal: Terminal,
    runtime: tokio::runtime::Handle,
    redraw: Arc<Mutex<Option<(WindowOpener, WindowId)>>>,
    cell_width: f64,
    last_viewport: (f64, f64),
    focus_pending: bool,
}

impl LietermApp {
    pub fn new(terminal: Terminal, cell_width: f64, runtime: tokio::runtime::Handle) -> Self {
        let shared = Arc::new(Mutex::new(Shared {
            width: 800.0,
            height: 600.0,
            ..Shared::default()
        }));
        let image = Arc::new(Mutex::new(None));
        let (actions_tx, actions_rx) = std_mpsc::channel();
        let (events_tx, events_rx) = unbounded_channel();
        let redraw = Arc::new(Mutex::new(None));
        let bridge = Bridge::new(events_tx, Arc::clone(&redraw));
        Self {
            shared,
            image,
            actions_tx,
            actions_rx,
            events_rx,
            bridge,
            terminal,
            runtime,
            redraw,
            cell_width,
            last_viewport: (0.0, 0.0),
            focus_pending: false,
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
}

impl AppHandler for LietermApp {
    fn build_ui(&mut self, ctx: &mut WindowContext<'_>) {
        let root = Root {
            shared: Arc::clone(&self.shared),
            image: Arc::clone(&self.image),
            actions: self.actions_tx.clone(),
            host: String::new(),
            port: "22".to_owned(),
            user: String::new(),
            password: String::new(),
        };
        ctx.runtime.app_mut().mount(root);
    }

    fn window_opened(&mut self, ctx: &mut WindowContext<'_>) {
        if let Ok(mut slot) = self.redraw.lock() {
            *slot = Some((ctx.opener.clone(), ctx.id));
        }
    }

    fn on_frame(&mut self, ctx: &mut WindowContext<'_>) {
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
                    }
                    ssh::spawn(&self.runtime, options, input, self.bridge.clone());
                    dirty = true;
                }
                UiAction::Scroll(delta) => {
                    let lines = (-(delta / term::CELL_HEIGHT)).round() as i32;
                    if lines != 0 {
                        self.terminal.scroll(lines);
                        dirty = true;
                    }
                }
            }
        }

        while let Ok(event) = self.events_rx.try_recv() {
            match event {
                SshEvent::Connected(label) => {
                    self.label(label);
                    if let Ok(mut shared) = self.shared.lock() {
                        shared.screen = Screen::Session;
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
            }
        }

        let viewport = ctx.runtime.viewport();
        let size = (viewport.width, viewport.height);
        if size != self.last_viewport {
            self.last_viewport = size;
            let cols = (((viewport.width - 8.0) / self.cell_width).floor() as usize).clamp(20, 400);
            let rows = (((viewport.height - STATUS_HEIGHT) / term::CELL_HEIGHT).floor() as usize)
                .clamp(6, 200);
            self.terminal.resize(cols, rows);
            if let Ok(mut shared) = self.shared.lock() {
                shared.width = viewport.width;
                shared.height = viewport.height;
            }
            let px_width = ctx.runtime.frame().width();
            let px_height = ctx.runtime.frame().height();
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

        if self.focus_pending
            && let Some(id) = find_capture(ctx.runtime.store(), ctx.runtime.store().root())
        {
            ctx.runtime.focus(id);
            self.focus_pending = false;
        } else if self.focus_pending {
            // 画面在本帧末尾才重建：再排一帧，等捕获面实体出现后再聚焦。
            ctx.opener.redraw(ctx.id);
        }

        if dirty {
            let px_width = ctx.runtime.frame().width();
            let px_height = ctx.runtime.frame().height();
            let scale = if viewport.width > 0.0 {
                px_width as f64 / viewport.width
            } else {
                1.0
            };
            self.terminal.render(px_width, px_height, scale);
            *self.image.lock().expect("image lock") = self.terminal.image();
            self.refresh_status();
        }
    }
}

fn find_capture(store: &ElementStore, id: ElementId) -> Option<ElementId> {
    if matches!(store.get(id)?.decl, Decl::TextInput { passive: true, .. }) {
        return Some(id);
    }
    store
        .children(id)
        .iter()
        .find_map(|child| find_capture(store, *child))
}
