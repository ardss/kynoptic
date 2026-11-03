# Native Messaging Host：实现说明（Stage 1 交付边界）

本目录在 Stage 1 只交付**文档与清单模板**，不交付 Rust host 程序代码。host 的 Rust 实现属于后续阶段的工作。

## 文件

- `kynoptic_browser_host.json`：Chrome native messaging host 清单模板。安装前必须填两处：
  - `path`：host 可执行文件的绝对路径（Windows 上仅支持绝对路径，不支持相对路径与环境变量）。
  - `allowed_origins`：扩展 ID，即 `chrome-extension://EXTENSION_ID_PLACEHOLDER/`，加载扩展（开发者模式）后在 `chrome://extensions` 里复制真实 ID 替换。

## Rust host 程序（本期不写代码，仅规定接口）

建议以 `crates/` 下新增一个二进制 crate（例如 `kynoptic-browser-host`）实现，要求：

### 协议

1. Chrome 通过 stdin/stdout 与 host 通信，消息为 **4 字节小端长度前缀 + UTF-8 JSON**。
   - stdin：来自扩展的事件对象，结构与 `background.js` 的 `makeEvent()` 输出一致（`event_type: ".tab_change"`、`ts_ms`、`kind`、`tab_id`、`window_id`、`url`、`title`、`active`）。
   - stdout：每条消息必须回一个 JSON（可为 `{"ok": true}` 空响应），否则扩展侧 `sendNativeMessage` 回调一直挂起。若只读不写，扩展会在一次调用后认为 host 无响应。
2. host 进程生命周期由 Chrome 管理：`sendNativeMessage` 每次调用拉起一个一次性进程。host 处理完消息、写库成功、flush 后即可退出，不要依赖进程常驻。

### 数据写入路径

3. 事件必须走**现有 db 写入路径**写入 `events` 表，不得新建第二真相表（铁律：append-only、只扩充现有表）。
4. 字段映射约定：
   - `ts_ms` -> `events` 表的 occurred-at 列（毫秒时间戳）。
   - `event_type = ".tab_change"`，与 stage0-audit.md 中 window 轨 `tab_change` 点事件口径一致（audit §4：扩展接入前 tab 轨大多 unknown，扩展就是补这个缺口）。
   - `url`/`title` 存入事件 payload（JSON），不拆新列。
   - 重复同题事件实例不去重、不删除，交由上层时间轴聚合（铁律：append-only）。
5. 测试禁读真实时钟：host 自身逻辑如需时间参数，一律 `_at` 后缀注入（仓库先例 `api_status_at` / `cleanup_old_events_at`）；入库时间戳以事件自带的 `ts_ms` 为准，不调用 `now()`。

### Windows 注册位置

6. host 清单的注册表键（Windows）：
   `HKEY_CURRENT_USER\SOFTWARE\Google\Chrome\NativeMessagingHosts\kynoptic_browser_host`
   默认值 = 本目录 `kynoptic_browser_host.json` 填写后的绝对路径。HKCU 注册不需要管理员权限。

### 安全边界

7. host 只接受来自本扩展的消息（Chrome 已按 `allowed_origins` 校验 origin）；host 收到消息后无需再鉴权来源，但应对 JSON 解析失败、字段缺失做防御，解析失败时回 `{"ok": false, "error": "..."}` 并退出，绝不 panic。
