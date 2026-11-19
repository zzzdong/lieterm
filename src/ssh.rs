//! russh SSH 会话：连接 / 密码认证 / PTY / shell，双向转发 UI 字节。
//!
//! 所有 SSH I/O 都在 tokio 任务里，UI 线程只通过 channel 交换字节与状态。

use std::sync::Arc;

use anyhow::{Context as _, Result};
use russh::{ChannelMsg, Disconnect, client};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::app::{Bridge, SshEvent, UiEvent};

#[derive(Debug, Clone)]
pub struct ConnectOptions {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub cols: u32,
    pub rows: u32,
}

struct ClientHandler;

impl client::Handler for ClientHandler {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool> {
        // MVP：接受主机密钥；PuTTY 式的主机密钥指纹缓存留待后续。
        eprintln!("[lieterm] 服务器主机密钥: {key:?}");
        Ok(true)
    }
}

pub fn spawn(
    handle: &tokio::runtime::Handle,
    options: ConnectOptions,
    input: UnboundedReceiver<UiEvent>,
    bridge: Bridge,
) {
    handle.spawn(async move {
        if let Err(err) = run(options, input, &bridge).await {
            bridge.notify(SshEvent::Error(format!("{err:#}")));
            bridge.notify(SshEvent::Closed);
        }
    });
}

async fn run(
    options: ConnectOptions,
    mut input: UnboundedReceiver<UiEvent>,
    bridge: &Bridge,
) -> Result<()> {
    let config = Arc::new(client::Config::default());
    let address = format!("{}:{}", options.host, options.port);
    let mut session = client::connect(config, address, ClientHandler)
        .await
        .context("连接 SSH 服务器失败")?;

    let auth = session
        .authenticate_password(options.user.clone(), options.password.clone())
        .await
        .context("密码认证失败")?;
    if !auth.success() {
        anyhow::bail!("密码认证被拒绝");
    }

    let mut channel = session
        .channel_open_session()
        .await
        .context("打开会话通道失败")?;
    channel
        .request_pty(
            true,
            "xterm-256color",
            options.cols,
            options.rows,
            0,
            0,
            &[],
        )
        .await
        .context("申请 PTY 失败")?;
    channel
        .request_shell(true)
        .await
        .context("启动 shell 失败")?;

    bridge.notify(SshEvent::Connected(format!(
        "{}@{}",
        options.user, options.host
    )));

    loop {
        tokio::select! {
            message = channel.wait() => {
                match message {
                    Some(ChannelMsg::Data { data }) => {
                        bridge.notify(SshEvent::Data(data.to_vec()));
                    }
                    Some(ChannelMsg::ExtendedData { data, ext: 1 }) => {
                        bridge.notify(SshEvent::Data(data.to_vec()));
                    }
                    Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
                    Some(_) => {}
                }
            }
            event = input.recv() => {
                match event {
                    Some(UiEvent::Input(bytes)) if bytes.is_empty() => {}
                    Some(UiEvent::Input(bytes)) => {
                        channel.data_bytes(bytes).await.context("发送终端输入失败")?;
                    }
                    Some(UiEvent::Resize { cols, rows, px_width, px_height }) => {
                        channel.window_change(cols, rows, px_width, px_height).await?;
                    }
                    None => break,
                }
            }
        }
    }

    let _ = session
        .disconnect(Disconnect::ByApplication, "lieterm exit", "zh")
        .await;
    bridge.notify(SshEvent::Closed);
    Ok(())
}
