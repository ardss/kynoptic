//! 统一错误类型
//!
//! 业务层（analyzer/anomaly/queries/daily_agg 的编排入口）与本 crate 的 src-tauri
//! 消费层共用本类型。`#[from] rusqlite::Error` / `serde_json::Error` / `io::Error`
//! / `csv::Error` 让底层错误自动 `?` 转换，无需 `.map_err(|e| e.to_string())`
//! 字符串化（Phase 13.2 收尾）。
//!
//! 容错策略仍保留：聚合类查询（COUNT/SUM）在 [`crate::queries`] 底层失败时返回
//! 0/空 + log warn（`count_or_log` / `string_or_log`），不冒泡到本类型——只有
//! 真正需要让调用方感知失败的写/编排操作（delete/recompute/analyze_day/detect_*）
//! 才返回 [`Result`]。

/// crate 统一错误
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// 底层数据库错误（SQL 执行失败、连接异常等）
    #[error("数据库错误: {0}")]
    Db(#[from] rusqlite::Error),

    /// 读连接池在限定周期内无可用连接（已触发降级路径）
    #[error("读连接池耗尽")]
    PoolExhausted,

    /// Schema 迁移失败
    #[error("迁移失败: {0}")]
    Migration(String),

    /// 数据非法（如时间戳格式、配置越界）
    #[error("数据错误: {0}")]
    InvalidData(String),

    /// JSON 序列化/反序列化错误（serde_json）
    #[error("序列化错误: {0}")]
    Serde(#[from] serde_json::Error),

    /// IO 错误（文件读写等）
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),
}

/// crate 统一 Result 别名
pub type Result<T> = std::result::Result<T, Error>;

/// 序列化为纯字符串（保持与旧 `Result<_, String>` 一致的前端契约）。
///
/// Tauri 2 的 `#[command]` 要求错误类型实现 [`serde::Serialize`]。前端
/// (`src/api.ts`) 用 `.catch(() => null)` 吞错，从不解析错误体，故序列化为
/// `Display` 字符串最稳妥——与 Phase 13.2 之前 `.map_err(|e| e.to_string())`
/// 的行为完全等价。
impl serde::Serialize for Error {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}
