# browser-extension

Kynoptic Chromium 扩展（MV3，事件驱动）：在 tab 激活/更新时通过 Native Messaging 上报 `.tab_change` 事件，补齐窗口轨迹中浏览器 tab 轨的 unknown 段（见 KY-PRD/stage0-audit.md §4）。

设计约束：
- 事件驱动，禁轮询页面：只监听 `tabs.onActivated` / `tabs.onUpdated`。
- 权限最小化：`activeTab` / `tabs` / `idle`，无 content script、无 `host_permissions`。
- 断线缓冲：host 不可达时事件存入 `chrome.storage.local`（上限 500 条，超出丢最旧），host 恢复后按序重发。

## 目录

```
browser-extension/
  manifest.json                 MV3 清单
  background.js                 service worker：监听事件、经 sendNativeMessage 上报、断线缓冲
  native-messaging/
    kynoptic_browser_host.json  host 清单模板（path 与 allowed_origins 需填）
    README.md                   Rust host 实现说明（Stage 1 只交付文档）
```

## 从零安装步骤

### 1. 装扩展（开发者模式）

1. 打开 `chrome://extensions`，右上角开启"开发者模式"。
2. 点"加载已解压的扩展程序"，选择本 `browser-extension/` 目录。
3. 记下扩展页面上显示的 **ID**（形如 `abcdefghijklmnopqrst`）。

### 2. 注册 native messaging host

当前 Stage 1 尚无 Rust host 可执行文件（见 `native-messaging/README.md` 的交付边界）。宿主程序可用任一能处理 stdin 长度前缀 JSON 的占位可执行文件先行联调：

1. 复制 `native-messaging/kynoptic_browser_host.json` 到固定目录（例如 `%APPDATA%\kynoptic\`），填两处：
   - `path`：host 可执行文件绝对路径。
   - `allowed_origins`：把 `EXTENSION_ID_PLACEHOLDER` 换成第 1 步记下的扩展 ID。
2. 写注册表（HKCU，无需管理员），或保存为 `register-host.reg` 双击导入：
   ```
   Windows Registry Editor Version 5.00

   [HKEY_CURRENT_USER\SOFTWARE\Google\Chrome\NativeMessagingHosts\kynoptic_browser_host]
   @="C:\\Users\\<你>\\AppData\\Roaming\\kynoptic\\kynoptic_browser_host.json"
   ```
   （路径为第 1 条中清单文件的实际绝对路径。）
3. 重新加载扩展或重启 Chrome，使注册生效。

### 3. 验证方法

1. 打开 `chrome://extensions`，点扩展卡片上的"service worker"链接打开 DevTools，在 Console 确认没有 `chrome.runtime.lastError` 报错。
2. 手动触发一次发送测试（在 service worker 的 Console 执行）：
   ```js
   chrome.runtime.sendNativeMessage("kynoptic_browser_host",
     { event_type: ".tab_change", ts_ms: 0, kind: "test" },
     r => console.log(r, chrome.runtime.lastError));
   ```
   host 注册成功时应收到 host 回的 JSON；失败时 `chrome.runtime.lastError` 会提示 `Native host has exited` 或 `not found`（后者说明注册表或清单路径没配对）。
3. 切换/刷新几个标签页，检查数据落库：host 接入后用 kynoptic 查询窗口轨迹，`.tab_change` 事件点应出现在 events 表中，且原 unknown 段被填补。
4. 断线缓冲验证：关闭 kynoptic host 进程后切换若干标签页，扩展不报错、事件进缓冲；重启 host 后再切换一次标签页，缓冲的旧事件应按序补发落库。

## Stage 1 交付边界

- 本期只交付扩展骨架与文档，**不交付 Rust host 代码**；host 的协议、写入路径（走现有 db 写入路径写 events 表）与安全约定见 `native-messaging/README.md`。
- 未验证项：以下只能在真机 Chrome 中验证，本仓库无法验证——扩展加载、`sendNativeMessage` 联通、缓冲重发时序、MV3 service worker 的挂起/唤醒行为。
