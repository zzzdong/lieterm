//! 声明结构测试：不连 SSH，也能确认对话框 / 菜单栏 / 会话页的关键节点都在声明树里。

use super::*;
use crate::manager_host::ManagerHost;

fn root_with(screen: Screen, status: &str, dialog: bool) -> Root {
    let shared = Arc::new(Mutex::new(Shared {
        screen,
        width: 320.0,
        height: 200.0,
        status: status.to_owned(),
        dialog_open: dialog,
        ..Shared::default()
    }));
    let (actions, _rx) = std_mpsc::channel();
    Root {
        shared,
        actions,
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
        config: Config::default(),
        config_path: std::path::PathBuf::new(),
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
    }
}

/// 把 Root 挂进真实的 Widget 树，收集主树 + 子组件浮层。
fn mounted_view(root: Root) -> zzzui::view::ViewTree {
    let mut app = zzzui::widget::App::new();
    app.mount(root);
    app.take_view().expect("首帧必须产出声明树")
}

fn has_passive(view: &ViewNode) -> bool {
    matches!(
        &view.decl,
        zzzui::element::Decl::TextInput { passive: true, .. }
    ) || view.children.iter().any(has_passive)
}

fn has_text(node: &ViewNode, needle: &str) -> bool {
    match &node.decl {
        zzzui::element::Decl::Text { content, .. } => content.contains(needle),
        zzzui::element::Decl::TextInput { value, .. } => value.contains(needle),
        _ => node.children.iter().any(|child| has_text(child, needle)),
    }
}

#[test]
fn session_view_declares_a_key_capture_and_status_bar() {
    let view = root_with(Screen::Session, "lieterm | 80×24", false).view(&UiTheme::default());
    assert!(has_passive(&view), "终端画面必须有一个被动键盘捕获面");
    assert!(has_text(&view, "80×24"), "状态栏文本必须进声明树");
    assert!(!has_text(&view, "新建连接"), "会话界面不应包含对话框提示");
}

#[test]
fn quick_connect_panel_lists_saved_sessions() {
    let mut root = root_with(Screen::Idle, "", false);
    root.config.upsert_session(SessionProfile {
        name: "prod".to_owned(),
        host: "10.0.0.1".to_owned(),
        port: 2222,
        user: "deploy".to_owned(),
        identity: String::new(),
    });
    root.shared.lock().expect("shared lock").quick_open = true;

    let view = root.view(&UiTheme::default());
    for label in [
        "快速连接",
        "prod",
        "deploy@10.0.0.1:2222",
        "新建",
        "双击连接",
    ] {
        assert!(has_text(&view, label), "快速连接面板缺少 {label}");
    }
}

#[test]
fn quick_connect_menu_offers_crud_and_connect() {
    let mut root = root_with(Screen::Idle, "", false);
    root.config.upsert_session(SessionProfile {
        name: "prod".to_owned(),
        host: "10.0.0.1".to_owned(),
        port: 2222,
        user: "deploy".to_owned(),
        identity: String::new(),
    });
    root.shared.lock().expect("shared lock").quick_open = true;
    root.quick_menu = Some((0, 40.0, 60.0));

    let overlays = root.overlays(7, &UiTheme::default());
    assert_eq!(overlays.len(), 1, "会话行右键应只弹出一个菜单");
    for label in ["连接", "编辑", "复制", "删除"] {
        assert!(
            has_text(&overlays[0].content, label),
            "快速连接菜单缺少 {label}"
        );
    }
}

#[test]
fn quick_connect_editor_declares_crud_fields() {
    let mut root = root_with(Screen::Session, "", false);
    root.shared.lock().expect("shared lock").quick_open = true;
    root.quick_editing = true;

    let view = root.view(&UiTheme::default());
    for label in ["名称", "主机", "端口", "用户", "密钥", "保存", "取消"] {
        assert!(has_text(&view, label), "快速连接编辑器缺少 {label}");
    }
    assert!(has_passive(&view), "面板打开时终端捕获面仍在");
}

#[test]
fn tab_bar_lists_open_tabs_and_close_buttons() {
    let root = root_with(Screen::Session, "", false);
    {
        let mut shared = root.shared.lock().expect("shared lock");
        shared.tabs = vec![
            TabSummary {
                title: "本地 pwsh".to_owned(),
            },
            TabSummary {
                title: "deploy@10.0.0.1:22".to_owned(),
            },
        ];
        shared.active_tab = 1;
    }

    let view = root.view(&UiTheme::default());
    assert!(has_text(&view, "本地 pwsh"), "缺少第一个标签");
    assert!(has_text(&view, "deploy@10.0.0.1:22"), "缺少第二个标签");
    assert!(has_text(&view, "×"), "可关闭标签应暴露 × 按钮");
    assert!(has_passive(&view), "标签栏下方仍有终端捕获面");
}

#[test]
fn tab_menu_offers_duplicate_and_close() {
    let mut root = root_with(Screen::Session, "", false);
    {
        let mut shared = root.shared.lock().expect("shared lock");
        shared.tabs = vec![TabSummary {
            title: "prod".to_owned(),
        }];
        shared.active_tab = 0;
    }
    root.tab_menu = Some((0, 16.0, 40.0));

    let overlays = root.overlays(7, &UiTheme::default());
    assert_eq!(overlays.len(), 1, "右键菜单应只弹出一个浮层");
    assert!(has_text(&overlays[0].content, "复制会话"));
    assert!(has_text(&overlays[0].content, "关闭标签页"));
}

#[test]
fn idle_view_prompts_for_a_new_connection() {
    let view = root_with(Screen::Idle, "", false).view(&UiTheme::default());
    assert!(!has_passive(&view), "没有会话时没有捕获面");
    assert!(has_text(&view, "文件 > 新建连接"), "空页提示入口");
}

#[test]
fn dialog_is_modal_and_offers_local_and_ssh_tabs() {
    // 对话框由 ConnectHost 子组件声明，必须挂进完整 Widget 树才能收集到。
    let tree = mounted_view(root_with(Screen::Idle, "", true));
    let dialog = tree
        .overlays
        .iter()
        .find(|overlay| has_text(&overlay.content, "新建连接"))
        .expect("启动时对话框打开");
    assert!(dialog.spec.is_modal(), "对话框是模态的");
    assert!(has_text(&dialog.content, "本地终端"));
    assert!(has_text(&dialog.content, "SSH"));
    assert!(has_text(&dialog.content, "连接"));
}

#[test]
fn menu_bar_declares_terminal_and_help_menus() {
    let view = root_with(Screen::Idle, "", false).view(&UiTheme::default());
    for label in ["文件", "会话", "终端", "帮助"] {
        assert!(has_text(&view, label), "菜单栏缺少 {label}");
    }
}

#[test]
fn sftp_sidebar_declares_browsing_and_transfer_controls() {
    let root = root_with(Screen::Session, "", false);
    {
        let mut shared = root.shared.lock().expect("shared lock");
        shared.sftp.open = true;
        shared.sftp.path = "/home".to_owned();
        shared.sftp.entries.push(SftpEntry {
            name: "demo.txt".to_owned(),
            is_dir: false,
            size: 12,
        });
        shared.sftp.local_path = "C:/tmp/demo.txt".to_owned();
        shared.sftp.notice = "就绪".to_owned();
    }

    let view = root.view(&UiTheme::default());
    for label in [
        "SFTP", "进入", "上传", "下载", "关闭", "/home", "demo.txt", "就绪",
    ] {
        assert!(has_text(&view, label), "侧边栏缺少 {label}");
    }
    assert!(has_passive(&view), "侧边栏打开时终端捕获面仍在");
}

#[test]
fn session_manager_lists_saved_profiles() {
    let mut root = root_with(Screen::Idle, "", false);
    root.config.upsert_session(SessionProfile {
        name: "prod".to_owned(),
        host: "10.0.0.1".to_owned(),
        port: 2222,
        user: "deploy".to_owned(),
        identity: String::new(),
    });
    root.sessions_open = true;
    let mut host = ManagerHost::new();
    host.set_props(&root.manager_props());

    let overlays = host.overlays(7, &UiTheme::default());
    assert_eq!(overlays.len(), 1, "会话管理浮层打开时不再叠加其它浮层");
    assert!(overlays[0].spec.is_modal(), "会话管理是模态对话框");
    for label in [
        "已保存会话",
        "prod",
        "deploy@10.0.0.1:2222",
        "保存当前",
        "删除选中",
    ] {
        assert!(
            has_text(&overlays[0].content, label),
            "会话管理缺少 {label}"
        );
    }
}

#[test]
fn manager_overlays_are_declared_by_the_mounted_host() {
    let mut root = root_with(Screen::Idle, "", false);
    root.config.upsert_session(SessionProfile {
        name: "prod".to_owned(),
        host: "10.0.0.1".to_owned(),
        port: 2222,
        user: "deploy".to_owned(),
        identity: String::new(),
    });
    root.sessions_open = true;

    let tree = mounted_view(root);
    let dialog = tree
        .overlays
        .iter()
        .find(|overlay| has_text(&overlay.content, "已保存会话"))
        .expect("管理浮层由 ManagerHost 子组件声明");
    assert!(dialog.spec.is_modal());
    assert!(has_text(&dialog.content, "deploy@10.0.0.1:2222"));
}

#[test]
fn macro_manager_lists_macros_and_enter_state() {
    let mut root = root_with(Screen::Session, "", false);
    root.config.upsert_macro(Macro {
        name: "磁盘".to_owned(),
        text: "df -h".to_owned(),
        enter: true,
    });
    root.macros_open = true;
    // 选中宏后编辑字段会同步为它的值（点击列表行的效果）。
    root.macro_selected = Some(0);
    root.macro_name = "磁盘".to_owned();
    root.macro_text = "df -h".to_owned();
    root.macro_enter = true;
    let mut host = ManagerHost::new();
    host.set_props(&root.manager_props());

    let overlays = host.overlays(7, &UiTheme::default());
    assert_eq!(overlays.len(), 1);
    assert!(overlays[0].spec.is_modal());
    for label in ["宏", "磁盘", "df -h", "自动回车：开", "运行"] {
        assert!(has_text(&overlays[0].content, label), "宏管理缺少 {label}");
    }
}

#[test]
fn manager_host_reports_row_selection_to_root() {
    let mut root = root_with(Screen::Idle, "", false);
    root.config.upsert_session(SessionProfile {
        name: "prod".to_owned(),
        host: "10.0.0.1".to_owned(),
        port: 2222,
        user: "deploy".to_owned(),
        identity: String::new(),
    });
    root.sessions_open = true;

    let mut host = ManagerHost::new();
    host.set_props(&root.manager_props());
    let click = RawCommand::Click {
        target: zzzui::element::WidgetId::null(),
        key: Some(session_entry_key(7, 0)),
    };
    let mut host_ctx = UpdateCtx::test();
    assert!(
        host.on_command(7, &click, &mut host_ctx),
        "子组件认领列表行点击"
    );

    let select = zzzui::element::CustomEvent::new(crate::manager_host::ManagerEvent::Select {
        kind: crate::manager_host::ManagerKind::Sessions,
        index: 0,
    });
    let emit = RawCommand::Emit {
        target: zzzui::element::WidgetId::null(),
        event: select,
    };
    let mut ctx = UpdateCtx::test();
    assert!(root.on_command(7, &emit, &mut ctx));
    assert_eq!(root.session_selected, Some(0));
    assert_eq!(root.session_name, "prod");
}

#[test]
fn geometry_accounts_for_both_sidebars() {
    let geom = Geometry::new(1200.0, 800.0, true, true, true);
    assert_eq!(geom.left, QUICK_WIDTH, "快速连接面板应把终端右推");
    assert_eq!(geom.right, 1200.0 - SIDEBAR_WIDTH, "SFTP 侧栏应把终端左收");
    assert_eq!(geom.top, MENUBAR_HEIGHT + TAB_BAR_HEIGHT);
    assert_eq!(geom.bottom, 800.0 - STATUS_HEIGHT);
    assert_eq!(geom.width(), 1200.0 - QUICK_WIDTH - SIDEBAR_WIDTH);
    assert_eq!(
        geom.height(),
        800.0 - MENUBAR_HEIGHT - TAB_BAR_HEIGHT - STATUS_HEIGHT
    );
}

#[test]
fn geometry_cols_follow_the_terminal_capture_width() {
    let cell_width = 9.0;
    let both = Geometry::new(1200.0, 800.0, true, true, true);
    let no_quick = Geometry::new(1200.0, 800.0, true, false, true);
    let expected = |geom: Geometry| ((geom.width() - 8.0) / cell_width).floor() as usize;
    assert_eq!(both.cols(cell_width), expected(both));
    assert_eq!(
        both.rows(term::CELL_HEIGHT),
        (both.height() / term::CELL_HEIGHT).floor() as usize
    );
    assert!(
        both.cols(cell_width) < no_quick.cols(cell_width),
        "快速连接面板打开后列数必须减少，否则终端会画出视口"
    );
}

#[test]
fn geometry_hittest_excludes_the_sidebars() {
    let geom = Geometry::new(1200.0, 800.0, true, true, true);
    let y = geom.top + 1.0;
    assert!(
        !geom.contains(QUICK_WIDTH - 1.0, y),
        "快速连接面板不属于终端"
    );
    assert!(geom.contains(QUICK_WIDTH + 1.0, y), "面板右侧应是终端");
    assert!(!geom.contains(geom.right + 1.0, y), "SFTP 侧栏不属于终端");
    assert!(
        !geom.contains(QUICK_WIDTH + 1.0, geom.bottom + 1.0),
        "状态栏不属于终端"
    );
}

#[test]
fn clicking_ssh_tab_switches_without_connecting_a_local_shell() {
    let tree = mounted_view(root_with(Screen::Idle, "", true));
    let dialog = tree
        .overlays
        .iter()
        .find(|overlay| has_text(&overlay.content, "新建连接"))
        .expect("对话框由 ConnectHost 声明");
    // 找到「SSH」文本所在行的 key：SlotScope::Control 让它相对 Tabs 根派生，
    // 不与宿主的 Act::Connect 撞车。
    fn ssh_row_key(node: &ViewNode) -> Option<NodeKey> {
        let is_ssh = match &node.decl {
            zzzui::element::Decl::Text { content, .. } => content.as_ref() == "SSH",
            _ => false,
        };
        if is_ssh {
            return Some(node.decl.key());
        }
        node.children.iter().find_map(ssh_row_key)
    }
    let ssh_text_key = ssh_row_key(&dialog.content).expect("SSH 标签文本应在声明树里");
    assert_ne!(ssh_text_key, child_key(dialog.owner, Act::Connect.local()));

    // 点击由 ConnectHost 认领，并上报 Choice{Tabs,1} 给 Root。
    let root = root_with(Screen::Idle, "", true);
    let shared = Arc::clone(&root.shared);
    let click = RawCommand::Click {
        target: zzzui::element::WidgetId::null(),
        key: Some(ssh_text_key),
    };
    let mut host = crate::connect_host::ConnectHost::new();
    host.set_props(&root.connect_props());
    let mut host_ctx = UpdateCtx::test();
    assert!(
        host.on_command(dialog.owner, &click, &mut host_ctx),
        "子组件认领标签点击"
    );

    // Root 认领事件后切换标签，且不得触发连接。
    let choice = zzzui::element::CustomEvent::new(crate::connect_host::Choice {
        act: Act::Tabs,
        index: 1,
    });
    let emit = RawCommand::Emit {
        target: zzzui::element::WidgetId::null(),
        event: choice,
    };
    let mut root = root_with(Screen::Idle, "", true);
    let mut ctx = UpdateCtx::test();
    assert!(root.on_command(7, &emit, &mut ctx));
    assert_eq!(root.tab, 1, "点 SSH 应切换标签");
    assert_eq!(
        shared.lock().expect("shared lock").screen,
        Screen::Idle,
        "切换标签不得触发连接"
    );
}
