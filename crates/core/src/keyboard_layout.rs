//! Windows 虚拟键码 (VK_*) → 可读键名映射
//!
//! 这部分是键盘布局业务逻辑（热力图按键频率统计、快捷键组合识别），
//! 原本内联在 src-tauri 的 commands/realtime.rs，现下沉到 core crate，
//! 使 command 层只做数据组装、不持有布局知识。

/// 数字键名查找（vk 0x30..=0x39 → "0".."9"）。
/// 用 const 数组替代穷举 match，消除不可达分支。
const DIGIT_NAMES: [&str; 10] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"];
fn static_digit_name(vk: u64) -> &'static str {
    DIGIT_NAMES[(vk - 0x30) as usize]
}

/// Windows 虚拟键码 → 可读键名。无法识别返回 None。
pub fn vk_to_key_name(vk: u64) -> Option<&'static str> {
    match vk {
        0x41..=0x5A => {
            let names = [
                "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P",
                "Q", "R", "S", "T", "U", "V", "W", "X", "Y", "Z",
            ];
            Some(names[(vk - 0x41) as usize])
        }
        0x30..=0x39 => Some(static_digit_name(vk)),
        0x08 => Some("Backspace"),
        0x09 => Some("Tab"),
        0x0D => Some("Enter"),
        0x10 | 0xA0 | 0xA1 => Some("Shift"),
        0x11 | 0xA2 | 0xA3 => Some("Ctrl"),
        0x12 | 0xA4 | 0xA5 => Some("Alt"),
        0x14 => Some("CapsLock"),
        0x1B => Some("Esc"),
        0x20 => Some("Space"),
        0x25 => Some("Left"),
        0x26 => Some("Up"),
        0x27 => Some("Right"),
        0x28 => Some("Down"),
        0x2D => Some("Insert"),
        0x2E => Some("Delete"),
        0x70..=0x7B => Some(match vk {
            0x70 => "F1",
            0x71 => "F2",
            0x72 => "F3",
            0x73 => "F4",
            0x74 => "F5",
            0x75 => "F6",
            0x76 => "F7",
            0x77 => "F8",
            0x78 => "F9",
            0x79 => "F10",
            0x7A => "F11",
            0x7B => "F12",
            _ => "Fn",
        }),
        0x5B | 0x5C => Some("Win"),
        0xBA => Some(";"),
        0xBB => Some("="),
        0xBC => Some(","),
        0xBD => Some("-"),
        0xBE => Some("."),
        0xBF => Some("/"),
        0xC0 => Some("`"),
        0xDB => Some("["),
        0xDC => Some("\\"),
        0xDD => Some("]"),
        0xDE => Some("'"),
        _ => None,
    }
}

/// 判断键名是否为修饰键（用于排除"修饰键本身"作为快捷键主键）。
pub fn is_modifier_key(name: &str) -> bool {
    matches!(
        name,
        "Shift" | "Ctrl" | "Alt" | "Win" | "WinRight" | "CapsLock"
    )
}

/// 把 monitor 采集的小写修饰键名规范为大写显示形式。
pub fn capitalize_mod(s: &str) -> String {
    match s {
        "ctrl" => "Ctrl".into(),
        "shift" => "Shift".into(),
        "alt" => "Alt".into(),
        "win" => "Win".into(),
        "cmd" => "Cmd".into(),
        "fn" => "Fn".into(),
        _ => s.into(),
    }
}
