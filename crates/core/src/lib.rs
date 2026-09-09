pub mod analyzer;
pub mod anomaly;
pub mod collector;
pub mod config;
pub mod constants;
pub mod daily_agg;
pub mod db;
pub mod error;
pub mod heatmap;
pub mod input_agg;
pub mod insights;
pub mod json_util;
pub mod keyboard_layout;
pub mod monitors;
pub mod queries;
pub mod registry;
pub mod time;
pub mod types;

// crate 统一错误类型——供业务层（analyzer/anomaly/queries 等）与 src-tauri 消费层共用。
pub use error::{Error, Result};
