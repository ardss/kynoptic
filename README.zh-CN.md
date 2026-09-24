# Kynoptic

[English](README.md) | 简体中文

![Kynoptic dashboard demo](demo.gif)

Windows 上的本地优先活动感知层。Kynoptic 记录你使用了哪些应用、机器何时繁忙、系统发生了什么变化——然后让你（或你的 AI 助手）事后提问。所有数据都留在你的机器上。

它回答那些事后才冒出来的问题："上周二我用过的那个工具叫什么？""昨天下午两点笔记本为什么发烫？""这周我真正花在编辑器上的时间有多少？"——通过 CLI、本地面板或 MCP 提问。

- 官网：<https://kynoptic.com>
- 状态：v0.3.0，活跃开发中。API 仍可能变化。

## 机器在忙 ≠ 人在场

机器繁忙不等于有人在场。Kynoptic 不把两者混成一个"活跃度"数字，而是把它们
分开：被 Windows 标记为注入（LLKHF_INJECTED——来自脚本、宏或 AI agent）的
输入事件会从"人在场"指标中滤除、单独计数，自动化脚本把键盘敲得再响也不会
虚增你的"用机时间"。

仪表盘建立在三个指标上：

- **人在场（human presence）**——有真实（非注入）键鼠/滚轮输入的分钟数（滚动计入在场：主动阅读也是"人"）。
- **自动化（automation）**——只有注入输入的分钟数（脚本、agent）。
- **前台驻留（foreground dwell）**——应用占据前台窗口的分钟数，与是否有输入无关。

对比三者即可回答原始活动日志回答不了的问题：**那里有没有"人"？**总览卡片
做的就是这道算术：

![Kynoptic 仪表盘总览](assets/dashboard-hero.png)

> 真实一天的数据示例：前台驻留 **20 小时 1 分**，人在场 **10 小时 14 分**，
> 自动化注入操作 **8 分** → 无人值守前台 = 20 小时 1 分 −（10 小时 14 分 +
> 8 分）= **9 小时 39 分**——机器看起来在忙、但没有任何人
> 在操作的部分（构建、同步任务、挂着不关的应用）。原始活动计数器会把这一
> 整块都报成"使用"；Kynoptic 告诉你其中哪部分是真的有人。

## 工作原理

- **40 个监控器**覆盖整机：前台窗口与应用切换、键鼠活动计数、空闲时间、电池、网络接口、设备、进程、音频、亮度、Wi-Fi 等。**默认启用 14 个**（纯 Win32 API、零子进程）；其余 26 个（浏览器标签页、剪贴板、蓝牙等）已内置，按需开启。
- **原始事件原样落库**到本机 SQLite。聚合表（每分钟/每天桶）只是加速查询的派生只读缓存；原始层永不截断、永不自动清理。
- **本地查询**：CLI 临时提问；MCP server 让本地 AI 助手回答"我今天干了啥"，数据不出机器。

## 快速开始

**方式一——安装包（推荐，1 分钟内完成，无需任何工具链）：**

从最新 Release 下载
[Kynoptic-Setup.exe](https://github.com/ardss/kynoptic/releases/latest)，
运行安装，从开始菜单启动 Kynoptic。采集自动开始，仪表盘在浏览器
`http://127.0.0.1:8422` 打开（8422 被占用时按
8422-8432 → 18422-18432 → 28422-28432 三段依次回退，
实际端口写入 `data\dashboard-port.txt`——**该回退仅在托盘入口生效**；
命令行 `kynoptic dashboard` 端口被占时会直接报错退出，
需用 `--port` 另指定端口）。

> **SmartScreen 提示：** 构建未做代码签名，首次运行 Windows 会弹出
> "Windows 已保护你的电脑"。点击 **更多信息 → 仍要运行**（二进制可自行检验——
> Release 页提供 SHA256 校验和，也可用 `cargo build --release` 从源码复现）。
> 程序完全在本机运行，除每日一次的版本检查外不发起任何网络请求。

**方式二——从源码构建：**

要求：Windows 10/11 与 Rust 工具链（stable，MSVC target）。

```bash
git clone https://github.com/ardss/kynoptic
cd kynoptic
cargo build --release -p kynoptic

# 采集（前台运行，Ctrl+C 优雅停止；数据默认写 <exe 目录>\data\kynoptic.db）
target\release\kynoptic.exe collect --help     # 查看可用参数

# 查询记录
target\release\kynoptic.exe query --from today
target\release\kynoptic.exe stats --days 7
target\release\kynoptic.exe dashboard          # 仅本机可访问的网页面板（127.0.0.1）
```

### 让 AI 助手使用它（MCP）

内置 MCP server（stdio JSON-RPC），提供六个工具：`get_current_status`、
`get_summary`、`get_timeline`、`get_top_apps`、`get_anomalies`、`wait_for`。接入你的 MCP 客户端：

```json
{
  "mcpServers": {
    "kynoptic": { "command": "kynoptic", "args": ["mcp"] }
  }
}
```

指定数据库时 `--db <PATH>` 与 `KYNOPTIC_DB` 环境变量均可（`mcp --db` 内部
即经 `KYNOPTIC_DB` 传递）：

```json
{
  "mcpServers": {
    "kynoptic": {
      "command": "kynoptic",
      "args": ["mcp"],
      "env": { "KYNOPTIC_DB": "D:/data/kynoptic.db" }
    }
  }
}
```

之后你的助手就能回答"昨天 CPU 飙高的时候我在干嘛"这类问题——数据始终不离开你的磁盘。

## 常见问题排查

- **双击托盘图标没反应**：说明已有一个 Kynoptic 实例在运行，本次启动自行退出了。程序目录的 `duplicate-start.log` 和数据目录的 `tray.log` 记录了原因。单实例按用户计、且为机器级——不区分数据目录，因此两个不同数据目录的部署也会互相顶替，后启动的不会运行。
- **弹出"保护已失效"提示**：负责自动恢复的看门狗计划任务被禁用或删除了。可在 Windows 任务计划程序里重新启用名为 `Kynoptic Watchdog` 的任务，或重新安装 Kynoptic。

## 性能

实测环境：Ryzen 5 5600X / Windows 11，可由仓库内基准 harness 复现（完整方法与
原始数据见 [BENCHMARKS.md](BENCHMARKS.md)）：

| 指标 | 实测 |
|---|---|
| 空载 CPU（整个采集器，默认监控集） | 单核 0.41%（p95 1.56%） |
| 常驻内存（稳态） | 约 16.5 MB |
| 启动到首帧采样 | 约 238 ms（中位数） |
| 存储成本 | raw 形态 336 字节/事件；默认的分钟粒度输入形态约小 2.3 倍 |
| 100 万事件库查询 p95 | 全部工具 < 50 ms（异常检测约 30 ms，走聚合缓存） |
| 大旧库打开（100 万行，待回填） | 约 8 ms（回填在后台分块进行） |

## 隐私

- 记录的数据默认永不离开你的机器。无账号、无遥测、无统计上报、无网络调用（每天一次的版本检查访问 GitHub 除外；发现新版本只在托盘菜单提示，绝不自动安装）。
- **键盘与鼠标只存每分钟计数——绝不存按键内容、键序与时间戳。**仪表盘键盘热力图
  使用逐键频次计数（每个键被按了多少次），是聚合统计而非内容。逐键频次
  **默认开启**（本地数据完整优先），可在设置中显式关闭（opt-out）。`export`
  导出默认输出 `window_title` 完整原文；需要剥 URL 查询串时显式传 `--redact`。
  隐私加固永远以开关形式存在，但绝不以静默削减本地数据为代价；把导出文件
  分享到机器之外的风险由使用者自行承担。
- 聚合是派生缓存；原始事件日志只增不删。
- 删除任何数据都是显式 opt-in，默认关闭；schema 迁移以重命名归档代替删除。

这个取舍是刻意的：数据完整，Kynoptic 才有用；数据本地，Kynoptic 才可信。

## 仓库结构

| Crate | 职责 |
|---|---|
| `crates/core` | 采集器、事件管线、SQLite 存储、查询 |
| `crates/cli` | `kynoptic` / `kynoptic-ctl` 命令行 |
| `crates/dash` | dashboard 服务与页面（CLI 与托盘共用） |
| `crates/mcp` | MCP server（stdio） |
| `crates/tray` | 托盘壳（纯 Win32，宿主采集器与本地面板） |

监控器注册表（默认开关的单一事实源）：`crates/core/src/registry.rs`。应用内部文档：
[APP.md](APP.md)，设计笔记：[CODE_NOTES.md](CODE_NOTES.md)。

## 许可

Apache-2.0。见 [LICENSE](LICENSE)。
