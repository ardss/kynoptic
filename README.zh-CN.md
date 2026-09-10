# Kynoptic

[English](README.md) | 简体中文

![Kynoptic dashboard demo](demo.gif)

Windows 上的本地优先活动感知层。Kynoptic 记录你使用了哪些应用、机器何时繁忙、系统发生了什么变化——然后让你（或你的 AI 助手）事后提问。所有数据都留在你的机器上。

它回答那些事后才冒出来的问题："上周二我用过的那个工具叫什么？""昨天下午两点笔记本为什么发烫？""这周我真正花在编辑器上的时间有多少？"——通过 CLI、本地面板或 MCP 提问。

- 官网：<https://kynoptic.com>
- 状态：v0.1，正在向 9 月 14 日发布冲刺。API 仍可能变化。

## 工作原理

- **40 个监控器**覆盖整机：前台窗口与应用切换、键鼠活动计数、空闲时间、电池、网络接口、设备、进程、音频、亮度、Wi-Fi 等。**默认启用 14 个**（纯 Win32 API、零子进程）；其余 26 个（浏览器标签页、剪贴板、蓝牙等）已内置，按需开启。
- **原始事件原样落库**到本机 SQLite。聚合表（每分钟/每天桶）只是加速查询的派生只读缓存；原始层永不截断、永不自动清理。
- **本地查询**：CLI 临时提问；MCP server 让本地 AI 助手回答"我今天干了啥"，数据不出机器。

## 快速开始

要求：Windows 10/11 与 Rust 工具链（stable，MSVC target）。

```bash
git clone https://github.com/ardss/kynoptic
cd kynoptic
cargo build --release -p kynoptic-cli

# 采集（前台运行，Ctrl+C 优雅停止；数据默认写 %LOCALAPPDATA%\kynoptic\kynoptic.db）
target\release\kynoptic.exe collect --help     # 查看可用参数

# 查询记录
target\release\kynoptic.exe query --from today
target\release\kynoptic.exe stats --days 7
target\release\kynoptic.exe dashboard          # 仅本机可访问的网页面板（127.0.0.1）
```

### 让 AI 助手使用它（MCP）

内置 MCP server（stdio JSON-RPC），提供五个工具：`get_current_status`、
`get_summary`、`get_timeline`、`get_anomalies`、`wait_for`。接入你的 MCP 客户端：

```json
{
  "mcpServers": {
    "kynoptic": { "command": "kynoptic", "args": ["mcp"] }
  }
}
```

之后你的助手就能回答"昨天 CPU 飙高的时候我在干嘛"这类问题——数据始终不离开你的磁盘。

## 性能

实测环境：Ryzen 5 5600X / Windows 11，可由仓库内基准 harness 复现（完整方法与
原始数据见 [BENCHMARKS.md](BENCHMARKS.md)）：

| 指标 | 实测 |
|---|---|
| 空载 CPU（整个采集器，默认监控集） | 单核 0.41%（p95 1.56%） |
| 常驻内存（稳态） | 约 16.5 MB |
| 启动到首帧采样 | 约 238 ms（中位数） |
| 存储成本 | 原始层 336 字节/事件（默认配置） |
| 100 万事件库查询 p95 | 全部工具 < 50 ms（异常检测约 30 ms，走聚合缓存） |
| 大旧库打开（100 万行，待回填） | 约 8 ms（回填在后台分块进行） |

## 隐私

- 记录的数据默认永不离开你的机器。无账号、无遥测、无统计上报、无网络调用。
- **键盘与鼠标只存每分钟计数——绝不存按键内容、键序与时间戳。**仪表盘键盘热力图
  使用逐键频次计数（每个键被按了多少次），是聚合统计而非内容。逐键明细默认关闭，
  可在设置中显式开启。这条边界是 Kynoptic 与 spyware 的分界线，由代码强制，不是口号。
- 聚合是派生缓存；原始事件日志只增不删。
- 删除任何数据都是显式 opt-in，默认关闭；schema 迁移以重命名归档代替删除。

这个取舍是刻意的：数据完整，Kynoptic 才有用；数据本地，Kynoptic 才可信。

## 仓库结构

| Crate | 职责 |
|---|---|
| `crates/core` | 采集器、事件管线、SQLite 存储、查询 |
| `crates/cli` | `kynoptic` / `kynoptic-ctl` 命令行与本地面板 |
| `crates/dash` | dashboard 服务与页面（CLI 与托盘共用） |
| `crates/mcp` | MCP server（stdio） |
| `crates/tray` | 托盘壳（纯 Win32，宿主采集器与本地面板） |

监控器注册表（默认开关的单一事实源）：`crates/core/src/registry.rs`。应用内部文档：
[APP.md](APP.md)，设计笔记：[CODE_NOTES.md](CODE_NOTES.md)。

## 许可

Apache-2.0。见 [LICENSE](LICENSE)。
