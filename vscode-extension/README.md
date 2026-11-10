# Kynoptic Editor Events (vscode-extension)

F04（PRD v1.0 跨生态活动感知与统一时间轴）：把 VS Code / Cursor 的真实编辑器事件
（文件焦点、语言、调试状态）上报给本机 kynoptic dashboard 的
`POST /api/ext/events`，落库 events 表（`event_type='editor'`，
`source_id='vscode_ext'` / `'cursor_ext'`），派生进统一时间轴 D 轨的
`editor_session` 区间（证据等级 observed）。

隐私边界：扩展**从不读取文件内容**，只发送文件路径与元数据（语言 id、调试
标志）。上报目标是本机回环 dashboard（127.0.0.1），数据不出机器。

## 从零安装

### 1. 安装 vsix

仓库根目录的 `vscode-extension/` 下已产出
`kynoptic-editor-extension-0.1.0.vsix`（构建命令见下节）。安装：

- VS Code：命令面板（Ctrl+Shift+P）→ `Extensions: Install from VSIX…` → 选择 vsix 文件；
  或命令行 `code --install-extension kynoptic-editor-extension-0.1.0.vsix`
- Cursor：`cursor --install-extension kynoptic-editor-extension-0.1.0.vsix`

### 2. 启动 dashboard 并取扩展令牌

1. 启动本机面板（`kynoptic-ctl dashboard` 或托盘菜单）。dashboard 启动时
   若数据目录没有 `ext-token.txt` 会自动生成扩展令牌。
2. 打开 `http://127.0.0.1:<端口>`（默认 8420）→ Settings 页签 →
   「编辑器扩展（vscode / cursor）」区块，复制显示的令牌。
   （令牌文件即数据目录下的 `ext-token.txt`，两者内容一致。）

### 3. 填扩展设置

VS Code / Cursor 设置（Ctrl+,）搜 `kynoptic`：

- `kynoptic.extToken`：粘贴第 2 步的令牌（必填，否则不上报，状态栏显示 `[!] kyn: no token`）
- `kynoptic.endpoint`：默认 `http://127.0.0.1:8420`，与 dashboard 实际端口一致
- `kynoptic.enabled`：默认开
- `kynoptic.flushSeconds`：批量上报间隔，默认 5 秒

填好令牌后状态栏应显示 `kyn: on`。

## 验证步骤

1. 在编辑器里打开任意文件、切几次窗口、启动一次调试（F5 再停止）。
2. 等待一个上报间隔（默认 5 秒）。
3. 验证事件已落库（任选其一）：
   - 打开 dashboard 的「统一时间轴」页签（`/api/timeline?minutes=60`），
     D 轨应出现 `editor_session` 区间；
   - 或直接查接口：

     ```bash
     curl -s "http://127.0.0.1:8420/api/timeline?minutes=60"
     ```

     响应 `track_d` 里带 `"source": "vscode_ext"`（Cursor 为 `"cursor_ext"`）
     的 `editor_session` 区间即为扩展证据（quality observed；编辑器仍在前台
     未关闭时尾部段为 unknown 收口，属正常口径）。
4. 重置令牌验证（可选）：设置页点「重置令牌」后，旧令牌立即失效——扩展必须
   更新 `kynoptic.extToken`，否则上报得到 401 且批次被丢弃。

## 从源码构建

```bash
cd vscode-extension
npm install
npm run compile          # tsc 输出到 out/
npm run typecheck        # tsc --noEmit
npx --yes @vscode/vsce package -o kynoptic-editor-extension-0.1.0.vsix
```

## 上报的事件类型

| action          | 触发                            | 附带字段                |
| --------------- | ------------------------------- | ----------------------- |
| file_open       | 激活编辑器切换到某个文件         | file_path, language     |
| document_change | 编辑文件（每文件 10 秒去抖）     | file_path, language     |
| focus_gained    | 编辑器窗口重新获得焦点           | —                       |
| focus_lost      | 编辑器窗口失去焦点 / 停用扩展    | —                       |
| debug_start     | 调试会话开始                     | debug=true              |
| debug_end       | 调试会话结束                     | debug=false             |

仅 `file` 协议的文档上报路径（untitled / 远端等不发送）。批量上限 200 条/批，
dashboard 单批超过 200 返回 400。
