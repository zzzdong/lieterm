//! OpenSSH `known_hosts` 的读写与主机密钥校验（TOFU + 不匹配拒绝）。
//!
//! 支持明文 hostname 与 `|1|` hashed hostname；`@revoked` 行会拒绝对应密钥，
//! `@cert-authority` 行不参与主机密钥比对（证书校验尚未实现）。
//! 首次见到某主机密钥时按 TOFU 追加记录（与 OpenSSH 的
//! `StrictHostKeyChecking=accept-new` 一致），之后同一主机、同一密钥类型
//! 的密钥变化即拒绝连接；服务器新增另一种密钥类型（如先记录 RSA、后协商
//! Ed25519）不算变化。

use std::io;
use std::path::PathBuf;

use data_encoding::BASE64;
use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;

/// 主机密钥校验结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKeyStatus {
    /// 同一主机、同一密钥：可信。
    Match,
    /// 同一主机但同一类型的密钥变了：拒绝（可能是中间人）。
    Mismatch,
    /// 首次见到该主机（或该密钥类型）：按 TOFU 记录。
    Unknown,
    /// 命中 `@revoked` 记录：拒绝。
    Revoked,
}

/// 内存中的 `known_hosts`（保留原始行，写回时追加）。
pub struct KnownHosts {
    path: PathBuf,
    lines: Vec<String>,
}

/// 一行解析结果。只借用原行，避免为每条记录分配。
struct Record<'a> {
    marker: Option<Marker>,
    hosts: &'a str,
    key_type: &'a str,
    key_blob: &'a str,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Marker {
    CertAuthority,
    Revoked,
}

impl KnownHosts {
    /// 读取 `path`（不存在时视为空表）。
    #[must_use]
    pub fn load(path: PathBuf) -> Self {
        let lines = std::fs::read_to_string(&path)
            .map(|text| text.lines().map(ToOwned::to_owned).collect())
            .unwrap_or_default();
        Self { path, lines }
    }

    /// 主机标识：22 端口用 `host`，其它端口用 `[host]:port`（OpenSSH 约定）。
    #[must_use]
    pub fn host_token(host: &str, port: u16) -> String {
        if port == 22 {
            host.to_owned()
        } else {
            format!("[{host}]:{port}")
        }
    }

    /// 查一个主机密钥（`key` 是 `ssh-ed25519 AAAAC3...` 形式的 openssh 串）。
    #[must_use]
    pub fn check(&self, host: &str, key: &str) -> HostKeyStatus {
        let Some((key_type, key_blob)) = split_key(key) else {
            return HostKeyStatus::Unknown;
        };
        let mut matched = false;
        let mut mismatched = false;
        let mut revoked = false;

        for line in &self.lines {
            let Some(record) = parse_record(line) else {
                continue;
            };
            if !host_matches(record.hosts, host) {
                continue;
            }
            // 不同密钥类型（如 ssh-rsa 与 ssh-ed25519）是并列记录，不算冲突。
            if record.key_type != key_type {
                continue;
            }
            if record.key_blob == key_blob {
                match record.marker {
                    Some(Marker::Revoked) => revoked = true,
                    Some(Marker::CertAuthority) => {}
                    None => matched = true,
                }
            } else if record.marker.is_none() {
                mismatched = true;
            }
        }

        if revoked {
            HostKeyStatus::Revoked
        } else if matched {
            HostKeyStatus::Match
        } else if mismatched {
            HostKeyStatus::Mismatch
        } else {
            // 主机未记录，或只记录了这个主机名的其它密钥类型。
            HostKeyStatus::Unknown
        }
    }

    /// 追加一条记录并落盘（只追加，不改动用户已有的行）。
    pub fn append(&mut self, host: &str, key: &str) -> io::Result<()> {
        let line = format!("{host} {key}");
        if self.lines.iter().any(|existing| existing.trim() == line) {
            return Ok(());
        }
        self.lines.push(line.clone());
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        use std::io::{Read as _, Seek as _, Write as _};
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&self.path)?;
        // 原文件若缺少结尾换行，先补一个，避免新记录粘在上一行末尾。
        let mut last = [0u8; 1];
        let needs_newline = file
            .seek(std::io::SeekFrom::End(-1))
            .ok()
            .and_then(|_| file.read_exact(&mut last).ok())
            .is_some_and(|()| last[0] != b'\n');
        if needs_newline {
            writeln!(file)?;
        }
        writeln!(file, "{line}")?;
        Ok(())
    }
}

/// 拆出 openssh 公钥串里的类型与 base64 数据，忽略尾部注释。
fn split_key(key: &str) -> Option<(&str, &str)> {
    let mut parts = key.split_whitespace();
    let key_type = parts.next()?;
    let key_blob = parts.next()?;
    Some((key_type, key_blob))
}

/// 解析一行 `known_hosts`；空行、注释与无法识别的标记返回 `None`。
fn parse_record(line: &str) -> Option<Record<'_>> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let mut parts = line.split_whitespace();
    let first = parts.next()?;
    let (marker, hosts) = if let Some(marker) = first.strip_prefix('@') {
        let marker = match marker {
            "cert-authority" => Marker::CertAuthority,
            "revoked" => Marker::Revoked,
            _ => return None,
        };
        (Some(marker), parts.next()?)
    } else {
        (None, first)
    };
    let key_type = parts.next()?;
    let key_blob = parts.next()?;
    Some(Record {
        marker,
        hosts,
        key_type,
        key_blob,
    })
}

/// `known_hosts` 的 host 字段匹配：逗号分隔、明文或 `|1|salt|hash`。
fn host_matches(patterns: &str, host: &str) -> bool {
    patterns
        .split(',')
        .any(|entry| entry == host || hashed_host_matches(entry, host))
}

/// OpenSSH 的 `|1|` hashed hostname：HMAC-SHA1(salt, host)。
fn hashed_host_matches(entry: &str, host: &str) -> bool {
    let Some(rest) = entry.strip_prefix("|1|") else {
        return false;
    };
    let mut parts = rest.split('|');
    let (Some(salt), Some(hash)) = (parts.next(), parts.next()) else {
        return false;
    };
    let (Ok(salt), Ok(hash)) = (
        BASE64.decode(salt.as_bytes()),
        BASE64.decode(hash.as_bytes()),
    ) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha1>::new_from_slice(&salt) else {
        return false;
    };
    mac.update(host.as_bytes());
    mac.verify_slice(&hash).is_ok()
}

/// 默认的 `known_hosts` 位置；`LIETERM_KNOWN_HOSTS` 可覆盖（测试 / 便携模式）。
#[must_use]
pub fn default_path() -> PathBuf {
    if let Some(path) = std::env::var_os("LIETERM_KNOWN_HOSTS") {
        return PathBuf::from(path);
    }
    home_dir()
        .map(|home| home.join(".ssh").join("known_hosts"))
        .unwrap_or_else(|| PathBuf::from("known_hosts"))
}

/// 用户主目录。
#[must_use]
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_known_hosts(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("lieterm-known-hosts-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("临时目录");
        dir.join("known_hosts")
    }

    #[test]
    fn host_token_uses_brackets_for_nonstandard_ports() {
        assert_eq!(KnownHosts::host_token("example.com", 22), "example.com");
        assert_eq!(
            KnownHosts::host_token("example.com", 2222),
            "[example.com]:2222"
        );
    }

    #[test]
    fn check_reports_match_mismatch_and_unknown() {
        let path = temp_known_hosts("check");
        std::fs::write(
            &path,
            "# comment\nexample.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIG9ldGhlcg==\n",
        )
        .expect("写文件");
        let known = KnownHosts::load(path);

        assert_eq!(
            known.check(
                "example.com",
                "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIG9ldGhlcg=="
            ),
            HostKeyStatus::Match
        );
        assert_eq!(
            known.check(
                "example.com",
                "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIWRvbmc="
            ),
            HostKeyStatus::Mismatch
        );
        assert_eq!(
            known.check("other.example", "ssh-ed25519 AAAA"),
            HostKeyStatus::Unknown
        );
    }

    #[test]
    fn trailing_comment_does_not_cause_mismatch() {
        let path = temp_known_hosts("comment");
        std::fs::write(
            &path,
            "example.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIG9ldGhlcg== user@host\n",
        )
        .expect("写文件");
        let known = KnownHosts::load(path);
        assert_eq!(
            known.check(
                "example.com",
                "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIG9ldGhlcg=="
            ),
            HostKeyStatus::Match
        );
    }

    #[test]
    fn another_key_type_is_unknown_not_mismatch() {
        let path = temp_known_hosts("types");
        std::fs::write(
            &path,
            "example.com ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQ==\n",
        )
        .expect("写文件");
        let known = KnownHosts::load(path);
        assert_eq!(
            known.check("example.com", "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIA=="),
            HostKeyStatus::Unknown
        );
    }

    #[test]
    fn append_writes_one_line_and_is_visible_after_reload() {
        let path = temp_known_hosts("append");
        let mut known = KnownHosts::load(path.clone());
        assert_eq!(known.check("h", "ssh-ed25519 AAAA"), HostKeyStatus::Unknown);

        known.append("h", "ssh-ed25519 AAAA").expect("追加");
        assert_eq!(known.check("h", "ssh-ed25519 AAAA"), HostKeyStatus::Match);

        let reloaded = KnownHosts::load(path.clone());
        assert_eq!(
            reloaded.check("h", "ssh-ed25519 AAAA"),
            HostKeyStatus::Match
        );
        let text = std::fs::read_to_string(&path).expect("读文件");
        assert_eq!(text.lines().count(), 1);
    }

    #[test]
    fn append_does_not_duplicate_identical_record() {
        let path = temp_known_hosts("dedupe");
        let mut known = KnownHosts::load(path.clone());
        known.append("h", "ssh-ed25519 AAAA").expect("追加");
        known.append("h", "ssh-ed25519 AAAA").expect("再次追加");
        let text = std::fs::read_to_string(&path).expect("读文件");
        assert_eq!(text.lines().count(), 1);
    }

    #[test]
    fn append_adds_missing_trailing_newline_first() {
        let path = temp_known_hosts("newline");
        std::fs::write(&path, "old ssh-ed25519 BBBB").expect("写文件");
        let mut known = KnownHosts::load(path.clone());
        known.append("new", "ssh-ed25519 AAAA").expect("追加");
        let text = std::fs::read_to_string(&path).expect("读文件");
        assert_eq!(text, "old ssh-ed25519 BBBB\nnew ssh-ed25519 AAAA\n");
    }

    #[test]
    fn hashed_hostnames_are_verified() {
        // `ssh-keygen -H` 生成的 `|1|` 行：salt/hash 用 HMAC-SHA1。
        let salt = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let mut mac = Hmac::<Sha1>::new_from_slice(&salt).expect("hmac");
        mac.update(b"example.com");
        let hash = mac.finalize().into_bytes();
        let entry = format!(
            "|1|{}|{} ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIG9ldGhlcg==",
            BASE64.encode(&salt),
            BASE64.encode(&hash)
        );
        let path = temp_known_hosts("hashed");
        std::fs::write(&path, format!("{entry}\n")).expect("写文件");
        let known = KnownHosts::load(path);

        assert_eq!(
            known.check(
                "example.com",
                "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIG9ldGhlcg=="
            ),
            HostKeyStatus::Match
        );
        assert_eq!(
            known.check(
                "example.com",
                "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIWRvbmc="
            ),
            HostKeyStatus::Mismatch
        );
        assert_eq!(
            known.check("other.example", "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIA=="),
            HostKeyStatus::Unknown
        );
    }

    #[test]
    fn revoked_key_is_rejected() {
        let path = temp_known_hosts("revoked");
        std::fs::write(
            &path,
            "@revoked example.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIG9ldGhlcg==\n",
        )
        .expect("写文件");
        let known = KnownHosts::load(path);
        assert_eq!(
            known.check(
                "example.com",
                "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIG9ldGhlcg=="
            ),
            HostKeyStatus::Revoked
        );
    }
}
