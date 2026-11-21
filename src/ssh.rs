//! russh SSH 会话：连接 / 主机密钥校验 / 公钥 + 密码 + 键盘交互认证 / PTY / shell。
//!
//! 安全默认值：
//! - `known_hosts` 按 TOFU 记录首次见到的密钥（等同 `StrictHostKeyChecking=accept-new`），
//!   之后同一主机密钥变化即**拒绝连接**（防中间人）；
//! - 连接 15s 超时；keepalive 20s × 3；
//! - 认证顺序：指定的私钥（或 `~/.ssh/id_*`）→ 密码 → 键盘交互（密码回填）；
//! - `~/.ssh/config` 的 `HostName` / `Port` / `User` / `IdentityFile` 作为对话框输入的兜底。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use russh::keys::PublicKeyOrCertificate;
use russh::keys::key::PrivateKeyWithHashAlg;
use russh::{ChannelMsg, Disconnect, client};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::app::{Bridge, SshEvent, UiEvent};
use crate::known_hosts::{HostKeyStatus, KnownHosts};
use crate::ssh_config;

#[derive(Debug, Clone)]
pub struct ConnectOptions {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    /// 私钥路径（可空：为空时用 `~/.ssh/config` 或自动扫描 `~/.ssh/id_*`）。
    pub identity: Option<String>,
    /// 测试用：覆盖 known_hosts 路径（生产为 `None`，走 `~/.ssh/known_hosts`）。
    pub(crate) known_hosts: Option<PathBuf>,
    pub cols: u32,
    pub rows: u32,
}

pub(crate) struct ClientHandler {
    /// `host` 或 `[host]:port`。
    host_token: String,
    known: Arc<Mutex<KnownHosts>>,
    /// 被拒绝的主机密钥指纹（中间人告警用）。
    rejected: Arc<Mutex<Option<String>>>,
}

/// 连接 + 认证参数（终端与 SFTP 共用，避免两边各写一遍 known_hosts / 认证顺序）。
#[derive(Debug, Clone)]
pub(crate) struct AuthOptions {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub identity: Option<String>,
    pub known_hosts: Option<PathBuf>,
}

/// 建立已认证的 SSH 会话；返回会话与用于状态栏的 `user@host:port`。
pub(crate) async fn connect_authenticated(
    options: AuthOptions,
) -> Result<(client::Handle<ClientHandler>, String)> {
    let resolved = if options.host.parse::<std::net::IpAddr>().is_ok() {
        ssh_config::Resolved::default()
    } else {
        ssh_config::resolve(&options.host)
    };
    let host = resolved.hostname.unwrap_or_else(|| options.host.clone());
    let port = if options.port == 22 {
        resolved.port.unwrap_or(options.port)
    } else {
        options.port
    };
    let user = if options.user.is_empty() {
        resolved.user.unwrap_or_default()
    } else {
        options.user.clone()
    };
    if user.is_empty() {
        bail!("没有用户名：请在连接对话框填写，或在 ~/.ssh/config 里配置 User");
    }
    let identity = options
        .identity
        .as_deref()
        .map(|path| PathBuf::from(shellexpand(path)))
        .or(resolved.identity);

    let known = Arc::new(Mutex::new(KnownHosts::load(
        options
            .known_hosts
            .clone()
            .unwrap_or_else(crate::known_hosts::default_path),
    )));
    let rejected = Arc::new(Mutex::new(None));
    let handler = ClientHandler {
        host_token: KnownHosts::host_token(&host, port),
        known,
        rejected: Arc::clone(&rejected),
    };

    let config = Arc::new(client::Config {
        keepalive_interval: Some(Duration::from_secs(20)),
        keepalive_max: 3,
        ..Default::default()
    });
    let address = format!("{host}:{port}");
    let mut session = match tokio::time::timeout(
        Duration::from_secs(15),
        client::connect(config, address.clone(), handler),
    )
    .await
    {
        Ok(Ok(session)) => session,
        Ok(Err(err)) => {
            let rejected = rejected.lock().ok().and_then(|slot| slot.clone());
            if let Some(fingerprint) = rejected {
                bail!("主机密钥不匹配（{fingerprint}）：可能存在中间人攻击，已拒绝连接");
            }
            bail!("连接 {address} 失败：{err}");
        }
        Err(_) => bail!("连接 {address} 超时（15s）"),
    };

    let mut authenticated = false;
    if let Some(path) = identity.or_else(scan_identity) {
        match load_private_key(&path, &options.password) {
            Ok(key) => {
                let key = PrivateKeyWithHashAlg::new(Arc::new(key), None);
                match session.authenticate_publickey(&user, key).await {
                    Ok(auth) if auth.success() => authenticated = true,
                    Ok(_) => eprintln!("[lieterm] 公钥认证被拒绝：{}", path.display()),
                    Err(err) => eprintln!("[lieterm] 公钥认证失败：{err}"),
                }
            }
            Err(err) => eprintln!("[lieterm] 读取私钥 {} 失败：{err}", path.display()),
        }
    }
    if !authenticated && !options.password.is_empty() {
        let auth = session
            .authenticate_password(&user, &options.password)
            .await
            .context("密码认证失败")?;
        authenticated = auth.success();
    }
    if !authenticated {
        authenticated = keyboard_interactive(&mut session, &user, &options.password).await?;
    }
    if !authenticated {
        bail!("认证失败：公钥、密码与键盘交互均被拒绝");
    }

    Ok((session, format!("{user}@{host}:{port}")))
}
impl client::Handler for ClientHandler {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool> {
        let public = match server_public_key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key.clone(),
            PublicKeyOrCertificate::Certificate(certificate) => {
                russh::keys::PublicKey::from(certificate.public_key().clone())
            }
        };
        let openssh = public.to_openssh().context("编码主机密钥失败")?;
        let status = {
            let known = self.known.lock().expect("known_hosts");
            known.check(&self.host_token, &openssh)
        };
        match status {
            HostKeyStatus::Match => Ok(true),
            HostKeyStatus::Unknown => {
                let fingerprint = public.fingerprint(russh::keys::HashAlg::Sha256);
                let mut known = self.known.lock().expect("known_hosts");
                if let Err(err) = known.append(&self.host_token, &openssh) {
                    eprintln!("[lieterm] 写入 known_hosts 失败：{err}");
                } else {
                    eprintln!("[lieterm] 首次连接 {fingerprint}，已记录到 known_hosts");
                }
                Ok(true)
            }
            HostKeyStatus::Mismatch => {
                let fingerprint = public.fingerprint(russh::keys::HashAlg::Sha256).to_string();
                eprintln!(
                    "[lieterm] 主机密钥不匹配 {}：{fingerprint}",
                    self.host_token
                );
                if let Ok(mut rejected) = self.rejected.lock() {
                    *rejected = Some(fingerprint);
                }
                Ok(false)
            }
        }
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
            eprintln!("[lieterm] SSH 会话错误：{err:#}");
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
    let (session, label) = connect_authenticated(AuthOptions {
        host: options.host.clone(),
        port: options.port,
        user: options.user.clone(),
        password: options.password.clone(),
        identity: options.identity.clone(),
        known_hosts: options.known_hosts.clone(),
    })
    .await?;
    bridge.notify(SshEvent::Connected(label));

    // 5. PTY + shell + 双向中继。
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

/// 密码认证之外，服务器可能只支持 keyboard-interactive：把密码回填到每个提问。
async fn keyboard_interactive(
    session: &mut client::Handle<ClientHandler>,
    user: &str,
    password: &str,
) -> Result<bool> {
    use client::KeyboardInteractiveAuthResponse;

    if password.is_empty() {
        return Ok(false);
    }
    let mut response = session
        .authenticate_keyboard_interactive_start(user, None::<String>)
        .await
        .context("键盘交互认证失败")?;
    loop {
        match response {
            KeyboardInteractiveAuthResponse::Success => return Ok(true),
            KeyboardInteractiveAuthResponse::Failure { .. } => return Ok(false),
            KeyboardInteractiveAuthResponse::InfoRequest { prompts, .. } => {
                let answers = prompts.iter().map(|_| password.to_owned()).collect();
                response = session
                    .authenticate_keyboard_interactive_respond(answers)
                    .await
                    .context("键盘交互回答失败")?;
            }
        }
    }
}

/// 优先用指定私钥；加密私钥用对话框里的密码尝试解锁一次。
fn load_private_key(path: &Path, password: &str) -> Result<russh::keys::PrivateKey> {
    if let Ok(key) = russh::keys::load_secret_key(path, None) {
        return Ok(key);
    }
    if password.is_empty() {
        bail!("私钥已加密，且没有可用的密码");
    }
    russh::keys::load_secret_key(path, Some(password)).map_err(Into::into)
}

/// 没有指定私钥时，扫描 `~/.ssh/id_*` 的常见默认密钥。
fn scan_identity() -> Option<PathBuf> {
    let home = crate::known_hosts::home_dir()?;
    ["id_ed25519", "id_ecdsa", "id_rsa"]
        .into_iter()
        .map(|name| home.join(".ssh").join(name))
        .find(|path| path.is_file())
}

/// 展开 `~`（对话框里可能手写 `~/.ssh/id_ed25519`）。
fn shellexpand(value: &str) -> String {
    if let Some(rest) = value.strip_prefix("~/")
        && let Some(home) = crate::known_hosts::home_dir()
    {
        return home.join(rest).to_string_lossy().into_owned();
    }
    value.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh::client::Handler as _;
    use russh::keys::PrivateKey;
    use russh::server;
    use std::time::Duration;
    use tokio::sync::mpsc;

    /// 内存 SSH 服务器：接受 tester/secret，开 PTY + shell 后发 banner 并回显输入。
    struct TestServer;

    impl server::Handler for TestServer {
        type Error = anyhow::Error;

        async fn auth_password(&mut self, user: &str, password: &str) -> Result<server::Auth> {
            if user == "tester" && password == "secret" {
                Ok(server::Auth::Accept)
            } else {
                Ok(server::Auth::reject())
            }
        }

        async fn channel_open_session(
            &mut self,
            _channel: russh::Channel<russh::server::Msg>,
            reply: server::ChannelOpenHandle,
            _session: &mut server::Session,
        ) -> Result<()> {
            reply.accept().await;
            Ok(())
        }

        async fn pty_request(
            &mut self,
            channel: russh::ChannelId,
            _term: &str,
            _cols: u32,
            _rows: u32,
            _px_width: u32,
            _px_height: u32,
            _modes: &[(russh::Pty, u32)],
            session: &mut server::Session,
        ) -> Result<()> {
            session.channel_success(channel)?;
            Ok(())
        }

        async fn shell_request(
            &mut self,
            channel: russh::ChannelId,
            session: &mut server::Session,
        ) -> Result<()> {
            session.channel_success(channel)?;
            session.data(channel, b"hello from ssh\r\n".to_vec())?;
            Ok(())
        }

        async fn data(
            &mut self,
            channel: russh::ChannelId,
            data: &[u8],
            session: &mut server::Session,
        ) -> Result<()> {
            session.data(channel, data.to_vec())?;
            Ok(())
        }
    }

    #[tokio::test]
    async fn ssh_password_auth_and_data_relay_work_end_to_end() {
        let temp = std::env::temp_dir().join(format!("lieterm-ssh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).expect("临时目录");
        let known = temp.join("known_hosts");

        let mut server_config = server::Config {
            inactivity_timeout: None,
            auth_rejection_time: Duration::from_millis(1),
            ..Default::default()
        };
        server_config.keys.push(
            PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                .expect("主机密钥"),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let server_join = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.expect("accept");
            server::run_stream(Arc::new(server_config), socket, TestServer)
                .await
                .expect("server");
        });

        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let bridge = crate::app::Bridge::from_events(event_tx);
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        let options = ConnectOptions {
            host: "127.0.0.1".to_owned(),
            port,
            user: "tester".to_owned(),
            password: "secret".to_owned(),
            identity: Some(temp.join("no-key").to_string_lossy().into_owned()),
            known_hosts: Some(known.clone()),
            cols: 80,
            rows: 24,
        };
        let client_join = tokio::spawn(async move { run(options, input_rx, &bridge).await });

        input_tx
            .send(UiEvent::Input(b"ping\r\n".to_vec()))
            .expect("输入");

        let (mut connected, mut banner, mut echo) = (false, false, false);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline && !(connected && banner && echo) {
            match tokio::time::timeout(Duration::from_millis(200), event_rx.recv()).await {
                Ok(Some(SshEvent::Connected(label))) => {
                    assert!(label.contains("tester@127.0.0.1"), "{label}");
                    connected = true;
                }
                Ok(Some(SshEvent::Data(bytes))) => {
                    let text = String::from_utf8_lossy(&bytes);
                    banner |= text.contains("hello from ssh");
                    echo |= text.contains("ping");
                }
                Ok(Some(SshEvent::Error(err))) => panic!("SSH 错误：{err}"),
                Ok(Some(
                    SshEvent::SftpReady { .. }
                    | SshEvent::SftpListing { .. }
                    | SshEvent::SftpNotice(_)
                    | SshEvent::SftpError(_),
                ))
                | Ok(Some(SshEvent::Closed))
                | Ok(None)
                | Err(_) => {}
            }
        }
        assert!(connected, "未收到 Connected");
        assert!(banner, "未收到服务器 banner");
        assert!(echo, "未收到输入回显");

        drop(input_tx);
        let _ = tokio::time::timeout(Duration::from_secs(2), client_join).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), server_join).await;
        assert!(known.exists(), "known_hosts 应记录主机密钥");
    }
    #[tokio::test]
    async fn changed_host_key_is_rejected() {
        let temp = std::env::temp_dir().join(format!("lieterm-hostkey-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).expect("临时目录");
        let path = temp.join("known_hosts");

        let key_a =
            PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519).expect("key A");
        let key_b =
            PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519).expect("key B");
        let token = KnownHosts::host_token("example.com", 2222);
        let mut known = KnownHosts::load(path);
        known
            .append(
                &token,
                &key_a.public_key().to_openssh().expect("openssh key A"),
            )
            .expect("append");

        let rejected = Arc::new(Mutex::new(None));
        let mut handler = ClientHandler {
            host_token: token,
            known: Arc::new(Mutex::new(known)),
            rejected: Arc::clone(&rejected),
        };
        let offer = PublicKeyOrCertificate::PublicKey {
            key: key_b.public_key().clone(),
            hash_alg: None,
        };
        assert!(
            !handler.check_server_key(&offer).await.expect("check"),
            "主机密钥变化必须拒绝"
        );
        assert!(
            rejected.lock().expect("rejected").is_some(),
            "应记录被拒指纹"
        );
    }
}
