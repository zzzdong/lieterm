//! 本地 shell 会话：Windows 走 ConPTY（`conpty`），Unix 走 pty（`portable-pty`）。
//!
//! 与 `ssh.rs` 共用同一套 `UiEvent` / `SshEvent`：UI 只认"连上了 / 收到字节 / 关了"，
//! 因而终端渲染、键盘编码、resize 都能在没有 SSH 服务器的环境下真实验证。

use std::io::{Read, Write};
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::app::{Bridge, SshEvent, UiEvent};

/// 默认 shell：优先 pwsh，其次 Windows PowerShell，最后 cmd（Unix 用 `$SHELL`）。
#[must_use]
pub fn default_shell() -> String {
    if cfg!(windows) {
        ["pwsh.exe", "powershell.exe"]
            .into_iter()
            .find_map(which)
            .or_else(|| which("cmd.exe"))
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|| "cmd.exe".to_owned())
    } else {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_owned())
    }
}

/// 解析 `LIETERM_LOCAL`：空 / 未设 ⇒ `None`；`1` / `on` ⇒ 默认 shell；
/// 其它值既可以是命令名（`cmd`、`bash`）也可以是绝对路径。
#[must_use]
pub fn shell_from_env() -> Option<String> {
    let value = std::env::var("LIETERM_LOCAL").ok()?;
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if value == "1" || value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("on") {
        return Some(default_shell());
    }
    Some(which(value).map_or_else(
        || value.to_owned(),
        |path| path.to_string_lossy().into_owned(),
    ))
}

fn which(name: &str) -> Option<PathBuf> {
    let direct = PathBuf::from(name);
    if direct.is_file() {
        return Some(direct);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// 在后台线程里跑一个本地 shell；`input` 与 SSH 会话同一条通道（输入 / resize）。
pub fn spawn(shell: &str, cols: u16, rows: u16, input: UnboundedReceiver<UiEvent>, bridge: Bridge) {
    let shell = shell.to_owned();
    let error_bridge = bridge.clone();
    let spawned = std::thread::Builder::new()
        .name("lieterm-local".to_owned())
        .spawn(move || {
            let mut input = input;
            if let Err(err) = run(&shell, cols, rows, &mut input, &bridge) {
                bridge.notify(SshEvent::Error(format!("{err:#}")));
                bridge.notify(SshEvent::Closed);
            }
        });
    if spawned.is_err() {
        error_bridge.notify(SshEvent::Error("无法启动本地 shell 线程".to_owned()));
    }
}

/// 读端 → UI 事件；进程退出（读 0 / 出错）时补一条 `Closed`。
fn relay<R: Read + Send + 'static>(mut reader: R, bridge: Bridge) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("lieterm-local-read".to_owned())
        .spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => bridge.notify(SshEvent::Data(buf[..n].to_vec())),
                }
            }
            bridge.notify(SshEvent::Closed);
        })
        .expect("启动读线程失败")
}

#[cfg(windows)]
fn run(
    shell: &str,
    cols: u16,
    rows: u16,
    input: &mut UnboundedReceiver<UiEvent>,
    bridge: &Bridge,
) -> Result<()> {
    // Windows 不用 portable-pty：它以 `PSEUDOCONSOLE_WIN32_INPUT_MODE` 建 ConPTY，
    // 写入的 VT 输入不会被 shell 解释（表现为"只收到 4 字节初始化、命令没反应"）。
    let mut command = std::process::Command::new(shell);
    command.env("TERM", "xterm-256color");
    if let Ok(cwd) = std::env::current_dir() {
        command.current_dir(cwd);
    }
    let mut options = conpty::ProcessOptions::default();
    options.set_console_size(Some((
        i16::try_from(cols).unwrap_or(i16::MAX),
        i16::try_from(rows).unwrap_or(i16::MAX),
    )));
    let mut process = options.spawn(command).context("启动本地 shell 失败")?;

    // `PipeReader` 不是 `Send`；转成 `std::fs::File` 交给读线程（`From` 会转移句柄所有权）。
    let reader: std::fs::File = process.output().context("取 PTY 读端失败")?.into();
    let mut writer = process.input().context("取 PTY 写端失败")?;

    bridge.notify(SshEvent::Connected(format!("本地 {shell}")));
    let relay_handle = relay(reader, bridge.clone());

    while let Some(event) = input.blocking_recv() {
        match event {
            UiEvent::Input(bytes) if bytes.is_empty() => {}
            UiEvent::Input(bytes) => {
                writer.write_all(&bytes).context("发送终端输入失败")?;
                let _ = writer.flush();
            }
            UiEvent::Resize { cols, rows, .. } => {
                let _ = process.resize(
                    i16::try_from(cols).unwrap_or(i16::MAX),
                    i16::try_from(rows).unwrap_or(i16::MAX),
                );
            }
        }
    }

    let _ = process.exit(0);
    let _ = relay_handle.join();
    Ok(())
}

#[cfg(unix)]
fn run(
    shell: &str,
    cols: u16,
    rows: u16,
    input: &mut UnboundedReceiver<UiEvent>,
    bridge: &Bridge,
) -> Result<()> {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};

    let pty = native_pty_system()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("创建 PTY 失败")?;

    let mut command = CommandBuilder::new(shell);
    command.env("TERM", "xterm-256color");
    if let Ok(cwd) = std::env::current_dir() {
        command.cwd(cwd);
    }
    let mut child = pty
        .slave
        .spawn_command(command)
        .context("启动本地 shell 失败")?;
    drop(pty.slave);

    let reader = pty.master.try_clone_reader().context("取 PTY 读端失败")?;
    let mut writer = pty.master.take_writer().context("取 PTY 写端失败")?;

    bridge.notify(SshEvent::Connected(format!("本地 {shell}")));
    let relay_handle = relay(reader, bridge.clone());

    while let Some(event) = input.blocking_recv() {
        match event {
            UiEvent::Input(bytes) if bytes.is_empty() => {}
            UiEvent::Input(bytes) => {
                writer.write_all(&bytes).context("发送终端输入失败")?;
                let _ = writer.flush();
            }
            UiEvent::Resize { cols, rows, .. } => {
                let _ = pty.master.resize(PtySize {
                    rows: u16::try_from(rows).unwrap_or(u16::MAX),
                    cols: u16::try_from(cols).unwrap_or(u16::MAX),
                    pixel_width: 0,
                    pixel_height: 0,
                });
            }
        }
    }

    drop(writer);
    let _ = child.kill();
    let _ = child.wait();
    let _ = relay_handle.join();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_shell_resolves_to_an_executable() {
        let shell = default_shell();
        assert!(!shell.is_empty());
        assert!(
            std::path::Path::new(&shell).is_file(),
            "默认 shell 应是可执行文件: {shell}"
        );
    }
}
