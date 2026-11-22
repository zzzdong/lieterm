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
    Download {
        remote: String,
        local: String,
    },
    Upload {
        local: String,
        remote: String,
    },
    /// 取消正在进行的传输（空闲时忽略）。
    Cancel,
    Close,
}

/// 一次文件传输的分块大小：大文件不再整读进内存。
const CHUNK_SIZE: usize = 64 * 1024;
/// 进度事件的节流阈值（每传这么多字节上报一次）。
const PROGRESS_STEP: u64 = 512 * 1024;

/// 传输结果：完成 / 用户取消 / 侧栏关闭。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Done { bytes: u64 },
    Cancelled,
    Closed,
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
            SftpAction::Download { remote, local } => {
                match download(&sftp, &remote, &local, &mut actions, bridge).await {
                    Ok(Outcome::Done { bytes }) => {
                        bridge.notify(SshEvent::SftpProgress {
                            done: bytes,
                            total: bytes,
                            active: false,
                        });
                        bridge.notify(SshEvent::SftpNotice(format!(
                            "已下载 {remote} → {local}（{bytes} 字节）"
                        )));
                    }
                    Ok(Outcome::Cancelled) => {
                        bridge.notify(SshEvent::SftpProgress {
                            done: 0,
                            total: 0,
                            active: false,
                        });
                        bridge.notify(SshEvent::SftpNotice(format!("已取消下载 {remote}")));
                    }
                    Ok(Outcome::Closed) => break,
                    Err(err) => {
                        bridge.notify(SshEvent::SftpProgress {
                            done: 0,
                            total: 0,
                            active: false,
                        });
                        bridge.notify(SshEvent::SftpError(format!("下载 {remote} 失败：{err:#}")));
                    }
                }
            }
            SftpAction::Upload { local, remote } => {
                // `SftpSession::write` 只带 WRITE 标志：远端文件不存在会失败，
                // 已存在时会保留旧文件尾部。create() 用 CREATE|TRUNCATE|WRITE。
                match upload(&sftp, &local, &remote, &mut actions, bridge).await {
                    Ok(Outcome::Done { bytes }) => {
                        bridge.notify(SshEvent::SftpProgress {
                            done: bytes,
                            total: bytes,
                            active: false,
                        });
                        let parent = parent_path(&remote);
                        let notice = format!("已上传 {local} → {remote}（{bytes} 字节）");
                        match list(&sftp, &parent).await {
                            Ok(entries) => {
                                bridge.notify(SshEvent::SftpNotice(notice));
                                bridge.notify(SshEvent::SftpListing {
                                    path: parent,
                                    entries,
                                });
                            }
                            Err(err) => {
                                bridge.notify(SshEvent::SftpError(format!("刷新目录失败：{err}")));
                            }
                        }
                    }
                    Ok(Outcome::Cancelled) => {
                        bridge.notify(SshEvent::SftpProgress {
                            done: 0,
                            total: 0,
                            active: false,
                        });
                        bridge.notify(SshEvent::SftpNotice(format!("已取消上传 {remote}")));
                    }
                    Ok(Outcome::Closed) => break,
                    Err(err) => {
                        bridge.notify(SshEvent::SftpProgress {
                            done: 0,
                            total: 0,
                            active: false,
                        });
                        bridge.notify(SshEvent::SftpError(format!("上传 {remote} 失败：{err:#}")));
                    }
                }
            }
            // 空闲时收到取消 / 关闭：关闭请求结束任务，取消无需处理。
            SftpAction::Cancel => {}
            SftpAction::Close => break,
        }
    }

    let _ = sftp.close().await;
    let _ = session
        .disconnect(Disconnect::ByApplication, "lieterm sftp close", "zh")
        .await;
    Ok(())
}

/// 传输期间检查取消 / 关闭；其它请求直接丢弃，避免与传输交错。
fn interrupted(actions: &mut UnboundedReceiver<SftpAction>) -> Option<Outcome> {
    while let Ok(action) = actions.try_recv() {
        match action {
            SftpAction::Cancel => return Some(Outcome::Cancelled),
            SftpAction::Close => return Some(Outcome::Closed),
            _ => {}
        }
    }
    None
}

/// 分块下载：远端读到本地，边传边上报进度。
async fn download(
    sftp: &SftpSession,
    remote: &str,
    local: &str,
    actions: &mut UnboundedReceiver<SftpAction>,
    bridge: &Bridge,
) -> Result<Outcome> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let mut source = sftp
        .open(remote)
        .await
        .with_context(|| format!("打开远端文件 {remote}"))?;
    let total = source.metadata().await.map(|meta| meta.len()).unwrap_or(0);
    let mut target = tokio::fs::File::create(local)
        .await
        .with_context(|| format!("创建本地文件 {local}"))?;
    bridge.notify(SshEvent::SftpProgress {
        done: 0,
        total,
        active: true,
    });

    let mut buffer = vec![0u8; CHUNK_SIZE];
    let mut done = 0u64;
    let mut reported = 0u64;
    let outcome = loop {
        if let Some(outcome) = interrupted(actions) {
            break outcome;
        }
        let read = source
            .read(&mut buffer)
            .await
            .with_context(|| format!("读取远端文件 {remote}"))?;
        if read == 0 {
            break Outcome::Done { bytes: done };
        }
        target
            .write_all(&buffer[..read])
            .await
            .with_context(|| format!("写入本地文件 {local}"))?;
        done += read as u64;
        if done - reported >= PROGRESS_STEP {
            reported = done;
            bridge.notify(SshEvent::SftpProgress {
                done,
                total,
                active: true,
            });
        }
    };

    match outcome {
        Outcome::Done { .. } => {
            target
                .flush()
                .await
                .with_context(|| format!("刷新本地文件 {local}"))?;
        }
        Outcome::Cancelled | Outcome::Closed => {
            let _ = target.flush().await;
            drop(target);
            // 半截文件会误导用户，取消后直接删掉。
            let _ = tokio::fs::remove_file(local).await;
        }
    }
    Ok(outcome)
}

/// 分块上传：本地读到远端（create 会截断旧文件），边传边上报进度。
async fn upload(
    sftp: &SftpSession,
    local: &str,
    remote: &str,
    actions: &mut UnboundedReceiver<SftpAction>,
    bridge: &Bridge,
) -> Result<Outcome> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let mut source = tokio::fs::File::open(local)
        .await
        .with_context(|| format!("打开本地文件 {local}"))?;
    let total = source.metadata().await.map(|meta| meta.len()).unwrap_or(0);
    let mut target = sftp
        .create(remote)
        .await
        .with_context(|| format!("打开远端文件 {remote}"))?;
    bridge.notify(SshEvent::SftpProgress {
        done: 0,
        total,
        active: true,
    });

    let mut buffer = vec![0u8; CHUNK_SIZE];
    let mut done = 0u64;
    let mut reported = 0u64;
    let outcome = loop {
        if let Some(outcome) = interrupted(actions) {
            break outcome;
        }
        let read = source
            .read(&mut buffer)
            .await
            .with_context(|| format!("读取本地文件 {local}"))?;
        if read == 0 {
            break Outcome::Done { bytes: done };
        }
        target
            .write_all(&buffer[..read])
            .await
            .with_context(|| format!("写入远端文件 {remote}"))?;
        done += read as u64;
        if done - reported >= PROGRESS_STEP {
            reported = done;
            bridge.notify(SshEvent::SftpProgress {
                done,
                total,
                active: true,
            });
        }
    };

    match outcome {
        Outcome::Done { .. } => {
            target
                .close()
                .await
                .with_context(|| format!("关闭远端文件 {remote}"))?;
        }
        Outcome::Cancelled | Outcome::Closed => {
            drop(target);
            // 取消后删除远端半截文件。
            let _ = sftp.remove_file(remote).await;
        }
    }
    Ok(outcome)
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
