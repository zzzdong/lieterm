//! 键名 → xterm 控制序列。
//!
//! zzzui 只给逻辑键名、可打印文本和三个修饰位；终端侧把它编码成 SSH 通道里的字节。
//! 方向键等功能键在有修饰键时走 CSI `1;{mod}` 形式，无修饰时走等价短序列。

fn modifier_param(shift: bool, alt: bool, ctrl: bool) -> u8 {
    1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl)
}

fn csi(seq: &str, name: &str, shift: bool, alt: bool, ctrl: bool) -> Option<Vec<u8>> {
    let param = modifier_param(shift, alt, ctrl);
    let bytes = if param == 1 {
        format!("\x1b[{seq}{name}")
    } else {
        format!("\x1b[1;{param}{seq}{name}")
    };
    Some(bytes.into_bytes())
}

/// 把一次按键编码成终端输入字节；返回空串表示这个键不该送到远端。
pub fn encode(name: &str, text: Option<&str>, ctrl: bool, shift: bool, alt: bool) -> Vec<u8> {
    if name == "Ime" {
        return text.unwrap_or_default().as_bytes().to_vec();
    }

    if let Some(seq) = special(name, ctrl, shift, alt) {
        return seq;
    }

    let Some(text) = text else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if alt {
        out.push(0x1b);
    }
    if ctrl {
        let Some(ch) = text.chars().next() else {
            return out;
        };
        if ch.is_ascii() {
            let upper = ch.to_ascii_uppercase();
            let code = match upper {
                '@'..='_' => upper as u8 - b'@',
                '?' => 0x7f,
                _ => ch as u8,
            };
            out.push(code);
            return out;
        }
    }
    out.extend_from_slice(text.as_bytes());
    out
}

fn special(name: &str, ctrl: bool, shift: bool, alt: bool) -> Option<Vec<u8>> {
    let prefix = if alt { "\x1b" } else { "" };
    let out = match name {
        "Enter" => return Some(b"\r".to_vec()),
        "Tab" if shift => return Some(b"\x1b[Z".to_vec()),
        "Tab" => return Some(b"\t".to_vec()),
        "Backspace" => {
            if ctrl {
                format!("{prefix}\x08")
            } else {
                format!("{prefix}\x7f")
            }
        }
        "Escape" => return Some(b"\x1b".to_vec()),
        "ArrowUp" => return csi("A", "", shift, alt, ctrl),
        "ArrowDown" => return csi("B", "", shift, alt, ctrl),
        "ArrowRight" => return csi("C", "", shift, alt, ctrl),
        "ArrowLeft" => return csi("D", "", shift, alt, ctrl),
        "Home" => return csi("", "H", shift, alt, ctrl),
        "End" => return csi("", "F", shift, alt, ctrl),
        "PageUp" => return csi("", "5~", shift, alt, ctrl),
        "PageDown" => return csi("", "6~", shift, alt, ctrl),
        "Insert" => return csi("", "2~", shift, alt, ctrl),
        "Delete" => return csi("", "3~", shift, alt, ctrl),
        "F1" | "F2" | "F3" | "F4" => {
            let letter = match name {
                "F1" => 'P',
                "F2" => 'Q',
                "F3" => 'R',
                _ => 'S',
            };
            if shift || alt || ctrl {
                return csi(
                    "",
                    &format!("{}~", 11 + name[1..].parse::<u8>().unwrap_or(1)),
                    shift,
                    alt,
                    ctrl,
                );
            }
            format!("\x1bO{letter}")
        }
        "F5" | "F6" | "F7" | "F8" | "F9" | "F10" | "F11" | "F12" => {
            let n: u8 = name[1..].parse().ok()?;
            let code = match n {
                5 => 15,
                6 => 17,
                7 => 18,
                8 => 19,
                9 => 20,
                10 => 21,
                11 => 23,
                _ => 24,
            };
            return csi("", &format!("{code}~"), shift, alt, ctrl);
        }
        _ => return None,
    };
    Some(out.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_and_control_characters() {
        assert_eq!(encode("a", Some("a"), false, false, false), b"a");
        assert_eq!(encode("A", Some("A"), false, true, false), b"A");
        assert_eq!(encode("c", Some("c"), true, false, false), vec![0x03]);
        assert_eq!(encode("[", Some("["), true, false, false), vec![0x1b]);
        assert_eq!(encode("a", Some("a"), false, false, true), b"\x1ba");
        assert_eq!(encode("Enter", Some("\r"), false, false, false), b"\r");
        assert_eq!(
            encode("Ime", Some("中"), false, false, false),
            "中".as_bytes()
        );
    }

    #[test]
    fn navigation_sequences() {
        assert_eq!(encode("ArrowUp", None, false, false, false), b"\x1b[A");
        assert_eq!(encode("ArrowDown", None, false, true, false), b"\x1b[1;2B");
        assert_eq!(encode("ArrowDown", None, true, false, false), b"\x1b[1;5B");
        assert_eq!(encode("Home", None, false, false, false), b"\x1b[H");
        assert_eq!(encode("Delete", None, false, false, false), b"\x1b[3~");
        assert_eq!(encode("F1", None, false, false, false), b"\x1bOP");
        assert_eq!(encode("F12", None, false, false, false), b"\x1b[24~");
    }
}
