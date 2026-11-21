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
        tab: 0,
        shell: 0,
        open_menu: None,
        shell_open: false,
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
