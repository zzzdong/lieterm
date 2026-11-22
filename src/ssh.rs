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
use russh::keys::HashAlg;
use russh::keys::PublicKeyOrCertificate;
#[cfg(any(unix, windows))]
use russh::keys::agent::client::AgentClient;
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

/// 认证阶段的总超时：服务器半开连接时不能无限等待。
const AUTH_TIMEOUT: Duration = Duration::from_secs(30);
/// 连接阶段（TCP + 握手）超时。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// 建立已认证的 SSH 会话；返回会话与用于状态栏的 `user@host:port`。
pub(crate) async fn connect_authenticated(
    options: AuthOptions,
) -> Result<(client::Handle<ClientHandler>, String)> {
    // 对 IP 也读 `~/.ssh/config`：这样 `Host *` 里的 User / IdentityFile / Port
    // 才能作为兜底（与 OpenSSH 一致），而不仅是别名。
    let resolved = ssh_config::resolve(&options.host);
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
    let address = socket_address(&host, port);
    let mut session = match tokio::time::timeout(
        CONNECT_TIMEOUT,
        client::connect(config, address.clone(), handler),
    )
    .await
    {
        Ok(Ok(session)) => session,
        Ok(Err(err)) => {
            let reason = rejected.lock().ok().and_then(|slot| slot.clone());
            if let Some(reason) = reason {
                bail!("主机密钥被拒绝（{reason}）：主机密钥已变更或已被撤销，已拒绝连接");
            }
            bail!("连接 {address} 失败：{err}");
        }
        Err(_) => bail!("连接 {address} 超时（{}s）", CONNECT_TIMEOUT.as_secs()),
    };

    let authenticated = match tokio::time::timeout(
        AUTH_TIMEOUT,
        authenticate(&mut session, &user, &options.password, identity),
    )
    .await
    {
        Ok(result) => result?,
        Err(_) => bail!("认证超时（{}s）", AUTH_TIMEOUT.as_secs()),
    };
    if !authenticated {
        bail!("认证失败：公钥、密码与键盘交互均被拒绝");
    }

    Ok((session, format!("{user}@{host}:{port}")))
}

/// 依次尝试：指定/默认私钥 → 密码 → 键盘交互（密码回填）。
async fn authenticate(
    session: &mut client::Handle<ClientHandler>,
    user: &str,
    password: &str,
    identity: Option<PathBuf>,
) -> Result<bool> {
    // RFC 4252 的 "none"：空密码账号等场景下服务器可能直接接受，不计入失败次数。
    if session.authenticate_none(user).await?.success() {
        return Ok(true);
    }

    let use_agent = identity.is_none();
    let mut rsa_hash = None;

    // 未显式指定私钥时优先尝试 ssh-agent / Pageant；显式指定时不混入 agent 的
    // 其它身份，避免用错密钥或耗尽 MaxAuthTries。
    if use_agent && try_agent_auth(session, user, &mut rsa_hash).await? {
        return Ok(true);
    }

    let paths = match identity {
        Some(path) => vec![path],
        None => default_identities(),
    };
    let mut keys = Vec::new();
    for path in paths {
        match load_private_key(&path, password) {
            Ok(key) => keys.push((path, key)),
            Err(err) => eprintln!("[lieterm] 读取私钥 {} 失败：{err}", path.display()),
        }
    }

    // 只为 RSA 查询服务端支持的签名算法：`None` 在 russh 里等价于已废弃的
    // `ssh-rsa`（SHA-1），现代 OpenSSH 默认拒绝。EXT_INFO 缺失时先试 SHA-256，
    // 再退回 SHA-1 兼容老服务器。agent 认证阶段已查询过则直接复用。
    let rsa_hash = if keys.iter().any(|(_, key)| key.algorithm().is_rsa()) {
        match rsa_hash {
            Some(cached) => Some(cached),
            None => session.best_supported_rsa_hash().await.ok().flatten(),
        }
    } else {
        rsa_hash
    };

    for (path, key) in keys {
        let hash_algs = if key.algorithm().is_rsa() {
            match rsa_hash {
                Some(Some(alg)) => vec![Some(alg)],
                Some(None) => vec![None],
                None => vec![Some(HashAlg::Sha256), None],
            }
        } else {
            vec![None]
        };
        for hash_alg in hash_algs {
            let candidate = PrivateKeyWithHashAlg::new(Arc::new(key.clone()), hash_alg);
            match session.authenticate_publickey(user, candidate).await {
                Ok(auth) if auth.success() => return Ok(true),
                Ok(_) => eprintln!("[lieterm] 公钥认证被拒绝：{}", path.display()),
                Err(err) => {
                    eprintln!("[lieterm] 公钥认证失败 {}：{err}", path.display());
                    break;
                }
            }
        }
    }

    if !password.is_empty() {
        let auth = session
            .authenticate_password(user, password)
            .await
            .context("密码认证失败")?;
        if auth.success() {
            return Ok(true);
        }
    }

    keyboard_interactive(session, user, password).await
}

/// 连接本机 ssh-agent（Unix）或 Pageant（Windows）。
#[cfg(unix)]
async fn try_agent_auth(
    session: &mut client::Handle<ClientHandler>,
    user: &str,
    rsa_hash: &mut Option<Option<HashAlg>>,
) -> Result<bool> {
    let mut agent = match AgentClient::connect_env().await {
        Ok(agent) => agent,
        Err(err) => {
            eprintln!("[lieterm] ssh-agent 不可用（{err}），跳过");
            return Ok(false);
        }
    };
    agent_auth(session, user, &mut agent, rsa_hash).await
}

/// Pageant 只存在于 Windows；未设置时静默跳过。
#[cfg(windows)]
async fn try_agent_auth(
    session: &mut client::Handle<ClientHandler>,
    user: &str,
    rsa_hash: &mut Option<Option<HashAlg>>,
) -> Result<bool> {
    let mut agent = match AgentClient::connect_pageant().await {
        Ok(agent) => agent,
        Err(err) => {
            eprintln!("[lieterm] Pageant 不可用（{err}），跳过");
            return Ok(false);
        }
    };
    agent_auth(session, user, &mut agent, rsa_hash).await
}

#[cfg(not(any(unix, windows)))]
async fn try_agent_auth(
    _session: &mut client::Handle<ClientHandler>,
    _user: &str,
    _rsa_hash: &mut Option<Option<HashAlg>>,
) -> Result<bool> {
    Ok(false)
}

/// 用 agent 中的身份逐个尝试公钥认证；agent 不可用不应阻断后续认证方式。
#[cfg(any(unix, windows))]
async fn agent_auth<S>(
    session: &mut client::Handle<ClientHandler>,
    user: &str,
    agent: &mut AgentClient<S>,
    rsa_hash: &mut Option<Option<HashAlg>>,
) -> Result<bool>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send,
{
    let identities = match agent.request_identities().await {
        Ok(identities) => identities,
        Err(err) => {
            eprintln!("[lieterm] 读取 ssh-agent 身份失败（{err}），跳过");
            return Ok(false);
        }
    };

    for identity in identities {
        let key = identity.public_key().into_owned();
        let hash = if key.algorithm().is_rsa() {
            match *rsa_hash {
                Some(hash) => hash,
                None => {
                    let supported = session.best_supported_rsa_hash().await.ok().flatten();
                    *rsa_hash = supported;
                    supported.flatten()
                }
            }
        } else {
            None
        };
        match session
            .authenticate_publickey_with(user, key, hash, agent)
            .await
        {
            Ok(auth) if auth.success() => return Ok(true),
            Ok(_) => eprintln!("[lieterm] ssh-agent 密钥被拒绝：{}", identity.comment()),
            Err(err) => eprintln!("[lieterm] ssh-agent 签名失败：{err}"),
        }
    }
    Ok(false)
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
            HostKeyStatus::Revoked => {
                let fingerprint = public.fingerprint(russh::keys::HashAlg::Sha256).to_string();
                eprintln!(
                    "[lieterm] 主机密钥已在 known_hosts 中撤销 {}：{fingerprint}",
                    self.host_token
                );
                if let Ok(mut rejected) = self.rejected.lock() {
                    *rejected = Some(format!("{fingerprint}，known_hosts 中标记为 @revoked"));
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

/// 没有指定私钥时，按顺序尝试 `~/.ssh` 下所有常见默认密钥。
///
/// 只取第一个存在的文件（旧行为）会在 `id_ed25519` 存在但未被服务器授权、
/// 而 `id_rsa` 已授权时直接认证失败；逐个尝试才与 OpenSSH 一致。
fn default_identities() -> Vec<PathBuf> {
    let Some(home) = crate::known_hosts::home_dir() else {
        return Vec::new();
    };
    ["id_ed25519", "id_ecdsa", "id_rsa"]
        .into_iter()
        .map(|name| home.join(".ssh").join(name))
        .filter(|path| path.is_file())
        .collect()
}

/// TCP 地址：IPv6 字面量要加方括号（`[::1]:22`），否则 `host:port` 无法解析。
fn socket_address(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
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
        let bridge = crate::app::Bridge::from_events(0, event_tx);
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
                Ok(Some((_, SshEvent::Connected(label)))) => {
                    assert!(label.contains("tester@127.0.0.1"), "{label}");
                    connected = true;
                }
                Ok(Some((_, SshEvent::Data(bytes)))) => {
                    let text = String::from_utf8_lossy(&bytes);
                    banner |= text.contains("hello from ssh");
                    echo |= text.contains("ping");
                }
                Ok(Some((_, SshEvent::Error(err)))) => panic!("SSH 错误：{err}"),
                Ok(Some((
                    _,
                    SshEvent::SftpReady { .. }
                    | SshEvent::SftpListing { .. }
                    | SshEvent::SftpNotice(_)
                    | SshEvent::SftpError(_),
                )))
                | Ok(Some((_, SshEvent::Closed)))
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
    #[test]
    fn socket_address_brackets_ipv6_literals() {
        assert_eq!(socket_address("127.0.0.1", 22), "127.0.0.1:22");
        assert_eq!(socket_address("::1", 2222), "[::1]:2222");
        assert_eq!(socket_address("[::1]", 2222), "[::1]:2222");
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
