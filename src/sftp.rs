//! SFTP 子系统：独立 SSH 连接 + 简易目录浏览 / 上传 / 下载。
//!
//! 复用 `ssh::connect_authenticated` 的 known_hosts 与认证顺序，但不占用终端的
//! SSH 会话：在单独连接上开 `sftp` subsystem，然后把请求串行化处理。

use tokio::sync::mpsc::UnboundedReceiver;

use anyhow::{Context as _, Result};
use russh::Disconnect;
use russh_sftp::client::SftpSession;

use crate::{
    app::{Bridge, SftpEntry, SshEvent},
    ssh::{self, AuthOptions},
};

/// UI → SFTP 任务。串行处理，避免并发改写同一远端目录。
#[derive(Debug)]
pub enum SftpAction {
    List(String),
    Download { remote: String, local: String },
    Upload { local: String, remote: String },
    Close,
}

pub fn spawn(
    handle: &tokio::runtime::Handle,
    options: AuthOptions,
    actions: UnboundedReceiver<SftpAction>,
    bridge: Bridge,
) {
    handle.spawn(async move {
        if let Err(err) = run(options, actions, &bridge).await {
            bridge.notify(SshEvent::SftpError(format!("{err:#}")));
        }
    });
}

async fn run(
    options: AuthOptions,
    mut actions: UnboundedReceiver<SftpAction>,
    bridge: &Bridge,
) -> Result<()> {
    let (session, label) = ssh::connect_authenticated(options).await?;
    let channel = session
        .channel_open_session()
        .await
        .context("打开 SFTP 通道失败")?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .context("请求 sftp subsystem 失败")?;
    let sftp = SftpSession::new(channel.into_stream())
        .await
        .context("启动 SFTP 会话失败")?;

    let cwd = sftp
        .canonicalize(".")
        .await
        .unwrap_or_else(|_| ".".to_owned());
    bridge.notify(SshEvent::SftpReady {
        path: cwd.clone(),
        entries: list(&sftp, &cwd).await?,
    });
    bridge.notify(SshEvent::SftpNotice(format!("SFTP 已连接：{label}")));

    while let Some(action) = actions.recv().await {
        match action {
            SftpAction::List(path) => match list(&sftp, &path).await {
                Ok(entries) => bridge.notify(SshEvent::SftpListing { path, entries }),
                Err(err) => {
                    bridge.notify(SshEvent::SftpError(format!("列目录 {path} 失败：{err}")))
                }
            },
            SftpAction::Download { remote, local } => match sftp.read(&remote).await {
                Ok(bytes) => {
                    if let Err(err) = tokio::fs::write(&local, &bytes).await {
                        bridge.notify(SshEvent::SftpError(format!(
                            "写入本地文件 {local} 失败：{err}"
                        )));
                        continue;
                    }
                    bridge.notify(SshEvent::SftpNotice(format!(
                        "已下载 {} → {}（{} 字节）",
                        remote,
                        local,
                        bytes.len()
                    )));
                }
                Err(err) => {
                    bridge.notify(SshEvent::SftpError(format!("下载 {remote} 失败：{err}")));
                }
            },
            SftpAction::Upload { local, remote } => match tokio::fs::read(&local).await {
                Ok(bytes) => {
                    // `SftpSession::write` 只带 WRITE 标志：远端文件不存在会失败，
                    // 已存在时会保留旧文件尾部。create() 用 CREATE|TRUNCATE|WRITE。
                    match upload(&sftp, &remote, &bytes).await {
                        Ok(()) => {
                            let parent = parent_path(&remote);
                            let notice =
                                format!("已上传 {} → {}（{} 字节）", local, remote, bytes.len());
                            match list(&sftp, &parent).await {
                                Ok(entries) => {
                                    bridge.notify(SshEvent::SftpNotice(notice));
                                    bridge.notify(SshEvent::SftpListing {
                                        path: parent,
                                        entries,
                                    });
                                }
                                Err(err) => {
                                    bridge.notify(SshEvent::SftpError(format!(
                                        "刷新目录失败：{err}"
                                    )));
                                }
                            }
                        }
                        Err(err) => {
                            bridge
                                .notify(SshEvent::SftpError(format!("上传 {remote} 失败：{err}")));
                        }
                    }
                }
                Err(err) => {
                    bridge.notify(SshEvent::SftpError(format!(
                        "读取本地文件 {local} 失败：{err}"
                    )));
                }
            },
            SftpAction::Close => break,
        }
    }

    let _ = sftp.close().await;
    let _ = session
        .disconnect(Disconnect::ByApplication, "lieterm sftp close", "zh")
        .await;
    Ok(())
}

/// 覆盖写入远端文件：不存在则创建，存在则截断。
async fn upload(sftp: &SftpSession, remote: &str, bytes: &[u8]) -> Result<()> {
    use tokio::io::AsyncWriteExt as _;

    let mut file = sftp
        .create(remote)
        .await
        .with_context(|| format!("打开远端文件 {remote}"))?;
    if let Err(err) = file.write_all(bytes).await {
        let _ = file.close().await;
        return Err(err).with_context(|| format!("写入远端文件 {remote}"));
    }
    file.close()
        .await
        .with_context(|| format!("关闭远端文件 {remote}"))?;
    Ok(())
}

async fn list(sftp: &SftpSession, path: &str) -> Result<Vec<SftpEntry>> {
    let mut entries = vec![SftpEntry {
        name: "..".to_owned(),
        is_dir: true,
        size: 0,
    }];
    for entry in sftp
        .read_dir(path)
        .await
        .with_context(|| format!("读取 {path}"))?
    {
        let metadata = entry.metadata();
        entries.push(SftpEntry {
            name: entry.file_name(),
            is_dir: metadata.is_dir(),
            size: metadata.len(),
        });
    }
    sort_entries(&mut entries);
    Ok(entries)
}

/// 目录在前，同类按名字（忽略大小写）排序，`..` 永远在最前。
fn sort_entries(entries: &mut [SftpEntry]) {
    entries.sort_by(|a, b| {
        if a.name == ".." {
            return std::cmp::Ordering::Less;
        }
        if b.name == ".." {
            return std::cmp::Ordering::Greater;
        }
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

/// 远端 POSIX 路径的父目录（`/` 的父仍是 `/`）。
pub(crate) fn parent_path(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_owned();
    }
    match trimmed.rfind('/') {
        Some(0) => "/".to_owned(),
        Some(index) => trimmed[..index].to_owned(),
        None => ".".to_owned(),
    }
}

/// 远端路径拼接（SFTP 恒用 `/`）。
pub(crate) fn join_remote(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, is_dir: bool) -> SftpEntry {
        SftpEntry {
            name: name.to_owned(),
            is_dir,
            size: 0,
        }
    }

    #[test]
    fn parent_path_handles_root_and_nested_paths() {
        assert_eq!(parent_path("/"), "/");
        assert_eq!(parent_path("/home/user"), "/home");
        assert_eq!(parent_path("/home/user/"), "/home");
        assert_eq!(parent_path("etc"), ".");
    }

    #[test]
    fn join_remote_uses_posix_separator() {
        assert_eq!(join_remote("/home/user", "a.txt"), "/home/user/a.txt");
        assert_eq!(join_remote("/", "a.txt"), "/a.txt");
    }

    #[test]
    fn sort_entries_puts_parent_first_then_directories() {
        let mut entries = vec![
            entry("z.txt", false),
            entry("B", true),
            entry("a.txt", false),
            entry("..", true),
            entry("A", true),
        ];
        sort_entries(&mut entries);
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["..", "A", "B", "a.txt", "z.txt"]);
    }
}
