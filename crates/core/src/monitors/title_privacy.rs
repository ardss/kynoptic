//! 窗口/标签页标题脱敏（预留的全局开关，**尚未接线**）
//!
//! 默认关闭：标题原文落库（本地数据完整优先，与既有行为契约一致）。
//! 设计路径（**目前没有任何调用方**——dash 设置项、tray、CLI 均未接入，
//! 开关恒为 false，行为与未引入本模块前完全一致）：settings
//! `redact_titles` → `CollectorSettings::redact_titles` →
//! [`set_redact_titles`]（collector 启动时接线，与 `vk_frequency_enabled`
//! 同模式）。接入后，采集侧把标题中出现的 URL 剥掉查询串（? 及之后），
//! 网址参数（token、session id 等）不再随 window/tab_change 落库。
//! 导出侧 `--redact` 是独立实现，语义与本模块一致，但不受本开关控制。

use std::sync::atomic::{AtomicBool, Ordering};

static REDACT_TITLES: AtomicBool = AtomicBool::new(false);

/// 接线点：collector 启动时按设置开关调用。
pub fn set_redact_titles(on: bool) {
    REDACT_TITLES.store(on, Ordering::Relaxed);
}

pub fn redact_titles_enabled() -> bool {
    REDACT_TITLES.load(Ordering::Relaxed)
}

/// 按当前开关处理标题：未开启时原样返回；开启时把标题中的每个
/// `scheme://` URL 截断到查询串之前（保留 scheme://host/路径）。
/// scheme 按 RFC 3986 形状识别（字母开头 + 字母数字/+.-
/// 字符），覆盖 http(s)、ws(s) 及自定义应用协议。
pub fn redact_title(title: &str) -> String {
    if !redact_titles_enabled() {
        return title.to_string();
    }
    let lower = title.to_ascii_lowercase();
    let mut out = String::with_capacity(title.len());
    let mut i = 0usize;
    while i < title.len() {
        let rest = &lower[i..];
        match find_scheme_url(rest) {
            Some(off) => {
                let start = i + off;
                out.push_str(&title[i..start]);
                // URL 到第一个空白字符为止
                let url_end = title[start..]
                    .find([' ', '\t', '\n', '\r'])
                    .map(|e| start + e)
                    .unwrap_or(title.len());
                let url = &title[start..url_end];
                match url.find('?') {
                    Some(q) => out.push_str(&url[..q]),
                    None => out.push_str(url),
                }
                i = url_end;
            }
            None => {
                out.push_str(&title[i..]);
                break;
            }
        }
    }
    out
}

/// 在 `hay`（小写副本）中找下一个 `scheme://` 的起始偏移。
/// scheme 必须以 ASCII 字母开头，后续为字母数字或 `+`/`-`/`.`。
fn find_scheme_url(hay: &str) -> Option<usize> {
    let bytes = hay.as_bytes();
    let mut from = 0usize;
    while let Some(sep) = hay[from..].find("://") {
        let pos = from + sep;
        // 向左扫出 scheme 的起点
        let mut s = pos;
        while s > 0 && is_scheme_char(bytes[s - 1]) {
            s -= 1;
        }
        if pos > s && bytes[s].is_ascii_alphabetic() {
            return Some(s);
        }
        // 该处 `://` 前不是合法 scheme，继续向后找
        from = pos + 3;
    }
    None
}

fn is_scheme_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'+' || b == b'-' || b == b'.'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_redact(on: bool, f: impl FnOnce()) {
        set_redact_titles(on);
        f();
        set_redact_titles(false);
    }

    #[test]
    fn off_by_default_keeps_original() {
        set_redact_titles(false);
        assert_eq!(
            redact_title("登录 - https://example.com/login?token=abc"),
            "登录 - https://example.com/login?token=abc"
        );
    }

    #[test]
    fn strips_query_keeps_host_and_path() {
        with_redact(true, || {
            assert_eq!(
                redact_title("登录 - https://example.com/login?token=abc&session=x"),
                "登录 - https://example.com/login"
            );
            assert_eq!(redact_title("https://a.com/p?token=x"), "https://a.com/p");
            assert_eq!(redact_title("https://x.io/a?q=1"), "https://x.io/a");
        });
    }

    #[test]
    fn handles_surrounding_text_and_trailing_url() {
        with_redact(true, || {
            assert_eq!(
                redact_title("报表 https://a.io/x?y=1 done"),
                "报表 https://a.io/x done"
            );
            assert_eq!(
                redact_title("主页 https://example.com"),
                "主页 https://example.com"
            );
        });
    }

    #[test]
    fn non_url_titles_untouched_even_when_on() {
        with_redact(true, || {
            // 本地文件名里的 ? 不是 URL 查询串，不处理
            assert_eq!(
                redact_title("文件?草稿.txt - 记事本"),
                "文件?草稿.txt - 记事本"
            );
            assert_eq!(redact_title("line1\nline2"), "line1\nline2");
        });
    }

    #[test]
    fn uppercase_scheme_matched() {
        with_redact(true, || {
            assert_eq!(
                redact_title("页面 HTTPS://A.com/p?token=x"),
                "页面 HTTPS://A.com/p"
            );
        });
    }

    #[test]
    fn ws_and_custom_schemes_matched() {
        with_redact(true, || {
            assert_eq!(
                redact_title("ws://a.com/socket?token=abc"),
                "ws://a.com/socket"
            );
            assert_eq!(
                redact_title("wss://a.com/socket?token=abc&sid=1"),
                "wss://a.com/socket"
            );
            assert_eq!(redact_title("vscode://file/x?token=t"), "vscode://file/x");
            assert_eq!(
                redact_title("zoommtg://zoom.us/join?action=join"),
                "zoommtg://zoom.us/join"
            );
        });
    }

    #[test]
    fn pseudo_scheme_like_text_untouched() {
        with_redact(true, || {
            // `://` 前不是字母开头的合法 scheme（含数字/中文/空格），不当作 URL
            assert_eq!(redact_title("比例 3://2 说明"), "比例 3://2 说明");
            assert_eq!(redact_title("中文://x?a=1"), "中文://x?a=1");
            // 本地文件名里的 ? 不是 URL 查询串
            assert_eq!(
                redact_title("文件?草稿.txt - 记事本"),
                "文件?草稿.txt - 记事本"
            );
        });
    }
}
