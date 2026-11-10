# browser-extension

Kynoptic Chromium 扩展（MV3，事件驱动）：在 tab 激活/更新时通过 Native Messaging 上报 `.tab_change` 事件，补齐窗口轨迹中浏览器 tab 轨的 unknown 段；Stage 3（PRD v1.0 F03）追加 ChatGPT 网页会话采集（opt-in），把会话开始/结束作为 `ai_session` 网页事件（source=`chatgpt_web`）经同一 Native Messaging 通道上报。

设计约束：
- 事件驱动，禁轮询页面：tab 轨只监听 `tabs.onActivated` / `tabs.onUpdated`；ChatGPT 会话轨只监听 SPA 路由变化（history.pushState/replaceState 补丁 + popstate）、可见性变化与页面卸载。
- 隐私边界：ChatGPT content script **不读取**对话内容、页面文字、标题——只产出「会话开始 / 会话结束」两种信号和时间戳。
- 显式 opt-in：ChatGPT 会话采集默认关闭；content script 不写进 manifest，由弹窗开关经 `chrome.scripting` 动态注册/注销，脚本内部再校验一次开关。关闭时立即注销并丢弃未配对的会话起点。
- 权限：`activeTab` / `tabs` / `idle` / `storage`（断线缓冲与开关状态） / `scripting`（动态注册）；`host_permissions` 仅 `chatgpt.com` / `chat.openai.com`。
- 断线缓冲：host 不可达时事件存入 `chrome.storage.local`（上限 500 条，超出丢最旧），host 恢复后按序重发。
- 孤儿起点不猜起点终点：service worker 重启后无配对 end 的 start 直接丢弃——时间轴上保持 unknown 真实缺口，不虚构会话结束时间。

## 目录

```
browser-extension/
  manifest.json                 MV3 清单（popup + scripting/storage + chatgpt.com host 权限）
  background.js                 service worker：监听 tab 事件、经 sendNativeMessage 上报、断线缓冲
  popup.html / popup.js         弹窗：ChatGPT 网页会话采集开关（默认关，动态注册 content script）
  content-chatgpt.js            ChatGPT content script（opt-in 动态注册；只报会话开始/结束）
  chatgpt-session.js            service worker 内：按 tab 配对 start/end，产 .ai_session 事件
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

**方式 A（推荐，一键）**：在仓库根目录执行

```powershell
powershell -ExecutionPolicy Bypass -File scripts\register-browser-host.ps1 -ExtensionId <第 1 步记下的 ID>
```

或直接用 CLI（需要先 `cargo build --release -p kynoptic-browser-host -p kynoptic`）：

```powershell
target\release\kynoptic.exe host-register --ext <第 1 步记下的 ID>
```

脚本/CLI 会：构建 host、生成填写好的清单（host exe 绝对路径 + 扩展 ID）写入 `%APPDATA%\kynoptic\kynoptic_browser_host.json`、写 HKCU 注册表键（无需管理员）。

**方式 B（手工）**：


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
     { event_type: ".tab_change", event_id: "manual-test-1", ts_ms: Date.now(), kind: "test", tab_id: 1, window_id: 1, url: "https://example.com/", title: "Sample", active: true },
     r => console.log(r, chrome.runtime.lastError));
   ```
   host 注册成功时应收到 `{ok:true, ...}`；重复发送同 `event_id` 应收到 `deduped:true`（幂等去重）。失败时 `chrome.runtime.lastError` 会提示 `Native host has exited` 或 `not found`（后者说明注册表或清单路径没配对）。
3. 切换/刷新几个标签页，检查数据落库：host 接入后用 kynoptic 查询窗口轨迹，`.tab_change` 事件点应出现在 events 表中，且原 unknown 段被填补。
4. 断线缓冲验证：关闭 kynoptic host 进程后切换若干标签页，扩展不报错、事件进缓冲；重启 host 后再切换一次标签页，缓冲的旧事件应按序补发落库。

## ChatGPT 网页会话（F03，Stage 3）

1. 在扩展弹窗勾选「ChatGPT 网页会话采集」——content script 动态注册到 `chatgpt.com` / `chat.openai.com`；已打开的标签页需刷新后生效。
2. 进入某个具体会话路由（`/c/...`、`/g/...`）且页面可见 → 上报 start；离开该路由、切后台或关闭页面 → 上报 end；start/end 在 service worker 配对为一条事件：
   ```json
   {
     "event_type": ".ai_session",
     "ts_ms": 1762100000000,
     "schema_version": 1,
     "event_data": {
       "schema_version": 1,
       "session_id": "chatgpt_web-tab12-1762099990000",
       "source": "chatgpt_web",
       "start": "2026-11-03T09:10:00.000Z",
       "end": "2026-11-03T09:25:00.000Z",
       "start_quality": "observed",
       "end_quality": "observed",
       "prompt_count": null
     }
   }
   ```
   host 侧按现有 ai_adapter 摄入路径写入 events 表（`source_id='ai'`，`device_id` 用本机 UUID），rebuild 后进 activity_intervals 的 `ai_run` 区间，统一时间轴 D 轨可见。
3. 验证方法：service worker DevTools Console 里 `chrome.storage.local.get(null, console.log)` 应能看到 `kynoptic_chatgpt_enabled` 与配对后的 pending 清空；打开 ChatGPT 会话再离开后，缓冲/host 日志应出现一条 `.ai_session`。

## Stage 1 交付边界

- 本期只交付扩展骨架与文档，**不交付 Rust host 代码**；host 的协议、写入路径（走现有 db 写入路径写 events 表）与安全约定见 `native-messaging/README.md`。
- 未验证项：以下只能在真机 Chrome 中验证，本仓库无法验证——扩展加载、`sendNativeMessage` 联通、缓冲重发时序、MV3 service worker 的挂起/唤醒行为；Stage 3 追加：ChatGPT 弹窗开关注册/注销 content script、SPA 路由补丁在 chatgpt.com 实际页面上的触发、start/end 配对在真机导航与多标签场景下的行为、`importScripts` 下 `report()` 的可见性（代码按经典 worker 函数提升语义编写，需真机确认）。
- 本仓库已验证：`manifest.json` JSON 语法、`background.js` / `content-chatgpt.js` / `popup.js` / `chatgpt-session.js` 经 `node --check` 语法校验通过。
