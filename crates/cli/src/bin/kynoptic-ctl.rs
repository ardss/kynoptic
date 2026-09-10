//! kynoptic-ctl：与 `kynoptic` 同一实现的别名二进制（include 复用 main.rs）。
//! 独立文件以避免 "file found in multiple build targets" 的 manifest 警告
//! （CI 以 -D warnings 运行 clippy）。

include!("../main.rs");
