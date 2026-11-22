//! 会话与宏的持久化配置。
//!
//! 默认位置：`~/.config/lieterm/config.json`（Windows 为 `%APPDATA%\lieterm\config.json`），
//! 可用 `LIETERM_CONFIG` 覆盖。密码不落盘：会话只保存主机、端口、用户与密钥路径。
//!
//! 宏是一段文本，支持常用转义：`\\n`、`\\r`、`\\t`、`\\e`、`\\0`、`\\xNN`（如 `\\x03` 表示 Ctrl-C）。

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 全部持久化配置。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub sessions: Vec<SessionProfile>,
    pub macros: Vec<Macro>,
    /// 终端外观与编码（字体 / 配色 / 编码）。
    pub terminal: TerminalSettings,
}

/// 终端外观与字符编码设置。
///
/// 字体族为空时自动探测系统等宽字体；配色与编码接受名称字符串，
/// 未知值回退到默认（深色 / UTF-8），不会让配置加载失败。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalSettings {
    /// 字体族名；空字符串 = 自动探测。
    pub font_family: String,
    /// 字号（逻辑像素），限制在 8..=40。
    pub font_size: f64,
    /// `dark` / `light` / `solarized`。
    pub color_scheme: String,
    /// `utf-8` / `gbk` / `gb18030` / `big5` / `shift_jis` / `latin1`。
    pub encoding: String,
}

impl Default for TerminalSettings {
    fn default() -> Self {
        Self {
            font_family: String::new(),
            font_size: 14.0,
            color_scheme: "dark".to_owned(),
            encoding: "utf-8".to_owned(),
        }
    }
}

impl TerminalSettings {
    /// 字号收敛到可用区间，避免配置写错导致终端不可见。
    #[must_use]
    pub fn clamped_font_size(&self) -> f64 {
        if self.font_size.is_finite() {
            self.font_size.clamp(8.0, 40.0)
        } else {
            14.0
        }
    }
}

/// 一个保存的连接参数（不含密码）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionProfile {
    pub name: String,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub identity: String,
}

/// 一个命名宏。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Macro {
    pub name: String,
    pub text: String,
    /// 发送后自动追加回车。
    #[serde(default)]
    pub enter: bool,
}

fn default_port() -> u16 {
    22
}

impl Config {
    /// 读取配置；文件不存在或损坏时返回默认配置（损坏时打印原因，不覆盖原文件）。
    #[must_use]
    pub fn load(path: &Path) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Self::default(),
            Err(err) => {
                eprintln!("[lieterm] 读取配置 {} 失败：{err}", path.display());
                return Self::default();
            }
        };
        match serde_json::from_str(&text) {
            Ok(config) => config,
            Err(err) => {
                eprintln!("[lieterm] 解析配置 {} 失败：{err}", path.display());
                Self::default()
            }
        }
    }

    /// 保存配置；自动创建父目录。
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let mut text = serde_json::to_string_pretty(self)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        text.push('\n');
        std::fs::write(path, text)
    }

    /// 按名称新增或覆盖会话。
    pub fn upsert_session(&mut self, profile: SessionProfile) {
        if let Some(existing) = self
            .sessions
            .iter_mut()
            .find(|session| session.name == profile.name)
        {
            *existing = profile;
        } else {
            self.sessions.push(profile);
        }
    }

    /// 删除第 `index` 个会话。
    pub fn remove_session(&mut self, index: usize) -> bool {
        if index < self.sessions.len() {
            self.sessions.remove(index);
            true
        } else {
            false
        }
    }

    /// 按名称新增或覆盖宏。
    pub fn upsert_macro(&mut self, entry: Macro) {
        if let Some(existing) = self
            .macros
            .iter_mut()
            .find(|existing| existing.name == entry.name)
        {
            *existing = entry;
        } else {
            self.macros.push(entry);
        }
    }

    /// 覆盖指定位置的宏（用于编辑后重命名）。
    pub fn replace_macro(&mut self, index: usize, entry: Macro) -> bool {
        if let Some(slot) = self.macros.get_mut(index) {
            *slot = entry;
            true
        } else {
            false
        }
    }

    /// 删除第 `index` 个宏。
    pub fn remove_macro(&mut self, index: usize) -> bool {
        if index < self.macros.len() {
            self.macros.remove(index);
            true
        } else {
            false
        }
    }
}

/// 默认配置文件路径。
#[must_use]
pub fn default_path() -> PathBuf {
    if let Some(path) = std::env::var_os("LIETERM_CONFIG") {
        return PathBuf::from(path);
    }
    #[cfg(windows)]
    if let Some(appdata) = std::env::var_os("APPDATA") {
        return PathBuf::from(appdata).join("lieterm").join("config.json");
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg).join("lieterm").join("config.json");
    }
    home_dir()
        .map(|home| home.join(".config").join("lieterm").join("config.json"))
        .unwrap_or_else(|| PathBuf::from("lieterm-config.json"))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

/// 把宏文本转成要写入终端的字节；`enter` 为真时追加 `\r`。
#[must_use]
pub fn macro_bytes(text: &str, enter: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            let mut buffer = [0u8; 4];
            out.extend_from_slice(ch.encode_utf8(&mut buffer).as_bytes());
            continue;
        }
        match chars.next() {
            Some('n') => out.push(b'\n'),
            Some('r') => out.push(b'\r'),
            Some('t') => out.push(b'\t'),
            Some('e') => out.push(0x1b),
            Some('0') => out.push(0),
            Some('\\') => out.push(b'\\'),
            Some('x') => {
                let mut hex = String::new();
                while hex.len() < 2 && chars.peek().is_some_and(|next| next.is_ascii_hexdigit()) {
                    hex.push(chars.next().expect("peek 已确认"));
                }
                if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                    out.push(byte);
                } else {
                    out.extend_from_slice(b"\\x");
                    out.extend_from_slice(hex.as_bytes());
                }
            }
            Some(other) => {
                out.push(b'\\');
                let mut buffer = [0u8; 4];
                out.extend_from_slice(other.encode_utf8(&mut buffer).as_bytes());
            }
            None => out.push(b'\\'),
        }
    }
    if enter {
        out.push(b'\r');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("lieterm-config-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("临时目录");
        dir
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("config.json");
        let mut config = Config::default();
        config.upsert_session(SessionProfile {
            name: "prod".to_owned(),
            host: "10.0.0.1".to_owned(),
            port: 2222,
            user: "deploy".to_owned(),
            identity: "~/.ssh/id_ed25519".to_owned(),
        });
        config.upsert_macro(Macro {
            name: "磁盘".to_owned(),
            text: "df -h".to_owned(),
            enter: true,
        });
        config.save(&path).expect("保存");

        let loaded = Config::load(&path);
        assert_eq!(loaded, config);
    }

    #[test]
    fn terminal_settings_default_and_partial_file() {
        let dir = temp_dir("terminal");
        let path = dir.join("config.json");

        // 旧版配置没有 terminal 字段：应整体回退到默认值。
        std::fs::write(&path, r#"{"sessions":[],"macros":[]}"#).expect("写文件");
        assert_eq!(Config::load(&path).terminal, TerminalSettings::default());

        std::fs::write(
            &path,
            r#"{"terminal":{"font_family":"Fira Code","font_size":18,"color_scheme":"solarized","encoding":"gbk"}}"#,
        )
        .expect("写文件");
        let settings = Config::load(&path).terminal;
        assert_eq!(settings.font_family, "Fira Code");
        assert_eq!(settings.clamped_font_size(), 18.0);
        assert_eq!(settings.color_scheme, "solarized");
        assert_eq!(settings.encoding, "gbk");
    }

    #[test]
    fn font_size_is_clamped_to_visible_range() {
        let mut settings = TerminalSettings {
            font_size: 400.0,
            ..TerminalSettings::default()
        };
        assert_eq!(settings.clamped_font_size(), 40.0);
        settings.font_size = 1.0;
        assert_eq!(settings.clamped_font_size(), 8.0);
        settings.font_size = f64::NAN;
        assert_eq!(settings.clamped_font_size(), 14.0);
    }

    #[test]
    fn missing_or_corrupt_file_falls_back_to_default() {
        let dir = temp_dir("corrupt");
        let missing = dir.join("missing.json");
        assert_eq!(Config::load(&missing), Config::default());

        let corrupt = dir.join("corrupt.json");
        std::fs::write(&corrupt, "{not json").expect("写文件");
        assert_eq!(Config::load(&corrupt), Config::default());
        // 损坏文件不应被启动流程覆盖。
        assert_eq!(
            std::fs::read_to_string(&corrupt).expect("读文件"),
            "{not json"
        );
    }

    #[test]
    fn upsert_and_remove_sessions_and_macros() {
        let mut config = Config::default();
        config.upsert_session(SessionProfile {
            name: "a".to_owned(),
            host: "h1".to_owned(),
            port: 22,
            user: "u".to_owned(),
            identity: String::new(),
        });
        config.upsert_session(SessionProfile {
            name: "a".to_owned(),
            host: "h2".to_owned(),
            port: 22,
            user: "u".to_owned(),
            identity: String::new(),
        });
        assert_eq!(config.sessions.len(), 1);
        assert_eq!(config.sessions[0].host, "h2");

        config.upsert_macro(Macro {
            name: "m".to_owned(),
            text: "one".to_owned(),
            enter: false,
        });
        config.upsert_macro(Macro {
            name: "m".to_owned(),
            text: "two".to_owned(),
            enter: true,
        });
        assert_eq!(config.macros.len(), 1);
        assert_eq!(config.macros[0].text, "two");

        assert!(config.remove_macro(0));
        assert!(!config.remove_macro(0));
        assert!(config.remove_session(0));
        assert!(!config.remove_session(0));
    }

    #[test]
    fn macro_escapes_are_decoded() {
        assert_eq!(macro_bytes("df -h", false), b"df -h");
        assert_eq!(macro_bytes("df -h", true), b"df -h\r");
        assert_eq!(macro_bytes(r"a\nb\rc\td\ee\0", false), b"a\nb\rc\td\x1be\0");
        assert_eq!(macro_bytes(r"\x03", false), vec![0x03]);
        assert_eq!(macro_bytes(r"\\", false), b"\\");
        assert_eq!(macro_bytes(r"\q", false), b"\\q");
        assert_eq!(macro_bytes(r"\xZZ", false), b"\\xZZ");
        assert_eq!(macro_bytes("中文", true), "中文\r".as_bytes());
    }
}
