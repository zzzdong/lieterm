//! 声明结构测试：不连 SSH，也能确认对话框 / 菜单栏 / 会话页的关键节点都在声明树里。

use super::*;

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
    let root = root_with(Screen::Idle, "", true);
    let overlays = root.overlays(7, &UiTheme::default());
    assert_eq!(overlays.len(), 1, "启动时对话框打开");
    assert!(overlays[0].spec.is_modal(), "对话框是模态的");
    assert!(has_text(&overlays[0].content, "本地终端"));
    assert!(has_text(&overlays[0].content, "SSH"));
    assert!(has_text(&overlays[0].content, "连接"));
}

#[test]
fn menu_bar_declares_the_three_menus() {
    let view = root_with(Screen::Idle, "", false).view(&UiTheme::default());
    for label in ["文件", "会话", "帮助"] {
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

    let overlays = root.overlays(7, &UiTheme::default());
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

    let overlays = root.overlays(7, &UiTheme::default());
    assert_eq!(overlays.len(), 1);
    assert!(overlays[0].spec.is_modal());
    for label in ["宏", "磁盘", "df -h", "自动回车：开", "运行"] {
        assert!(has_text(&overlays[0].content, label), "宏管理缺少 {label}");
    }
}
