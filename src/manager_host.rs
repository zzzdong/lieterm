//! 会话 / 宏管理浮层宿主。
//!
//! Root 仍是管理状态的真相源，每次重建声明时通过 `ChildDecl::props` 下发 `ManagerForm`；
//! 浮层里的点击 / 输入由本组件认领后 `emit` 给 Root。这样浮层归属与命令路由都在子组件里，
//! 而业务状态不需要跨组件复制多份。

use std::any::Any;

use zzzui::{
    element::{
        AlignItems, Cursor, Decl, FlexDirection, InteractionState, Length, LocalKey, NodeKey,
        PointerEvents, ROOT_KEY, RawCommand, Style, VisualOverrides, child_key,
    },
    geom::{Color, Edges},
    view::{OverlayView, ViewNode},
    widget::{
        BgToken, Button, Column, Dialog, Label, Row, Scroll, Shade, TextField, TextToken, UiTheme,
        UpdateCtx, Widget,
    },
};

use crate::app::{
    Act, MACRO_ENTRY_BASE, MACROS_SLOT, ManagerForm, SESSION_ENTRY_BASE, SESSIONS_SLOT,
    macro_entry_key, session_entry_key,
};

/// 区分两套管理浮层。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagerKind {
    Sessions,
    Macros,
}

/// 管理浮层上报给 Root 的事件。
pub enum ManagerEvent {
    /// 文本框输入。
    Edit { act: Act, value: String },
    /// 按钮动作。
    Action(Act),
    /// 列表行选中。
    Select { kind: ManagerKind, index: usize },
    /// 浮层关闭请求。
    Dismiss(ManagerKind),
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

/// 管理浮层宿主；`form` 是 Root 每次重建时通过 props 下发的只读参数。
pub struct ManagerHost {
    form: ManagerForm,
}

impl ManagerHost {
    pub fn new() -> Self {
        Self {
            form: ManagerForm::default(),
        }
    }

    fn session_manager(&self, owner: NodeKey, theme: &UiTheme, form: &ManagerForm) -> OverlayView {
        let mut rows = Vec::new();
        for (index, profile) in form.sessions.iter().enumerate().take(100) {
            let target = if profile.user.is_empty() {
                format!("{}  {}:{}", profile.name, profile.host, profile.port)
            } else {
                format!(
                    "{}  {}@{}:{}",
                    profile.name, profile.user, profile.host, profile.port
                )
            };
            rows.push(manager_row(
                theme,
                SESSION_ENTRY_BASE + index as LocalKey,
                form.session_selected == Some(index),
                target,
            ));
        }
        if rows.is_empty() {
            rows.push(manager_hint(theme, "还没有保存的会话"));
        }

        let content = Column::new().gap(10.0).view(theme).with_children([
            Row::new().gap(8.0).view(theme).with_children([
                Label::new("名称").view(theme),
                TextField::new(form.session_name.clone())
                    .width(340.0)
                    .with_key(Act::SessionName)
                    .view(theme),
            ]),
            Scroll::new().height(200.0).view(theme).with_children(rows),
            Label::new("保存当前：把上方 SSH 表单里的主机 / 端口 / 用户 / 密钥写进配置")
                .size(12.0)
                .color(theme.text(TextToken::Secondary))
                .view(theme),
            manager_notice_view(theme, &form.notice),
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

    fn macro_manager(&self, owner: NodeKey, theme: &UiTheme, form: &ManagerForm) -> OverlayView {
        let mut rows = Vec::new();
        for (index, entry) in form.macros.iter().enumerate().take(100) {
            let inline = entry.text.replace(['\n', '\r'], " ");
            let preview: String = inline.chars().take(36).collect();
            let suffix = if entry.enter { "  ↵" } else { "" };
            rows.push(manager_row(
                theme,
                MACRO_ENTRY_BASE + index as LocalKey,
                form.macro_selected == Some(index),
                format!("{}  {}{}", entry.name, preview, suffix),
            ));
        }
        if rows.is_empty() {
            rows.push(manager_hint(theme, "还没有宏，点「新建」开始"));
        }

        let content = Column::new().gap(10.0).view(theme).with_children([
            Row::new().gap(8.0).view(theme).with_children([
                Label::new("名称").view(theme),
                TextField::new(form.macro_name.clone())
                    .width(340.0)
                    .with_key(Act::MacroName)
                    .view(theme),
            ]),
            Row::new().gap(8.0).view(theme).with_children([
                Label::new("内容").view(theme),
                TextField::new(form.macro_text.clone())
                    .width(340.0)
                    .with_key(Act::MacroText)
                    .view(theme),
            ]),
            Row::new().gap(8.0).view(theme).with_children([
                Button::new(if form.macro_enter {
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
            manager_notice_view(theme, &form.notice),
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
}

impl Default for ManagerHost {
    fn default() -> Self {
        Self::new()
    }
}

impl Widget for ManagerHost {
    fn view(&self, _theme: &UiTheme) -> ViewNode {
        // 本组件只声明浮层，主树留一个不占空间的占位。
        ViewNode::new(Decl::div(ROOT_KEY))
    }

    fn set_props(&mut self, props: &dyn Any) {
        if let Some(form) = props.downcast_ref::<ManagerForm>() {
            self.form = form.clone();
        }
    }

    fn overlays(&self, owner: NodeKey, theme: &UiTheme) -> Vec<OverlayView> {
        let form = &self.form;
        let mut out = Vec::new();
        if form.sessions_open {
            out.push(self.session_manager(owner, theme, form));
        }
        if form.macros_open {
            out.push(self.macro_manager(owner, theme, form));
        }
        out
    }

    fn on_command(&mut self, owner: NodeKey, command: &RawCommand, ctx: &mut UpdateCtx) -> bool {
        if let RawCommand::OverlayDismiss { key, .. } = command {
            let form = &self.form;
            if form.sessions_open && *key == child_key(owner, SESSIONS_SLOT) {
                ctx.emit(ManagerEvent::Dismiss(ManagerKind::Sessions));
                return true;
            }
            if form.macros_open && *key == child_key(owner, MACROS_SLOT) {
                ctx.emit(ManagerEvent::Dismiss(ManagerKind::Macros));
                return true;
            }
            return false;
        }

        if let RawCommand::TextChanged { value, .. } = command {
            let act = match command.key::<Act>(owner) {
                Some(act @ (Act::SessionName | Act::MacroName | Act::MacroText)) => act,
                _ => return false,
            };
            ctx.emit(ManagerEvent::Edit {
                act,
                value: value.to_string(),
            });
            return true;
        }

        let form = &self.form;
        if let Some(key) = command.node_key() {
            if form.sessions_open
                && let Some(index) =
                    (0..form.sessions.len()).find(|index| session_entry_key(owner, *index) == key)
            {
                ctx.emit(ManagerEvent::Select {
                    kind: ManagerKind::Sessions,
                    index,
                });
                return true;
            }
            if form.macros_open
                && let Some(index) =
                    (0..form.macros.len()).find(|index| macro_entry_key(owner, *index) == key)
            {
                ctx.emit(ManagerEvent::Select {
                    kind: ManagerKind::Macros,
                    index,
                });
                return true;
            }
        }

        match command.key::<Act>(owner) {
            Some(
                act @ (Act::SessionConnect
                | Act::SessionSave
                | Act::SessionDelete
                | Act::SessionClose
                | Act::MacroEnter
                | Act::MacroNew
                | Act::MacroSave
                | Act::MacroRun
                | Act::MacroDelete
                | Act::MacroClose),
            ) => {
                ctx.emit(ManagerEvent::Action(act));
                true
            }
            _ => false,
        }
    }
}

/// 管理浮层里的一行（选中态 / 悬停态与 SFTP 目录项一致）。
fn manager_row(theme: &UiTheme, slot: LocalKey, selected: bool, text: String) -> ViewNode {
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

fn manager_hint(theme: &UiTheme, text: &str) -> ViewNode {
    Label::new(text)
        .size(12.0)
        .color(theme.text(TextToken::Secondary))
        .view(theme)
}

fn manager_notice_view(theme: &UiTheme, notice: &str) -> ViewNode {
    Label::new(notice.to_owned())
        .size(12.0)
        .color(theme.text(TextToken::Secondary))
        .view(theme)
}
