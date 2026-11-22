//! 精简的 `~/.ssh/config` 解析：把 Host 别名解析成 HostName / Port / User / IdentityFile。
//!
//! 只做 PuTTY 用户最常见的那一档：`Host` 支持 `*` / `?` 通配与 `!` 否定，
//! 取第一个匹配块的有效值（与 OpenSSH "first obtained value wins" 一致）；
//! 不支持 `Include` / `Match` / `ProxyJump`。

use std::path::PathBuf;

/// 一个 Host 别名解析出的连接参数（全部可选，缺省沿用对话框输入）。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub hostname: Option<String>,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub identity: Option<PathBuf>,
}

/// 默认的 `~/.ssh/config` 路径。
#[must_use]
pub fn default_path() -> Option<PathBuf> {
    crate::known_hosts::home_dir().map(|home| home.join(".ssh").join("config"))
}

/// 按别名解析（读不到文件时返回全 `None`）。
#[must_use]
pub fn resolve(alias: &str) -> Resolved {
    let path = std::env::var_os("LIETERM_SSH_CONFIG")
        .map(PathBuf::from)
        .or_else(default_path);
    let Some(path) = path else {
        return Resolved::default();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Resolved::default();
    };
    resolve_from(&text, alias)
}

/// 纯解析入口（测试与 `LIETERM_SSH_CONFIG` 覆盖用）。
#[must_use]
pub fn resolve_from(text: &str, alias: &str) -> Resolved {
    let mut out = Resolved::default();
    let mut matched = false;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(keyword) = parts.next() else {
            continue;
        };
        let values: Vec<&str> = parts.collect();
        match keyword.to_ascii_lowercase().as_str() {
            "host" => matched = host_matches(&values, alias),
            "hostname" if matched && out.hostname.is_none() => {
                out.hostname = values.first().map(|value| (*value).to_owned());
            }
            "port" if matched && out.port.is_none() => {
                out.port = values.first().and_then(|value| value.parse().ok());
            }
            "user" if matched && out.user.is_none() => {
                out.user = values.first().map(|value| (*value).to_owned());
            }
            "identityfile" if matched && out.identity.is_none() => {
                out.identity = values
                    .first()
                    .map(|value| PathBuf::from(expand_home(value)));
            }
            _ => {}
        }
    }
    out
}

/// `Host` 行匹配：支持通配与 `!` 否定；主机名匹配不区分大小写（与 OpenSSH 一致）。
fn host_matches(patterns: &[&str], alias: &str) -> bool {
    let alias = alias.to_ascii_lowercase();
    let mut positive = false;
    for pattern in patterns {
        if let Some(negated) = pattern.strip_prefix('!') {
            if wildcard_match(&negated.to_ascii_lowercase(), &alias) {
                return false;
            }
        } else if wildcard_match(&pattern.to_ascii_lowercase(), &alias) {
            positive = true;
        }
    }
    positive
}

/// 简单通配：`*` 任意串、`?` 单字符。
fn wildcard_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<usize> = None;
    let mut mark = 0;
    while ti < text.len() {
        if pi < pattern.len() && (pattern[pi] == '?' || pattern[pi] == text[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < pattern.len() && pattern[pi] == '*' {
            star = Some(pi);
            pi += 1;
            mark = ti;
        } else if let Some(start) = star {
            pi = start + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < pattern.len() && pattern[pi] == '*' {
        pi += 1;
    }
    pi == pattern.len()
}

fn expand_home(value: &str) -> String {
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

    const CONFIG: &str = r#"
Host prod prod-*
    HostName 10.0.0.10
    Port 2222
    User deploy

Host *.internal !secret.internal
    HostName internal.example
    User ops

# 全局默认（放最后：first obtained value wins）
Host *
    User fallback
    IdentityFile ~/.ssh/id_ed25519
"#;

    #[test]
    fn exact_alias_resolves_host_port_user_and_identity() {
        let resolved = resolve_from(CONFIG, "prod");
        assert_eq!(resolved.hostname.as_deref(), Some("10.0.0.10"));
        assert_eq!(resolved.port, Some(2222));
        assert_eq!(resolved.user.as_deref(), Some("deploy"));
        assert!(
            resolved.identity.is_some(),
            "Host * 的 IdentityFile 应作为兜底"
        );
    }

    #[test]
    fn wildcard_alias_matches() {
        let resolved = resolve_from(CONFIG, "prod-eu");
        assert_eq!(resolved.hostname.as_deref(), Some("10.0.0.10"));
    }

    #[test]
    fn negated_pattern_excludes_the_block() {
        let resolved = resolve_from(CONFIG, "secret.internal");
        assert_eq!(resolved.hostname, None, "!secret.internal 应排除该块");
        assert_eq!(resolved.user.as_deref(), Some("fallback"), "回落到 Host *");
    }

    #[test]
    fn unknown_alias_falls_back_to_global_defaults() {
        let resolved = resolve_from(CONFIG, "random-host");
        assert_eq!(resolved.hostname, None);
        assert_eq!(resolved.user.as_deref(), Some("fallback"));
    }

    #[test]
    fn wildcard_matcher_supports_star_and_question() {
        assert!(wildcard_match("prod-*", "prod-eu"));
        assert!(wildcard_match("h?st", "host"));
        assert!(!wildcard_match("h?st", "hoost"));
        assert!(wildcard_match("*", "anything"));
        assert!(wildcard_match("a*b*c", "aXXbYYc"));
    }

    #[test]
    fn host_matching_is_case_insensitive() {
        let resolved = resolve_from("Host PROD\n    User deploy\n", "Prod");
        assert_eq!(resolved.user.as_deref(), Some("deploy"));
    }
}
