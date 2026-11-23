//! 连接对话框子组件：自己声明浮层、处理浮层内命令，通过事件把业务动作上报给 Root。
//!
//! Root 仍是表单状态的真相源，每帧把状态同步到 `form` 快照供本组件渲染；浮层里的
//! 点击 / 输入由本组件认领后 `emit` 给 Root。这样浮层归属、命令路由都在子组件里，
//! 而业务状态不需要跨组件复制多份。

use std::sync::{Arc, Mutex};

use zzzui::{
    element::{
        AlignItems, Cursor, Decl, FlexDirection, InteractionState, JustifyContent, Length, NodeKey,
        PointerEvents, ROOT_KEY, RawCommand, Style, VisualOverrides, child_key,
    },
    geom::{Corners, Edges},
    view::{OverlayView, ViewNode},
    widget::{
        BgToken, BorderToken, Button, Center, Column, Dialog, Family, Key, Label, Menu, Row, Shade,
        Tabs, TextField, TextToken, UiTheme, UpdateCtx, Widget,
    },
};

use crate::app::{
    AUTH_OPTIONS, Act, AUTH_POPUP_SLOT, ConnectForm, DIALOG_SLOT, FIELD_WIDTH, LABEL_WIDTH,
    SHELL_POPUP_SLOT,
};

/// 浮层里的表单输入：交给 Root 写回它自己的字段。
pub struct FormEdit {
    pub act: Act,
    pub value: String,
}

/// 浮层关闭请求。
pub struct CloseDialog;

/// 用户在对话框里点了「连接」。
pub struct ConnectRequested;

/// 下拉 / 标签选择；用 `act` 区分是哪一类。
pub struct Choice {
    pub act: Act,
    pub index: usize,
}

/// 某个下拉按钮被点击（展开 / 收起）。
pub struct Toggle {
    pub act: Act,
}

/// 纯装饰子节点不参与命中，点击穿透到父级。
fn passthrough(mut node: ViewNode) -> ViewNode {
    match &mut node.decl {
        Decl::Text { style, .. } | Decl::Icon { style, .. } => {
            style.visual.pointer_events = PointerEvents::None;
        }
        _ => {}
    }
    node
}

/// 连接对话框宿主；`form` 是 Root 每帧同步进来的只读快照。
pub struct ConnectHost {
    form: Arc<Mutex<ConnectForm>>,
}

impl ConnectHost {
    pub fn new(form: Arc<Mutex<ConnectForm>>) -> Self {
        Self { form }
    }

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

    fn form_label(text: &str, theme: &UiTheme) -> ViewNode {
        let mut style = Style::default();
        style.layout.direction = FlexDirection::Row;
        style.layout.align_items = AlignItems::Center;
        style.layout.justify_content = JustifyContent::End;
        style.layout.width = Length::px(LABEL_WIDTH);
        style.layout.height = Length::px(32.0);
        ViewNode::new(Decl::div(ROOT_KEY).with_style(style)).child(Label::new(text).view(theme))
    }

    fn dialog(&self, owner: NodeKey, theme: &UiTheme, form: &ConnectForm) -> OverlayView {
        let mut body = vec![
            Tabs::new(["本地终端", "SSH"])
                .selected(form.tab)
                .with_key(Act::Tabs)
                .view(theme),
        ];

        if form.tab == 0 {
            let shells: Vec<&str> = form.shells.iter().map(|(name, _)| name.as_str()).collect();
            let fields = Column::new()
                .gap(8.0)
                .width(FIELD_WIDTH)
                .view(theme)
                .with_children([
                    Label::new("本地 Shell").view(theme),
                    self.select_field(theme, Act::Shell, &shells, form.shell, form.shell_open),
                ]);
            body.push(Center::new().view(theme).with_children([fields]));
        } else {
            let mut ssh_fields = vec![
                Row::new().gap(8.0).view(theme).with_children([
                    Self::form_label("主机", theme),
                    TextField::new(form.host.clone())
                        .width(FIELD_WIDTH)
                        .with_key(Act::Host)
                        .view(theme),
                ]),
                Row::new().gap(8.0).view(theme).with_children([
                    Self::form_label("端口", theme),
                    TextField::new(form.port.clone())
                        .width(FIELD_WIDTH)
                        .with_key(Act::Port)
                        .view(theme),
                ]),
                Row::new().gap(8.0).view(theme).with_children([
                    Self::form_label("用户", theme),
                    TextField::new(form.user.clone())
                        .width(FIELD_WIDTH)
                        .with_key(Act::User)
                        .view(theme),
                ]),
                Row::new().gap(8.0).view(theme).with_children([
                    Self::form_label("认证方式", theme),
                    self.select_field(theme, Act::AuthMode, AUTH_OPTIONS, form.auth, form.auth_open),
                ]),
            ];
            if form.auth == 0 {
                ssh_fields.push(
                    Row::new().gap(8.0).view(theme).with_children([
                        Self::form_label("密码", theme),
                        TextField::new(form.password.clone())
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
                        TextField::new(form.identity.clone())
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
            let fields = Column::new()
                .gap(8.0)
                .width(LABEL_WIDTH + 8.0 + FIELD_WIDTH)
                .view(theme)
                .with_children(ssh_fields);
            body.push(Center::new().view(theme).with_children([fields]));
        }

        // 连接失败原因在快照的 label 里。
        if let Some(reason) = form.failure.as_deref() {
            body.push(
                Label::new(reason)
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
}

impl Widget for ConnectHost {
    fn view(&self, _theme: &UiTheme) -> ViewNode {
        let mut style = Style::default();
        style.layout.width = Length::px(0.0);
        style.layout.height = Length::px(0.0);
        ViewNode::new(Decl::div(ROOT_KEY).with_style(style))
    }

    fn overlays(&self, owner: NodeKey, theme: &UiTheme) -> Vec<OverlayView> {
        let form = self.form.lock().expect("form lock");
        if !form.dialog_open {
            return Vec::new();
        }
        let mut out = vec![self.dialog(owner, theme, &form)];
        if form.shell_open {
            let shells: Vec<&str> = form.shells.iter().map(|(name, _)| name.as_str()).collect();
            out.push(
                Menu::new(shells)
                    .anchored(child_key(owner, Act::Shell.local()))
                    .key(child_key(owner, SHELL_POPUP_SLOT))
                    .selected(form.shell)
                    .width(FIELD_WIDTH)
                    .build(owner, theme),
            );
        }
        if form.auth_open && form.tab == 1 {
            out.push(
                Menu::new(AUTH_OPTIONS.iter().copied())
                    .anchored(child_key(owner, Act::AuthMode.local()))
                    .key(child_key(owner, AUTH_POPUP_SLOT))
                    .selected(form.auth)
                    .width(FIELD_WIDTH)
                    .build(owner, theme),
            );
        }
        out
    }

    fn on_command(&mut self, owner: NodeKey, command: &RawCommand, ctx: &mut UpdateCtx) -> bool {
        // 关闭请求：对话框 / 两个下拉。
        if let RawCommand::OverlayDismiss { key, .. } = command {
            let form = self.form.lock().expect("form lock");
            if *key == child_key(owner, DIALOG_SLOT) {
                ctx.emit(CloseDialog);
                return true;
            }
            if *key == child_key(owner, SHELL_POPUP_SLOT) && form.shell_open {
                ctx.emit(Choice { act: Act::Shell, index: form.shell });
                return true;
            }
            if *key == child_key(owner, AUTH_POPUP_SLOT) && form.auth_open {
                ctx.emit(Choice { act: Act::AuthMode, index: form.auth });
                return true;
            }
            return false;
        }

        // 文本输入
        if let RawCommand::TextChanged { value, .. } = command {
            let act = match command.key::<Act>(owner) {
                Some(a @ (Act::Host | Act::Port | Act::User | Act::Password | Act::Identity)) => a,
                _ => return false,
            };
            ctx.emit(FormEdit { act, value: value.to_string() });
            return true;
        }

        // 菜单选中
        let shell_len = self.form.lock().expect("form lock").shells.len();
        if let Some(index) = Menu::selection(child_key(owner, SHELL_POPUP_SLOT), shell_len, command) {
            ctx.emit(Choice { act: Act::Shell, index });
            return true;
        }
        if let Some(index) = Menu::selection(child_key(owner, AUTH_POPUP_SLOT), AUTH_OPTIONS.len(), command) {
            ctx.emit(Choice { act: Act::AuthMode, index });
            return true;
        }

        // Tabs
        if let Some(index) = Tabs::selection(child_key(owner, Act::Tabs.local()), 2, command) {
            ctx.emit(Choice { act: Act::Tabs, index });
            return true;
        }

        match command.key::<Act>(owner) {
            Some(a @ (Act::Shell | Act::AuthMode)) => {
                ctx.emit(Toggle { act: a });
                true
            }
            Some(Act::Connect) => {
                ctx.emit(ConnectRequested);
                true
            }
            Some(Act::Cancel) => {
                ctx.emit(CloseDialog);
                true
            }
            _ => false,
        }
    }
}
