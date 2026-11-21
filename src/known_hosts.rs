//! OpenSSH `known_hosts` 的读写与主机密钥校验（TOFU + 不匹配拒绝）。
//!
//! 只处理明文 hostname（`host keytype base64`）；`|1|` 开头的 hashed hostname
//! 当作"未知"处理（不误判为不匹配）。首次见到某主机时按 TOFU 追加记录
//! （与 OpenSSH 的 `StrictHostKeyChecking=accept-new` 一致），之后同一主机
//! 密钥变化即拒绝连接。

use std::io;
use std::path::PathBuf;

/// 主机密钥校验结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKeyStatus {
    /// 同一主机、同一密钥：可信。
    Match,
    /// 同一主机但密钥变了：拒绝（可能是中间人）。
    Mismatch,
    /// 首次见到该主机：按 TOFU 记录。
    Unknown,
}

/// 内存中的 `known_hosts`（保留原始行，写回时追加）。
pub struct KnownHosts {
    path: PathBuf,
    lines: Vec<String>,
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
        let mut found_host = false;
        for line in &self.lines {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.split_whitespace();
            let Some(hosts) = parts.next() else { continue };
            // hashed hostname（`|1|...`）无法反查：当作未知，避免误报不匹配。
            if hosts.starts_with("|1|") || hosts.starts_with("|2|") {
                continue;
            }
            if !hosts.split(',').any(|candidate| candidate == host) {
                continue;
            }
            found_host = true;
            let known_key = parts.collect::<Vec<_>>().join(" ");
            if known_key == key {
                return HostKeyStatus::Match;
            }
        }
        if found_host {
            HostKeyStatus::Mismatch
        } else {
            HostKeyStatus::Unknown
        }
    }

    /// 追加一条记录并落盘（只追加，不改动用户已有的行）。
    pub fn append(&mut self, host: &str, key: &str) -> io::Result<()> {
        let line = format!("{host} {key}");
        self.lines.push(line.clone());
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(file, "{line}")?;
        Ok(())
    }
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
        let dir = std::env::temp_dir().join(format!("lieterm-known-hosts-{name}"));
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
    fn hashed_hostnames_are_treated_as_unknown() {
        let path = temp_known_hosts("hashed");
        std::fs::write(
            &path,
            "|1|abc|def ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIG9ldGhlcg==\n",
        )
        .expect("写文件");
        let known = KnownHosts::load(path);
        assert_eq!(
            known.check(
                "example.com",
                "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIG9ldGhlcg=="
            ),
            HostKeyStatus::Unknown
        );
    }
}
