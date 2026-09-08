//! JSON 工具
//!
//! 集中处理"JSON 字符串解析失败"的容错,避免散落各 command 的
//! `serde_json::from_str(&s).unwrap_or(json!({}))` 样板。

use serde_json::Value;

/// 解析 JSON 字符串为 `Value`,失败返回 `Value::Object(Default::default())`。
///
/// 用于历史事件 event_data 反序列化——失败时不能 panic(可能磁盘损坏或 schema 漂移),
/// 返回空对象 `{ }` 让上层继续处理(取子字段全 None,行为可预期)。
///
/// 替代命令层散落的 3 处 `serde_json::from_str(&s).unwrap_or(json!({}))`。
pub fn parse_event_data(s: &str) -> Value {
    serde_json::from_str(s).unwrap_or_else(|_| Value::Object(Default::default()))
}

/// 解析 JSON 字符串为 `Value`,失败返回 `Value::Null`。
///
/// 与 [`parse_event_data`] 的差异：失败时不返回空对象而是 Null。
/// 用于"无数据是合理默认值"的场景(如 [`crate::insights`] 的 USB 事件)。
pub fn parse_event_data_or_null(s: &str) -> Value {
    serde_json::from_str(s).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_object() {
        let v = parse_event_data(r#"{"k": 1}"#);
        assert_eq!(v["k"], 1);
    }

    #[test]
    fn parse_invalid_returns_empty_object() {
        assert_eq!(
            parse_event_data("not json"),
            Value::Object(Default::default())
        );
        assert_eq!(parse_event_data(""), Value::Object(Default::default()));
        assert_eq!(parse_event_data("{"), Value::Object(Default::default()));
    }

    #[test]
    fn parse_or_null_returns_null_on_error() {
        assert_eq!(parse_event_data_or_null("not json"), Value::Null);
        let v = parse_event_data_or_null(r#"["a", "b"]"#);
        assert!(v.is_array());
    }
}
