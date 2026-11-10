# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- F07 统一时间轴筛选器（PRD v1.0 F07，2026-11-10 差距补齐）：①core 新增
  `TimelineFilter`（project / source / evidence 三维）与纯函数
  `api_unified_timeline_filtered_at`——evidence 按 PRD §2.3 词表
  （observed/inferred/unknown）匹配，source 匹配 `source_id` 或 `app_name`
  （ai_run 的 app_name=源名 claude_code/zcode/dsh，用户无需知道存储里
  source_id='ai'），project 精确匹配区间归属；无筛选时与既有
  `api_unified_timeline_at` 完全同值（同一实现 wrapper）。B 轨整段 unknown
  占位段仅当过滤为空或 evidence 显式含 unknown 且未带 source/project 过滤
  时保留——占位段是「无数据源」声明，不是可筛数据；被过滤排除时轨道显示
  「窗口内无区间」，缺口仍由「未接入」标注如实可见。②dash `GET
  /api/timeline?minutes=N` 支持 `project=` `source=` `evidence=`（逗号分隔
  与重复参数两种写法等价）；evidence 越词表值返回 400（静默空表会被误读为
  「无活动」，违反 unknown 不是零的可追溯原则）；响应回显 `filters` 字段供
  三端一致性核对。③merge=1 时筛选条件原样透传各远端（三端契约一致）。
  ④MCP `get_unified_timeline` 新增可选 project/source/evidence 数组参数
  （与 HTTP 同一 TimelineFilter 契约，evidence 越表报错）。⑤统一时间轴
  页签筛选条：项目文本框（精确匹配）、来源下拉（windows/chrome_ext/
  vscode_ext/claude_code/zcode/dsh/chatgpt_web）、证据等级三勾选框
  （全部不勾=不过滤），应用/清除按钮 + 回显标签；筛选随请求发后端由 core
  纯函数统一过滤（UI/API/MCP 同口径同值，前端不做本地二次筛选）。
  ⑥测试：证据等级/来源/项目/组合过滤与默认同值契约、HTTP 筛选契约
  （400 词表校验、逗号与重复参数等价、ignored_params、filters 回显）、
  merge 透传 query 进请求行（mock 请求行回显）、dashboard 筛选 UI 挂点。

- F11 多设备 A/B 联合时间轴（PRD v1.0 F11，dash 合流阶段）：①设置项
  `device_endpoints`（[{name,url,token}]，opt-in 默认空 = 单机模式）——
  远端设备即另一台机器的本产品 dashboard 地址（经 SSH 隧道时指向本地
  转发端口），设置页双语编辑器可增删行；POST 校验 name 非空、url 仅
  http 且主机名可解析（与拉取侧同一解析器，防注入请求行）、字段长度
  钳制与 categories 同口径。②新合流端点形态 `GET
  /api/timeline?minutes=N&merge=1`：并行（每端点独立线程）逐个拉取各
  远端 `/api/timeline?minutes=N`（不带 merge，防级联合流；单端点 5s
  超时），任一失败降级为该设备 offline 显式标注（status=offline +
  error 摘要），绝不阻塞也绝不替代本机数据；按设备分轨合并返回，每条
  track / 区间带 `device` 字段；`human_presence_boundary` 明示 C 轨
  （人在场）按设备独立呈现、多设备绝不相加（stage0-audit §5 决策 3）。
  传输零新依赖：std::net 手写 HTTP/1.1 GET（与 serve 同族），远端开启
  dashboard-token.txt 时以 `X-Kynoptic-Access-Token` 头携带 settings 里
  配置的 token。③统一时间轴页签 UI：多设备响应自动出现设备选择器
  （全部/本机/各远端，纯前端过滤缓存数据）；分轨渲染带设备徽标；offline
  设备以 [!] 行显式标注「时间轴缺失——真实缺口不是零」；区间 tooltip 带
  设备名。④测试：本地 TcpListener mock（在线返回 timeline JSON /
  连接拒绝 / 故意慢响应测超时降级）覆盖 fetch 合并/降级/超时、路由级
  merge=1（在线+offline 混合）、无端点时单机合流形态、纯合流函数
  now_ms 注入（零真实时钟依赖）、device_endpoints 设置回环与校验拒绝。
  真机 A/B 联调待双机安装后执行（阻断中）。

- F09 采集器健康汇总（PRD v1.0 F09）：新端点 `GET /api/health`（只读，
  token_required 时随 /api/* 统一鉴权）。三段判定：轮询监视器逐项
  ok/degraded/stale/unknown（依据进程级 last_success_age_secs /
  consecutive_slow，阈值常量化并注明依据——监视器 stale > 300s（覆盖最慢
  60s 级常规轮询周期 5 倍裕量）、AI 通道 stale > 24h（hook 事件与会话粒度
  对齐）、扩展 stale > 10min（编辑器默认 5s 一批，超时更可能是没在用而非
  断连，故只标 stale 不判「已断开」）；unknown = 本进程无证据（如独立
  serve 进程读不到托盘采集器内存统计），不是零也不是故障。AI hook 通道
  （claude_code / zcode）与文件扫描通道（ai）最近成功时间直接读 events 表
  （observed）；扩展连接状态（chrome_ext / vscode_ext / cursor_ext）按
  source_id 最近事件判定；键盘/鼠标为钩子型（回调驱动无轮询周期）不进
  监视器表并注记。dashboard 新「健康」页签（/api/health 可视化，双语，
  未知状态灰显）；CLI 新子命令 `kynoptic health [--json]`（与端点同一
  api_health_at 纯函数入口，三端同源同数值）。判定全部时间参数化
  （api_health_at 注入 now），单测覆盖阈值边界 / 通道状态随时间翻转 /
  路由 200 / 结构完整性。

- F10 隐私安全补强（PRD v1.0 F10）：①绑定断言测试——serve 起后同端口
  通配地址（0.0.0.0）仍可绑定即证明只绑 127.0.0.1，另钉 DNS rebinding
  防线（非回环 Host 403 forbidden_host）；②token_required 覆盖新端点
  测试——dashboard-token.txt 启用后 GET /api/health 无/错令牌 401、正确
  令牌 200（HTTP 层 /api/* 前缀统一闸门，无需逐端点白名单）；③整标题
  脱敏 opt-in：新设置项 `redact_titles_full`（默认 false，不改既有行为
  与既有数据——只拦新写入，历史数据禁删禁改铁律不变），开启后新写入的
  window/tab_change 标题一律以 `[redacted]` 落库，比 redact_titles 的
  URL 查询串剥离更强一档；接线路径同款（设置页开关 + POST /api/settings
  → tray / CLI 映射 CollectorSettings → title_privacy::set_redact_full），
  设置页隐私区补双语说明；core 单测钉优先级（full 优先于 URL 模式、默认
  原样）；dash 单测钉设置读写回环 + 默认关 + UI 元素在位；④扩展摄取
  路径注入防线：POST /api/ext/events 的 file_path 含 `..` 路径段整批
  400 拒收不落行（真实编辑器路径事件不带相对穿越；文件名内恰好含
  `a..b` 非路径段不受影响）；AI hook 通道 payload 字段白名单本就不收
  任何路径字段（session_id / tool_name / duration_ms / waiting / retry），
  无路径注入面，测试已有覆盖。

- F06 缺口补齐：人工项目指定与归属可追溯（PRD v1.0）。新表
  `project_overrides`（migration 0018，append-only 派生层）：每次人工指定
  追加一行、永不改删（修正 = 追加新行），记录 corrected_from（指定前归属
  来源，审计）/ created_at / created_by / source='manual'，可与 events 一起
  全量重建 activity_intervals。归属优先级两档：人工指定（project_source=
  'manual'，归属证据 observed——用户亲口指定是直证）> 规则命中
  （project_source='rule'，证据 inferred，不变）；全不命中仍为 NULL（查询
  侧按 unassigned 展示，无证据不猜）。匹配语义：区间 (kind, app_name,
  title_hash) 相同且窗口相交，多条取最晚指定（id 最大）。activity_intervals
  加 project_source 列；ai_run 无标题证据、规则无从命中，但人工指定按
  源名 + 窗口相交仍可归属（不改区间在场 quality——归属证据与在场证据
  两回事）。dash 新端点 `POST /api/intervals/project`（与 POST /api/settings
  同级防线：会话令牌 + CSRF + Origin；payload 校验 + 目标区间核对——
  activity_intervals 里必须存在匹配行否则 400 + append-only 落库 + 读
  settings 规则重建派生表，人工指定立即生效）；统一时间轴下钻面板显示
  项目来源并可一键「指定项目」（成功后自动重取时间轴）。可追溯三端同源：
  `/api/timeline?minutes=` 每区间返回 project_source（rule/manual/
  unassigned）与 project_evidence（manual=observed / rule=inferred），
  `/api/reports?days=` 每行返回 project_source；CLI `timeline` / `reports`
  展示来源列，新增只读子命令 `kynoptic project-overrides [--limit N]
  [--json]`（人工指定审计清单，只读打开不写库）。设置页 project_rules
  说明补人工指定优先级与审计语义。测试：core 4 项（人工指定优先于规则
  且规则命中保持 rule / 追加覆盖最晚生效且旧行保留可重建 / ai_run 人工
  指定不改区间 quality / editor_session 视图同标）+ dash 3 项（POST 全链路
  指定→重建→timeline manual/observed→二次指定 corrected_from=manual /
  校验面五类 400 且零落行 / 缺省 unassigned 三端贯通），时钟一律 _at 注入；
  CURRENT_SCHEMA_VERSION 17→18。

- F05 AI hook 真实事件通道补齐（PRD v1.0）：文件扫描降级为 inferred 后备，
  新增 observed 真实通道。core 新模块 `ai_hooks`（dash 与 CLI 共用同一校验
  与幂等路径）：`POST /api/ext/events` 接受 AI hook 事件（source =
  `claude_code` / `zcode`，扩展令牌鉴权与编辑器分支一致；dsh 无 hook/插件
  事件机制——调研 2026-11-03 `.dsh` 目录只有 workflow 产物、无 hooks 配置
  面——通道如实拒绝并说明走扫描）；单条事件 `hook_event_name`（事件词表
  映射 session_start/prompt_submit/tool_call/permission_request/tool_result/
  tool_error/notification/session_stop）+ `session_id`（1-128 字符）+
  可选 `tool_name` / `duration_ms` / `waiting` / `retry` / `timestamp`
  （hook payload 无时间戳字段——事件时间取 kynoptic 收到时刻，observed）。
  校验整批先验后写；幂等键 `ext_event_id="{source}:{payload_fnv64}"`
  （同一条 hook 事件重试重发 payload 字节相同 → 不重复写，migration 0017
  部分唯一索引）。写现有 events 表（event_type='ai_session'、
  source_id=工具源名、quality='observed'，不建第二真相表），落库后与编辑器
  分支共用同一节流重建。CLI 新增 `kynoptic ext-event --source
  claude_code|zcode [--session ID]`：stdin 读 JSON 直写 db，与 HTTP 完全
  同一校验与幂等函数；结果 JSON 打 stderr（hook runner 对 stdout 有输出
  协议，stdout 保持空）；库不存在不静默新建（写门禁与 db cleanup 同口径）。
  CLI 新增 `kynoptic ai-hooks render [--target claude_code|zcode|all]`：
  输出可直接安装的 hooks 配置片段（claude_code：settings.json hooks 键，
  PreToolUse/PostToolUse/Notification/Stop；zcode：config.json 顶层 hooks
  键，七个支持事件、`enabled:true`（配置文件 hooks 默认禁用）、不含
  Notification——官方文档明示不支持；命令指向本机 current_exe 的
  ext-event；dsh 不渲染并在 notes 说明）。ai_run 区间推导升级
  （activity_intervals）：hook observed 事件按 (source, session_id) 聚合为
  [最早,最晚] 事件 ts 区间（observed）；同会话两路证据并存时 observed
  优先、丢弃扫描 inferred 区间（不双计）；同源同设备重叠区间做并集合并
  （merged quality 取最弱端；跨设备不缝合）。安装 hooks 到用户真实工具
  配置属于安装验收阶段动作，本阶段未执行（标阻断）。
  测试：core ai_hooks 6 项 + activity_intervals 3 项（observed 聚合 /
  observed 优先不双计 / 并集合并+跨设备不缝合）+ dash 路由 4 项（鉴权/
  幂等/整批拒收/区间贯通，时钟一律注入）+ CLI 进程级集成测试 3 项（真实
  hook payload 经 stdin 子进程全链路：落库→幂等→rebuild 区间）。ZCode hook
  机制依据本机官方 zcode-guide diagnosing-hooks 文档（七事件词表、
  enabled:true 语义、Notification 不支持）；Claude Code 依据官方 hooks
  文档口径（payload 字段 session_id/hook_event_name/tool_name）。

- F04 编辑器扩展接入（PRD v1.0）：新增 `vscode-extension/`（TypeScript，
  VS Code + Cursor 兼容，`@vscode/vsce` 可打包 vsix）。监听
  onDidChangeActiveTextEditor / onDidChangeTextDocument（每文件 10s 去抖）/
  onDidChangeWindowState / debug 会话开始结束，产出 file_open /
  document_change / focus_gained / focus_lost / debug_start / debug_end
  真实编辑器事件（只发路径与元数据，禁读文件内容），批量（≤200/批）
  HTTP POST 到本机 dash。新 dash 端点 `POST /api/ext/events`：扩展令牌
  （data/ext-token.txt，serve 启动缺失即生成）鉴权、401 拒绝、Bearer 或
  `X-Kynoptic-Ext-Token` 头；schema 校验整批先验后写；幂等键
  `ext_event_id = "{source_id}:{event_id}"`（migration 0017 events 加列 +
  部分唯一索引，重发不产生重复行）；写现有 events 表
  （event_type='editor'、source_id='vscode_ext'/'cursor_ext'、
  quality='observed'，不建第二真相表），写连接 WAL + busy_timeout 5s 与
  tray 采集器并发安全（本 crate 唯一的 events 写路径，扩展持令牌才可写）。
  落库后节流（10s）重建 activity_intervals：core 新增
  `derive_ext_editor_intervals` 把扩展事件按焦点/文件/调试重放成
  `editor_session` 直证区间（闭段 observed；尾部未闭合按重建时刻收口
  unknown；文件路径参与 F06 project_rules 打标）——窗口名分类（dwell 路径）
  与扩展直证两路证据并存。设置页新增扩展令牌展示/重置区块
  （`GET /api/ext/token`、`POST /api/ext/token/reset`，重置后旧令牌立即
  失效）。测试：core 派生 2 项 + dash 端点 5 项（token/幂等/schema/区间/
  轮换，时钟一律注入）。真机 VS Code 验证未做（无真机环境），标阻断。

- F02 浏览器扩展接入（PRD v1.0 Stage 2）：新增 `crates/browser-host`
  （kynoptic-browser-host）Native Messaging host 本体——stdin 读 4 字节小端
  长度前缀 JSON，解析扩展事件（`.tab_change` / `.ai_session`），经 core 现有
  `Database::with_writer` 写入路径写入 events 表（不建第二真相表；append-only，
  `.tab_change` 以 `event_data.$.event_id`、`.ai_session` 以
  `event_data.$.event_data.session_id` 幂等去重，重复投递跳过不删行）；
  `source_id='chrome_ext'`、quality='observed'、device_id 复用
  `ai_adapter::ensure_device_id`（metadata 表同源机制）；timestamp 一律取事件
  自带 `ts_ms`（禁读真实时钟）。解析失败/字段缺失/oversize（>1 MiB）回
  `{"ok":false,"error":...}` 后安全退出，不 panic。单测 11 项（fixture 脱敏，
  时钟注入）。kynoptic CLI 新增 `host-register [--ext ID]... [--host-exe]
  [--manifest PATH]`：生成填写好的 host 清单写入 `%APPDATA%\kynoptic\` 并写
  HKCU 注册表键（`Software\Google\Chrome\NativeMessagingHosts\kynoptic_browser_host`，
  无需管理员；扩展 ID 32 位格式校验、占位符拒绝）。
  scripts/ 新增 `register-browser-host.ps1` 一键注册。扩展侧：manifest 补
  `nativeMessaging` 权限；background.js 的 `.tab_change` 事件补客户端
  `event_id` 幂等键（断线缓冲重发不产生重复行），与 host 的消息契约一致。
- F11 多设备维度（PRD v1.0 Stage 3）：migration 0016 为 activity_intervals
  派生表加 `device_id` 列（rebuild 从源事件原样抄写——dwell 取首现事件、
  away 取 idle_start 事件、ai_run 取事件行；NULL = 事件未带设备标识，不虚构
  本机 UUID）。/api/timeline（统一时间轴）每个区间带 `device_id`，响应带
  `device_boundary` 边界说明；/api/reports 行按 device_id 分组（无标识的行
  归入 "(unknown)" 分组标签），响应带 `devices` 列表与 `device_boundary`。
  本期不建同步服务：每库 = 一台机器，双机 human_presence / 任何时长**不在
  响应内相加**（跨机求和会把同时在线重复计数），多机合流由后续经 MCP /
  导出的流程完成。README 与 README.zh-CN 写明该边界。
- 三端对等暴露（PRD DoD：UI / CLI / MCP 同源同数值）：`api_unified_timeline_at`
  与 `api_reports_at` 两个只读纯函数从 kynoptic-dash 下沉到
  kynoptic-core::timeline（mcp 不能反向依赖 dash——dash 已依赖 mcp，成环），
  dash 保留同名 re-export，既有调用点不受影响。kynoptic CLI 新增
  `timeline [--minutes N] [--json]` 与 `reports [--days N] [--json]` 只读子
  命令（调用同一纯函数；--json 输出与 dash 端点响应体完全一致）。MCP 新增
  `get_unified_timeline`（minutes 1..1440，默认 60）与 `get_reports`（days
  1..90，默认 7）工具，均带可选 `now_ms` 显式时钟注入（可复现查询；0/负数
  硬错误，超限钳制）。工具面 7 → 9，schema 拒绝未知参数、协议往返测试更新，
  新增 get_unified_timeline/get_reports 的 tools/call 集成测试（now_ms 注入，
  不读真实时钟）。
- F03 ChatGPT 网页会话（browser-extension，PRD v1.0 Stage 3）：新增
  `content-chatgpt.js`（仅 chatgpt.com / chat.openai.com，弹窗显式 opt-in
  后由 `chrome.scripting` 动态注册——不写进 manifest，脚本内再校验一次开
  关；不读取对话内容/页面文字/标题，只监听 SPA 路由变化、可见性与页面卸载，
  上报会话开始/结束信号）；`chatgpt-session.js` 在 service worker 内按 tab
  配对 start/end，产出 `.ai_session` 网页事件（source=chatgpt_web，事件与
  event_data 双层 schema_version 标注，start/end_quality=observed，
  prompt_count=null）经既有 native messaging 通道上报；孤儿 start（worker
  重启 / 扩展关闭）不配对不虚构——时间轴保持 unknown 真实缺口。
  `popup.html` / `popup.js` 新增开关（默认关；关闭时立即注销并丢弃未配对
  起点）；manifest 0.2.0：权限补 `storage`（此前断线缓冲用到但漏声明）与
  `scripting`，host_permissions 仅 chatgpt.com / chat.openai.com；扩展
  README 更新事件示例与验证步骤。

### Added

- F06 项目归属（PRD v1.0 Stage 3）：settings.json 新增 `project_rules`
  （opt-in，默认空列表 = 不打标；上限 100 条，路径前缀/标题关键词 → 项目名，
  匹配顺序即优先级）。migration 0015 为 activity_intervals 派生表加
  `project` 列；`rebuild_activity_intervals_with_rules_at` 在重建时按规则给
  foreground_dwell / editor_session 区间打 project（证据 inferred——规则匹配
  是推导不是直证；未命中与 ai_run 区间恒 NULL：ai_session 事件不带标题/路径，
  无归属证据不猜）。POST /api/settings 接受 project_rules（校验上限/字段
  长度/全空规则拒绝），设置页新增规则编辑器（增删行 + 三字段输入）。
  schema 版本常量 CURRENT_SCHEMA_VERSION 14 → 15。
- F08 统计报表（PRD v1.0 Stage 3）：新端点 GET /api/reports?days=N（默认 7，
  上限 90，超限钳制 + days_requested/note 标注；`_at` 注入时钟）。按本地日 ×
  区间类别（foreground_dwell / ai_run / editor_session）× 来源 × 项目汇总
  时长，跨本地午界的区间按午界拆分归日。复审口径：estimated（含 inferred）
  与 unknown 段单列，不计入 observed 合计；project=(none) 表示无归属证据，
  不是零。dashboard 新增「报表」页签（天数 7/14/30/90 切换 + 三桶分列表格
  + 窗口合计行）。
- 统一时间轴 D 轨（Stage 3）改为 ai_run + editor_session 双源区间（此前只有
  ai_run）：每区间带 interval_kind / project / project_evidence 字段；前端
  按 source 分色（claude_code / zcode / dsh / 编辑器四色，unknown/inferred
  纹理底仍优先）；区间全空的窗口该轨标「未接入」（不隐藏不标 0）；下钻
  明细显示项目名与区间类别。

### Added

- F05 AIActivityAdapter（PRD v1.0 Stage 2，crates/core/src/ai_adapter.rs）：
  只读扫描三种外部 AI 源（本机真实格式实地调研后固化进模块文档）——
  Claude Code 会话转写（`~/.claude/projects/*/*.jsonl`，行级 timestamp）、
  ZCode 会话库（`~/.zcode/cli/db/db.sqlite` 只读打开，session 表 epoch 毫秒）、
  DSH 工作流产物（`<工作区>/.dsh/workflow-artifacts/dwfrun-*/`，仅 mtime，
  inferred；近 1 小时内改动按扫描收口标 unknown）。scan_ai_sessions_at(dirs, now)
  解析 session_id / source / start / end（证据词表 observed/inferred/unknown）/
  ai_run_elapsed（活跃段并集，不是求和）/ prompt_count；ingest_ai_sessions 写
  events 表 `event_type='ai_session'`（明细在 event_data JSON 含 schema_version，
  不建第二真相表），按 session_id 去重 append-only 幂等；device_id 首次生成
  UUIDv4 存 metadata 表（stage0-audit §5 决策 3）。测试 8 个（7 个脱敏自造
  fixture 单测 + 1 个 `--ignored` 真实源只读冒烟：本机实测 claude_code=7 /
  zcode=13281 / dsh=23 全部解析成功，绝不写源文件）；扫描幂等、断口收口、
  区间推导全部 `_at` 注入时钟；fixture 禁真实标题/路径。
- activity_intervals 新增两类区间推导（Stage 2，rebuild_activity_intervals_at
  全量重建口径扩展）：`ai_run`（从 ai_session 事件派生，quality 取区间最弱端
  证据：observed/inferred/unknown，无时间证据的事件行丢弃不虚构区间）与
  `editor_session`（F04 编辑器前台驻留分类视图：app_name 命中 code.exe /
  cursor.exe / windsurf.exe 等编辑器名单，source='windows'，quality 沿用
  foreground_dwell 口径——闭区间 observed、断线收口尾部 unknown）。F04 边界：
  本阶段只做前台分类，不做编辑器内部扩展/插件接入。
- 统一时间轴 D 轨（AI 活动）接入真实数据：/api/timeline 的 track_d 改为
  只读 activity_intervals 的 ai_run 区间（connected=true，移除「未接入」
  标注）；E 轨（网络遥测）仍为预留。测试更新走既有 `_at` 注入口径。

- browser-extension/ 骨架（PRD v1.0 F02，Stage 1）：MV3 事件驱动 Chromium 扩展
  （tabs.onActivated/onUpdated，无轮询无 content script，权限 activeTab/tabs/idle），
  tab 事件经 chrome.runtime.sendNativeMessage 以 .tab_change 上报，host 不可达时
  chrome.storage.local 缓冲（上限 500 条）按序重发；native-messaging/ 含 host
  清单模板（allowed_origins 占位）与 Rust host 实现说明（走现有 db 写入路径写
  events 表；本期只交付文档，不写 Rust 代码）。
- 统一事件结构（0014 迁移，PRD v1.0 Stage 1）：events 表加列 source_id /
  device_id / quality，存量行回填 source_id='windows'、quality='observed'
  （append-only，不删不重排；不建第二真相表）。CURRENT_SCHEMA_VERSION 13→14。
- activity_intervals 派生区间表（可由 events 全量重建，不进真相链）：
  interval_kind（foreground_dwell / tab_foreground_dwell（Stage 2 预留）/ away）、
  start_ts / end_ts / app_name / title_hash / quality / source_id，及
  rebuild_activity_intervals_at(conn, now) 重建函数。口径（stage0-audit §2/§4）：
  连续同 (app,title) 事件只刷新不产生新区间（修复重复事件多算时长）；与
  away（idle_start/idle_end 对）求交标 estimated；尾部断线按重建时刻收口标
  unknown（unknown 是真实缺口，不静默归 0）。单测全部 `_at` 注入时钟，
  fixture 按 sample-30min.json 真实模式（同题重复、整点刷新、idle 交集、
  断线收口）构造。
- 统一时间轴端点 `GET /api/timeline?minutes=`（PRD v1.0 F07，Stage 1）：
  只读 activity_intervals 派生表（默认 60、上限 1440 分钟，超限钳制并标注
  minutes_requested），按轨道 A-E 分组返回区间——A 前台窗口、C 在场/away
  为真实数据；B 浏览器 tab 无数据源时整段 unknown 并标注「未接入」；
  D AI 活动 / E 网络遥测本阶段留空 + 「未接入」。每条区间含 quality 与
  证据等级（PRD §2.3 词表：observed / inferred / unknown，unknown 不是零）。
  与既有 `?hours=` 小时桶端点共用路由、按参数分流，向后兼容。dashboard
  新增「统一时间轴」视图：横向分轨渲染、unknown 纹理底、30 分钟-24 小时
  缩放、点击区间下钻明细与证据等级。测试 4 个（轨道分组聚合、窗口过滤
  裁剪、路由参数契约、视图注册），全部 `_at` 注入时钟。

## [0.8.1] - 2026-11-02

深度并行审查（测试卫生/代码规范/API 文档一致性/DB 运维面四路）产出的标准化迭代；
另含用户明确要求的自体遥测与 0.8.0 两处「写了但没接」的接线交付。

### Added

- 自体遥测（A1）：self_metric_minute 分钟桶表（0013 迁移）+ 采集循环每分钟
  连续记录自身进程 CPU/内存/线程/句柄（M1 MetricPoint 契约）+ `/api/self-metrics`
  查询面。此前自身消耗只能靠全机 Top5 旁路采样偶尔看到（10 月内存 194→350MB
  爬升一个月才被发现）。
- ETW 增强模式接线（A2）：settings `net_etw_enabled`（默认关）→ NetEtw 采集
  线程（KynopticNetMon 会话 → net_process_minute 落库）。0.8.0 该表恒空、
  归因面恒 Unavailable（模块+读路径已建，写路径缺位），本版补齐唯一生产写入方。
- 诊断模式接线（A3）：`POST /api/diag` start/stop（硬上限 30 分钟钳制）；
  采集侧消费档位（采样 30s→5s 实时生效、ETW 缓冲 64→256KB 建会话时生效）；
  预算巡检喂自体实测（超限强制回退）。0.8.0 档位/控制器零消费方=不可达路径。
- 端点清单补齐：skill.md 与 lib.rs 头注释补入 self-metrics 与 0.8.0 漏记的
  net-process/net-derived/sys-overview/resource-top/diag-timeline（自家
  「路由表与 skill.md 两处同步」规则此前破约）。

### Changed

- 心跳阈值配置化：KYNOPTIC_STALLED_SECS（≥900，缺省 1800）/ 
  KYNOPTIC_MAX_AGE_SECS（≥60，缺省 180）环境变量可覆盖，双机调阈值不再重编译。
- CLI 可见错误统一双语出口（`bilingual()` 此前零接线，双语铁律落空）。
- `⚠` 符号全部换 `[!]`（Windows 终端把 ⚠ 渲染成彩色 emoji，违禁 emoji 铁律）。

### Fixed

- CLI `db cleanup` 补删 agg_minute_actor / net_process_minute / self_metric_minute
  （此前 CLI 路径漏删留孤儿行；core 侧 S1.1 修过的问题在 CLI 路径复发）。
- VACUUM 满盘失败写 disk-paused.flag（此前只有日志无用户可见面）。
- 测试时钟竞态×2：input_agg 激活分钟与断言分钟两次读钟跨分钟边界随机红；
  mcp summary 历史日期基准两次读钟跨本地午夜同日。均改单锚/注入
  （同 0.8.1 前哨 PR #75 的 api_status 24h 窗口日历炸弹根因族）。
- mcp-wait spawn 失败降级留档+跳过（原 expect 会让整条 stdio 服务 panic）。
- CHANGELOG [0.8.0] M2/M4.2 两条如实修正（0.8.0 实际交付范围与 0.8.1 接线说明）。
- 过期注释与横幅：dash stdout 横幅只回 db 文件名；「Wave31 挂账」注释修正
  （访问令牌已实现）。

### 移交后续（如实）

- 临时目录命名两范式并存（pid+seq vs 纳秒）：纯风格、不触断言，0.8.2 统一。
- categories 分类规则暂只能手改 settings.json（契约三方一致，仅无设置页 UI）。
- DU 本机管理员身份的 ETW 等价实采验证仍为 Pending-Admin（A 机隔离会话已验
  100MiB 口径）。

## [0.8.0] - 2026-11-02

可观测性升级（obs-m 系列，feat/obs-m 分支 PR #72）。

### Added

- 统一指标契约（M1）：`MetricPoint` 12+1 字段信封（schema v1）——metric_id
  三段命名、`source`（Etw/Derived/Provider）、`quality`（Confirmed/
  Estimated/Incomplete/Unavailable；未采到不是零、估算不是实测）、
  `process_identity`（pid+exe_start 防复用误绑）、loss/coverage 显式字段。
- ETW 内核网络流量（M2）：专用会话 `KynopticNetMon`（Microsoft-Windows-
  Kernel-Network）——会话管理器状态机、内核事件解析 + 分钟聚合、进程身份
  关联（PID→进程名+启动时间，含短命进程区间归属与 PID 复用防护）、
  `net_process_minute` 持久化（0012 迁移）与 `/api/net-process` 查询面、
  受控传输验证（隔离会话 100MiB，production 不受影响）。
  如实注：本版仅模块+读路径，采集进程接线（settings 开关 + NetEtw 线程）
  于 0.8.1 交付，0.8.0 运行时 net_process_minute 恒空。
- 代理归因链（M3）：代理进程与监听端口识别（GetExtendedTcp/UdpTable +
  可配置进程名模式，证据层不做假设性归类）；四类流量派生（直连/代理相关/
  代理外层/口径模糊，四独立 metric_id 永不互加；TUN/多路复用降 Estimated
  并带原因）；网络归因面板视图（五分区，每分区带 Confirmed/Estimated/
  Unavailable 徽标与原因）。
- 资源异常模型（M4.1）：`ResourceAnomaly` 统一事件（滚动基线近 7 天同
  小时分位 + 绝对阈值双门限防误报；流量激增/持续上传/CPU 持续+周期尖峰/
  内存斜增/磁盘吞吐/短命进程反复/单父扇出/通道事件检测器集；只检测记录，
  不自动杀进程/改代理/断网）。
- 诊断模式（M4.2）：默认关；本版交付档位/控制器与设置页展示骨架，
  开启入口与采集侧接线（ETW 缓冲 64KB→256KB、采样 30s→5s、硬预算强制收口、
  到期自动回落 Basic）于 0.8.1 交付（审查发现 0.8.0 接线不在，如实修正）。
- 三视图（M4.3）：系统总览（CPU/内存/GPU/网络/关键异常/监控健康）、
  资源排行（按进程网络 Top20，无名不伪造）、诊断时间线（锚点关联事件/
  资源变化/目的地址可得性）；无持久化源的维度一律显式 Unavailable+原因。
- 数据保留策略（S1.1 骨架）：**默认关闭**，用户在设置中显式开启才生效；
  清理覆盖 events 与 actor 证据缓存盲区。

### Changed

- ETW 启用路径勘误：内核网络供应商真实 GUID
  `{7DD42A49-5329-4832-8DFD-43D979153A88}`（旧 GUID 未注册致 EnableTraceEx2
  错 87）；内核供应商走 EnableTraceEx（EnableTraceEx2 对内核供应商恒 87，
  24 组合探针矩阵证实）。启动链中途失败显式收口内核会话；运行中丢失数经
  EVENT_TRACE_CONTROL_QUERY 可见。
- CI/门禁：dashboard 内嵌 JS 由 check-js.mjs 语法+逻辑断言双护（M3.4/M4.3
  徽标与渲染逻辑 vm 求值）；推送经 ls-remote 核验流程固化。

### Pending-Admin / Limitations（如实）

- 本机（DU）为非管理员、UAC 确认被策略自动取消：ETW 实采与端口表验证在
  A 机（管理员 ssh 会话、隔离会话名/隔离端口）完成；本机管理员身份下的
  等价实采验证仍为 Pending-Admin。
- 套餐用量（plan 分区）恒 Unavailable：本机无可信套餐计量面（核心数据口
  非 HTTP 面、配置加密、本地库无用量表）；禁止用进程 I/O/网卡累计/连接数
  伪造，订阅端点 subscription-userinfo 头为后续远程接入候选。
- 直连/代理相关（direct/via_proxy）流级拆分待 M3.2 ETW 派生全量接线，
  当前显式 Unavailable 不硬拆；分钟聚合不含远端五元组，目的地址不可得。
- 按进程 CPU/内存/磁盘无持久化数据源：资源排行仅网络维度出数，其余维度
  显式 Unavailable（未采不伪造）。

## [0.7.0] - 2026-10-31

### Changed

- Release pipeline: `release.yml` checkout now uses `fetch-depth: 0` so
  the empty-tag-commit gate can see the parent commit (a shallow clone
  made `git diff-tree` report 0 files for every tag and would have
  failed every future release). Removed a duplicated doc-version step.
- CI: the empty-commit guard on main pushes is now scoped to `push`
  events only, and the doc-version consistency gate runs there too.
- Installer (`kynoptic.iss`): the `schtasks` fallback for the watchdog
  scheduled task now queries first, keeps an existing task untouched
  (with a loud log line `existing task kept, NOT overwritten`), and only
  creates the task when it does not exist. The COM registration path is
  unchanged.

### Added

- `.github/PULL_REQUEST_TEMPLATE.md`: four-section template, including
  the expected file set of the change.
- `ENGINEERING.md` §7: mechanical-gate inventory with real close-out
  notes (merge-queue remains off during single-maintainer phase; the
  main-push empty-commit gate is the interim guard).

### Fixed

- `Cargo.lock`: workspace member versions synced to 0.6.0 (lock had
  drifted behind the manifests).

## [0.6.0] - 2026-10-30

### Added

- Dash (settings): `redact_titles` privacy switch, default off (opt-in).
  When enabled, URL query strings are stripped from window/tab titles
  before they are persisted, so token/session ids no longer land in the
  database. The wiring spans AppSettings -> tray/CLI CollectorSettings ->
  collector -> `title_privacy::set_redact_titles`; it affects new writes
  only, historical rows are untouched.
- Dash (API): `/api/report` gains `presence_minutes` (human presence
  minutes before the 20-minute threshold filter), letting the report page
  distinguish "no presence data in the baseline window" from "presence
  data but no 20-minute focused block" instead of rendering the same
  empty state for both.
- Core (input devices): raw-input collections are merged into physical
  devices with a device category; the input page aggregates per physical
  device and no longer shows raw vendor codes for unknown vendors.
- CI (docs): `check-doc-version.mjs` now anchors code-derived facts
  (the CLI subcommand list) against the docs to stop transcription drift.
- Core (tests): a new `spawn_no_window_scan` test locks the invariant
  that every product-side `Command::new` goes through
  `spawn::no_window()`.

### Changed

- Docs: the CLI surface is corrected to 21 subcommands in `llms.txt` and
  APP.md (`apps`, `topics`, `recompute-actors` had been missing since
  v0.4.0); the "pure Win32 APIs, zero subprocesses" claim is toned down
  to the accurate statement (the network monitor may fall back to
  spawning `netstat.exe` when the native path is unavailable, bounded at
  3 s); the uninstall-keeps-data behavior is now disclosed in both
  READMEs, APP.md, `llms.txt` and the bundled SKILL.md.

### Fixed

- Core (db): mixed minutes (raw and input_agg events inside the same
  local minute) aggregated inconsistently - the incremental cache took a
  MAX snapshot while the full rebuild sums both forms (probed 5 vs 7
  keys), the production write path could never trigger the recompute
  because input_agg rows report rowid 0 to the agg layer, and the daily
  heal check could miss a diverged day in continuous sessions. The
  rowid/aggregate/heal contracts are now unified end to end: incremental
  and rebuild agree row-for-row in mixed minutes and the heal check can
  no longer miss a diverged day. The external contract (input_agg rows
  always report rowid 0) is preserved and pinned by tests.
- Core (queries): presence in raw mode - the two authoritative paths
  (`collect_day_hits` and the capped branch) now agree on mixed minutes
  (human raw activity constitutes presence; injected-only minutes do
  not).
- Dash: the anomalies tab red badge was never visible since its
  introduction - assigning `display=""` only cleared the inline style and
  fell back to the stylesheet's `display:none`; the visible branch now
  sets `inline-block` explicitly.
- Dash (API): `/api/input` daily-compare series now zero-fills missing
  days to the requested window length (a 7-day window could previously
  return 5 rows with date gaps), matching the documented contract.
- Update: installed-form detection now matches any `unins\d+\.exe`. Inno
  Setup increments the uninstaller number on reinstall, so machines with
  only `unins001.exe` were misdetected as portable - which disabled the
  `kynoptic update` refuse-gate for installed form and made the tray's
  one-click update take the portable in-place path. Detection now lives
  in one place (`kynoptic_core::install`) shared by CLI and tray.
- Update/CLI: two remaining raw `Command::new` call sites go through
  `spawn::no_window()`; the tray `pick_free_port` test race (flaky under
  parallel port pressure) is fixed while keeping its cross-range retry
  semantics.
- Build: Cargo.lock regenerated after the 0.5.0 version bump - workspace
  members were still pinned at 0.4.0, breaking `--locked` builds.

## [0.5.0] - 2026-10-29

### Changed

- Core (monitors/ps): PowerShell-backed collection now runs through a
  persistent single-process host instead of a cold `powershell.exe` spawn
  per sample — scripts are delivered base64-over-stdin (UTF-8 exact),
  results delimited by a per-call sentinel, each call bounded by the same
  60s deadline; a hung host is killed and lazily respawned, and if the
  host cannot start at all the engine permanently falls back to per-call
  spawn. With all 40 monitors enabled, PS process churn drops from
  ~9 powershell spawns/min (isolated baseline) to one resident host.
  `run_ps` / `run_ps_detailed` signatures and tri-state semantics are
  unchanged.
- Core (monitors/ps): the per-call spawn fallback now reads stdout
  concurrently with the child lifetime; output larger than the pipe
  buffer no longer wedges both sides into the 60s timeout (latent
  deadlock, fixed).
- Core (collector): monitors with a PowerShell dependency are throttled
  through a 4-permit semaphore covering both the first sample and the
  steady-state loop, capping the startup burst (previously ~13 monitors
  raced their first collect within seconds); a monitor whose collect
  consistently overruns its own interval backs off exponentially
  (interval x 2^n, capped at 600s) and resets on a normal pass.
- Dash: `/api/status` gains a per-monitor `monitors` array
  (`last_success_age_secs`, `last_duration_ms`, `consecutive_slow`);
  the settings view shows one line while any monitor is in backoff, and
  tolerates older backends without the field.

### Fixed

- Core (registry): dependency labels corrected after a full 40-item audit
  — `ime` (natively rewritten, registry API only) and `location`
  (collects nothing) are now `native`; the three deviations the binary
  Native/PowerShell split cannot express (network `netstat.exe` fallback,
  device optional disk-io WMI query, gpu per-tick `nvidia-smi.exe`) are
  documented next to their registry entries.

### Notes

- Default 14-monitor set behavior, presence/automation/foreground
  semantics, and privacy boundaries are unchanged.
- BENCHMARKS.md gains an all-40 isolated baseline measured on this code
  line before the refactor; post-refactor numbers are backfilled only
  after the released build is measured on real machines.

## [0.4.0] - 2026-10-25

### Added

- Core (db): the actor evidence cache `agg_minute_actor` (schema migration
  0011) stores per-minute input evidence counts only — total/injected keys,
  clicks, and scroll ticks — maintained by the same three paths as the
  existing aggregates (same-transaction incremental writes, full rebuild,
  chunked backfill/heal) with row-level parity tests. Actor labels, AI-channel
  flags, and foreground coverage are intentionally NOT stored: they are
  derived in the query layer from window-switch intervals (joint review
  contract 0.4.0, correction 3). Existing `agg_minute`/`agg_daily` rows and
  their semantics are untouched. `kynoptic-ctl recompute-actors
  [--from/--to] [--dry-run]` rebuilds this derived cache only; raw events
  are never modified.

- Core (queries): `foreground_actor_dwell` classifies foreground coverage
  into five mutually-exclusive per-minute actors — `user` (human input),
  `agent` (injected input; a proxy metric with `evidence_type=injected_input`,
  not a claim that an AI did it), `mixed`, `unattended` (foreground, no
  input), `unknown` (uncovered minutes: collection gaps, pre-first-switch,
  stalls beyond the 2h cap — never silently merged into other classes).
  Per-app seconds are split by foreground-overlap allocation and reported as
  `estimated_actor_dwell_sec` (an estimate, not precise engaged time).
  A `browseros` channel column (configurable title-suffix rule, default
  " - BrowserOS") is auxiliary evidence only and never overrides the five
  classes; `ai_suspect = agent ∪ (browseros ∧ unattended)` is a clearly
  labeled speculative composite. The 0.3.2 `foreground_dwell_by_app`
  behavior is unchanged.

- Core (queries): `topics_summary` — window-title topic aggregation with
  explicit source labels (`title` today; `domain` stays empty until events
  carry verifiable URLs — never inferred from titles). Sensitive titles
  (login/token/credential/password patterns) collapse to
  `<redacted:sensitive>` and never surface as plaintext, including on the
  explicit `--raw` path; non-sensitive keys are reported at main-title
  granularity by default.

- CLI: `kynoptic apps [--days N] [--actor CLASS] [--json]` — per-day per-app
  actor dwell estimates with unknown/unattributed seconds shown explicitly;
  `kynoptic topics [--days N] [--raw] [--json]` — topic aggregation with
  redaction defaults; `kynoptic recompute-actors [--from YYYY-MM-DD]
  [--to YYYY-MM-DD] [--dry-run]` — explicit one-shot actor-cache backfill
  (never touches events, never runs automatically).

- MCP: `get_top_apps` responses now include the per-app actor split
  (`estimated_actor_dwell_sec`, `agent_evidence_type`, `browseros_sec`) plus
  `unattributed_sec` and the method note; new `get_topics` tool (default
  redacted keys, `raw` opt-in still filters sensitive titles), bringing the
  tool surface to seven.

### Fixed

- Core (monitors/gpu): the static GPU info query (Win32_VideoController via
  PowerShell) is now cached with a 10-minute TTL. Model/driver/resolution
  fields were re-queried every 10 s on the hot cadence meant for dynamic
  metrics — with all monitors enabled this was a PowerShell cold start every
  10 s for data that essentially never changes; it now runs 0.1 times per
  minute. Dynamic metrics (nvidia-smi) keep the 10 s cadence, now under a
  3-second bounded wait so a hung nvidia-smi can no longer stall the
  collection thread. (Ops review P1/P3.)

- Installer: the watchdog scheduled task is now registered via the
  Task Scheduler COM API instead of `schtasks /Create /SC MINUTE`, whose
  default XML carries `DisallowStartIfOnBatteries=true` — on battery power
  the watchdog silently never ran, and there was no logon trigger. The new
  registration removes both battery restrictions, adds a logon trigger plus
  the 1-minute repetition, sets IgnoreNew and a 10-minute execution limit,
  and still falls back to the old `schtasks` path if COM registration
  fails. (Ops review P2.)

- Installer: rebuilding the watchdog task no longer silently overwrites an
  operator-applied `wscript` hidden-launch wrapper. The installer now
  detects a wrapped action on the existing task and preserves it verbatim;
  unwrapped installs get the bare exe as before. (Ops review P1.)

- Core (registry): the `location` monitor description now says plainly that
  it is a placeholder collecting nothing (the historical ipinfo.io public-IP
  lookup was removed under the privacy红线); it no longer reads as a real
  sensor. (Ops review P3.)

## [0.3.2] - 2026-10-24

### Changed

- Core (queries): foreground app dwell now credits collection-stall gaps at
  the stall cap instead of discarding them entirely, and the dashboard
  overview credits the trailing (still-foreground) segment. A day with a
  single window switch used to report 0 seconds of foreground dwell — a
  3-hour continuous session was invisible. CLI presence summaries keep the
  conservative no-tail behavior (joint review P1).

- Core (db): the per-app daily aggregate (`agg_daily` `app:*` buckets) is
  now written by the incremental path with the same predicate as the
  rebuild/backfill path (any keyboard/mouse/window row with a non-empty
  app name, not just window switches). Browser tab-change rows were only
  counted after a rebuild, so incremental and rebuilt aggregates diverged
  and new-app surge baselines jumped after a heal. `rebuild_agg`'s full
  rebuild also runs inside a single transaction now, so concurrent readers
  never observe the deleted-but-not-yet-rebuilt intermediate state (joint
  review P1/P2).

- Core (ingest): events whose timestamp is not valid RFC 3339 are now
  rejected at the write side instead of being written and tolerated at
  read time. This includes empty-string timestamps. Rejected events are
  counted (`UNPARSEABLE_REJECTED`) and logged so the watchdog surfaces
  them — they are not silently dropped — because such rows could never
  land in any date bucket (the `events.timestamp` range predicates are
  plain string comparisons) and only polluted the database. Historical
  rows are not cleaned up; read-side tolerance still covers them. This
  complements the existing future-timestamp gate (`FUTURE_REJECTED`);
  both now run in the same write-side filter.

- MCP: the `wait_for` tool's timeout is now parameterized via
  `timeout_sec` (defaulting to the previous 300 seconds); `0` is a hard
  error. `wait_for` is no longer accepted inside a batch request — it
  must be sent as a standalone request so it can run on the background
  thread under the wait cap without stalling other queued calls.

### Added

- Core: the session monitor now receives Windows session events
  (lock/unlock, logon/logoff, session disconnect) through WTS session
  notification (`WTSRegisterSessionNotification` +
  `WM_WTSSESSION_CHANGE`) on a dedicated hidden-window message thread;
  events are queued and forwarded on the next collect. The 5s
  `OpenInputDesktop` polling is kept only as a fallback (unlock
  recovery; and the legacy two-tick debounce when WTS registration
  fails, e.g. on non-interactive sessions).

- Core (db): the schema version is now also written to
  `PRAGMA user_version` (an integer stored inside the database file,
  visible to external tools) and read back as the authoritative
  source; older databases that only carry the metadata-text version
  fall back to it, and a metadata parse failure now warns explicitly
  instead of silently treating the version as 0 (which would trigger a
  full, hard-failing migration replay).

- Core (db): read-connection pool exhaustion is now counted. When no
  pooled reader is free, queries degrade to temporary connections and
  the cumulative degradation count is exposed
  (`reader_degraded_count`) for monitoring — a steadily growing value
  means a read connection is being held too long or the read load
  exceeds the pool size.

- Core (db): `Database::wait_for_backfill(timeout)` lets callers
  (tests, aggregation readers) block until background backfill
  completes instead of polling, returning false on timeout.

### Fixed

- Core (input_agg): a restart within the same minute no longer overwrites
  the previous session's partial input snapshot. The new session seeds its
  counters from the existing rows, so snapshots and minute finals carry
  "old partial + new" merged counts. Previously a 30-key partial snapshot
  could be overwritten by a 2-key restart while `agg_minute` kept the stale
  MAX — events, aggregate cache, and truth split three ways (joint review
  P1).

- Core (collector): the writer's shutdown path now retries the final batch
  with bounded backoff and archives the failure to `collector-error.log`
  instead of ignoring the write result — under a persistent disk/lock
  failure the tail batch used to vanish silently with the thread. A
  partially-failed degraded batch now retries the failed ordinary rows
  individually instead of miscounting the batch as fully landed (joint
  review P1).

- Core (monitors/window): foreground dedup state is now reset when a
  collector session ends, and a failed channel send rolls the dedup state
  back. Previously a hot-reload/restart lost the first foreground segment
  (window unchanged → never re-emitted), and a full event channel
  permanently swallowed that switch (joint review P1).

- Core (db): the agg-cache completeness check now counts only input
  buckets on the `agg_minute` side. Switch-heavy days inflated the cached
  minute count and masked missing input minutes, sending charts down the
  cache path with partial data (joint review P1).

- Core (queries): raw mode now fills the automation bucket — pure injected
  minutes (SendKeys-style automation) previously reported 0 automation
  because only the human side was collected in raw mode. The human
  minute classification also clamps per-term subtractions at zero (joint
  review conditional P1/P2).

- Core (monitors/mouse): XBUTTON (mouse side-button) clicks are now
  captured in raw mode; they were only counted in minute mode.

- Core (monitors/network): `netstat -e` now runs under a 3-second bounded
  wait; a hung netstat used to freeze the monitor thread silently.

- CLI (export): `--redact` now also sanitizes the `title` field embedded
  in the `event_data` JSON. Window/browser rows carry the title twice
  (column + JSON) and the JSON copy used to leave the raw title in the
  exported file.

- CLI: the daily report generated by `kynoptic report` no longer carries the
  legacy upstream product name in its title; the title is now
  "# Kynoptic · {date} 报告", matching the product branding used in all
  other user-visible output.
- Dashboard: watchdog alerts now have a "cleared" counterpart. The
  collection watchdog and the tray stall detector each record a
  `collection_recovered` notification event when collection resumes.
  On the CLI side the recovery gate is a persisted pending-alert marker
  (stall-family alerts only), so it survives the per-minute fresh
  `watchdog --once` process and fires on the first healthy heartbeat
  after an alert — including the watchdog-killed-tray restart path;
  user-initiated exit and update-in-progress alerts do not set it. The
  tray side fires on the stall falling edge, only when the collector is
  actually running and writing again. And
  `/api/status` now pairs the newest event within 24h: if it is a
  recovery event the banner flips to a neutral "alert cleared (time)"
  message instead of showing "collection is hung…" for up to 24 hours
  while the tray icon and heartbeat JSON already reported healthy.
  `/api/status` also exposes the event's `at` timestamp for the cleared
  time display.
- Dashboard: the keyboard heatmap now closes its key-set accounting.
  VK 13 is shared by the main Enter and the numpad ⏎; the count is now
  rendered on one key only and the other is annotated "counted with
  Enter", removing the DOM double-count. Virtual keys present in
  `key_freq` but outside the ANSI-104 layout (e.g. some media keys) are
  no longer silently invisible — a note below the map reports how many
  presses and distinct keys fall outside the layout.
- Dashboard: the daily heatmap (12/26/52-week views) now renders month
  abbreviation labels above the first column of each month (GitHub-style),
  alongside the existing weekday labels.
- CLI (`db stats`): the database integrity check (`PRAGMA quick_check`)
  now runs before the counts are printed. A degraded database gets a
  prominent warning and a non-zero exit instead of all-zero
  events/sessions/ghosts counts with exit code 0, which let users and
  scripts read "the database is broken" as "there was no activity".
- CLI (`export`): all three formats now write to a temporary file in the
  target directory and atomically rename it on success. A mid-write
  failure (e.g. disk full) leaves a `.partial` file next to the target
  path and exits non-zero; previously a truncated file landed at the
  final path while the command still printed "✓ 导出 N 行" and exited 0.
- Tray: when started with a random dashboard port (`--port 0`), the
  actually bound port is now written to `dashboard-port.txt` and the
  tray menu's "open panel" entry uses that real port; previously the
  file was never written and the menu entry was a dead link on a fresh
  install.
- Dashboard: host/infrastructure process names (e.g. `applicationframehost.exe`,
  `textinputhost.exe`, `conhost.exe`) no longer appear verbatim in the top-app
  ranking, timeline, report, or the "top app" KPI. They are now mapped to
  short, readable labels ("UWP", "IME", "Console"), so a UWP-host process
  can no longer head the ranking as an opaque raw name.
- Dashboard: category-rule matching now matches ASCII tokens as whole
  words (word boundaries) instead of bare substrings. A token like
  `edge` no longer matches inside `ledges`, and `qq` no longer matches
  inside `qquick`/`quick`-style titles; Chinese (CJK) tokens keep their
  substring semantics (no word boundaries in Chinese). The `文档`
  default rule gained `winword`/`powerpnt` so Word/PowerPoint still
  classify by process name under the tighter matching.
- Dashboard (`POST /api/settings`): the response's `ignored` array no
  longer misreports accepted fields. `update_check` and `low_power`
  are accepted and persisted, yet were listed in `ignored` (the
  settings page sent both on every save, producing two spurious
  entries per save), while `monitors` — a response-only key — was
  excluded from the list; one response thus echoed effective values
  while claiming they were ignored. The accepted-field set is now a
  single constant shared by request handling, the settings payload,
  and the `ignored` computation; `monitors` sent by a client is now
  correctly reported as ignored.
- Dashboard: transport-level rejections now use the same
  `{"error","code"}` JSON as the other error responses (400/401/403/404/405).
  413 (payload limit) adds `code: payload_too_large`, 503 (connection
  cap) now sends `code: service_unavailable` instead of a plain-text
  body, 431 sends `code: header_too_large`, and the loopback-Host 403
  sends `code: forbidden_host`; previously 503/431/403-host were
  plain text and 413 had no code, so programmatic consumers saw raw
  English strings. The settings page maps the new codes to its
  bilingual error text.

- CLI (`update`): self-update now verifies before touching anything: the
  release manifest (`SHA256SUMS.txt`) plus all five exes are downloaded
  and every file's SHA-256 is checked, and an update whose manifest is
  incomplete (missing asset or `SHA256SUMS.txt` entries) is refused —
  `--check` validates manifest content, not just asset names.
  Replacement is two-phase: all old binaries move to `.bak` first, new
  files are placed second, and any failure rolls the whole set back
  from `.bak` (per-step results checked; a failed rollback is reported
  as such). An orphan-`.bak` recovery scan runs at update entry and
  before the watchdog restarts the tray.

- CLI (`watchdog`): the tray-exit marker (`tray-exit.flag`) is now a
  one-time status record instead of a repeated fault alert. A persistent
  latch (per-flag-file mtime key, kept in watchdog state) guarantees
  each marker file is recorded exactly once across the per-minute
  `watchdog --once` processes.

- Tray: a user-initiated pause now survives a tray restart. Pausing
  writes a `paused.flag` marker; on startup the tray reads it and keeps
  collection stopped — icon, tooltip, and menu all show the paused
  state instead of silently resuming after a crash-restart or boot.
  Only an explicit "resume" from the tray menu clears the marker.

- Tray: the "resume collection" menu item in the error state now labels
  which subsystem failed (collector vs dashboard), and "open panel"
  re-points to the data directory (where `dashboard-error.log` explains
  the failure) when the dashboard itself is down.

- Tray: scheduled-task diagnostics no longer embed raw schtasks stderr —
  on Chinese systems it is encoded in the system code page (GBK) and
  arrived as mojibake. Failure messages now report the exit code plus a
  plain hint ("task missing/disabled, blocked by group policy, or the
  Task Scheduler service is not running").

- Tray: the tray icon is now sized against the DPI of the monitor that
  hosts the taskbar (`GetDpiForWindow` on the shell window +
  `GetSystemMetricsForDpi(SM_CXSMICON)`) and redrawn on DPI change
  (`WM_DPICHANGED`); with different scaling per monitor the small icon
  was previously enlarged with blur.

- Core: when a batch event insert fails under external write-lock
  contention, it degrades to sub-batch transactions (50 rows/txn, ~6
  commits per 300-row batch) before falling back to per-row retries —
  observed WAL growth under contention drops from ~15–20x to ~3x.

- Core: the notification dedup high-watermark is now persisted in the
  database metadata table (same store as the events watermark and
  clipboard salt), so restarts and separate processes agree on what was
  already notified; the watermark is read from / written to the
  database the collector is actually writing, so one process can no
  longer pollute another database's dedup state.

- Dashboard: the 60s response caches of `/api/report` and
  `/api/anomalies` now key on a settings epoch (bumped on every
  successful `POST /api/settings`), so saving settings takes effect in
  those views immediately instead of serving stale cached pages.

- Tray: a left-click double-click on the tray icon no longer opens two
  or three duplicate dashboard tabs. The hardware double-click fires
  multiple mouse messages and each one opened the panel; a 1-second
  merge window now collapses the gesture into a single open (single
  click and double click share the same "open panel" action).

- Tray: error-state recovery no longer flickers. Clicking "resume
  collection" while any failure flag is active (collector failed,
  dashboard failed, database degraded, or collector stalled) no longer
  optimistically flips the icon back to green — the timer would flip
  it straight back, producing a green/yellow blink. The icon only
  turns green once the flags are actually clear. Collector spawn
  failures are likewise now archived persistently, raise the
  collector-failure flag (surfaced in the tray error state), and are
  retried on every refresh instead of being silently dropped.

- Core (activity monitor): activity on UAC-elevated processes is no
  longer swallowed as "(unknown)". When neither `OpenProcess` nor the
  process snapshot can resolve a high-privilege process's exe name
  (the process already exited, or its owner handle is inaccessible),
  the event carries an explicit `<elevated>` marker instead of an
  empty app name, so the activity is still visible and countable.

- CLI: a dangling `--db` (or other flag missing its value) is now an
  explicit error. Previously a trailing valueless flag was silently
  ignored and, for date windows, the command fell back to "today" and
  exited 0 — reporting success for a command the user never actually
  asked for.

- CLI: failing to open the database read-only (file header is not
  SQLite — corrupted or not a database file) now reports the recovery
  path (stop tray/collector, restore from a backup over the original
  path) instead of a bare driver error.

### Changed

- CLI (db): write-lock contention errors (`SQLITE_BUSY`/`SQLITE_LOCKED`)
  are now reported with a unified "loser" wording across all three
  write paths (`cleanup`/`vacuum`/`checkpoint`/`aggrepair` and the
  sessions delete): instead of a bare "database is locked", the error
  names the likely lock holder and adds a "stop the tray, then retry"
  hint. Read-only commands (`db stats`, reports) were also switched to
  true read-only connections — zero writes, no migrations, no 5s
  busy-timeout stall when the writer holds the lock.

- Dashboard: every `/api/*` app name is now normalized by a single
  display-layer mapping (`.exe` stripped, host → friendly label, common
  apps → friendly name), so `msedge.exe` shows as "Microsoft Edge",
  `qq.exe` as "QQ", `notepad.exe` as "Notepad", etc. The `Top apps`,
  `report`, `apps grid`, `daily top`, and `insights` endpoints and the
  page tooltips all use the same normalized names (previously the raw
  process name was shown in some tooltips). Counting and ranking are
  unchanged — the mapping is one-to-one and idempotent.
- Docs: the version-check byte-volume description is corrected. The
  once-daily check performs a single `GET` of GitHub's entire
  **Releases list** (on the order of ~100 KB, growing with release/asset
  count and paginable), not "a few hundred bytes". A fully-offline
  escape hatch is now documented: point `HTTPS_PROXY` at an unreachable
  proxy to make the check fail with no outbound traffic (the tray fails
  silently; `kynoptic update --check` run by hand exits non-zero).
  Synced across `README.md`, `README.zh-CN.md`, and `SECURITY.md`.

- CLI (`db cleanup N --yes`): deleting expired raw events now also
  cascade-deletes the derived aggregate rows for the expired dates
  (`daily_agg`, `agg_minute`, and the `app:*` buckets of `agg_daily`)
  in the same transaction, so the dashboard's minute/daily aggregate
  views no longer keep showing dates whose raw events have been
  deleted. The events high-watermark is advanced in the same
  transaction: the watermark is a row-count figure, so leaving it
  untouched after row deletion made the next database open misread the
  legitimate post-cleanup decrease as corruption.
- CLI (`export`): JSON/JSONL exports now serialize a NULL `window_title`
  as JSON `null` instead of `""`, so programmatic consumers can tell
  "no title" apart from "empty title" (CSV output is unchanged — both
  forms are empty in CSV).

- Dashboard: unified query-parameter semantics across all GET API
  endpoints. Duplicate parameters now consistently take the first
  occurrence (date params previously took the last, numeric the first);
  explicitly empty values (`?date=`, `?days=`) now return 400 for
  numeric params too instead of silently falling back to the default;
  numeric params below their lower bound (including 0) return 400 with
  an error message stating the valid range, replacing the previous
  split wording; values above the cap are still clamped with 200 but
  the response now annotates the clamp with `X_requested` + `note`
  (previously only `/api/input` did this; `/api/anomalies` also sets
  `truncated=true` when its 30-day window is capped); unrecognized or
  wrongly-cased query parameters are echoed in an `ignored_params`
  array (mirroring POST `/api/settings`'s `ignored`), so `?DATE=…` is
  no longer silently dropped. API docs in
  `crates/cli/src/assets/skill.md` updated accordingly.

- Docs: the dashboard module's endpoint list was missing six of the
  seventeen API endpoints (`/api/input`, `/api/insights`, `/api/report`,
  `/api/trends`, `/api/apps_grid`, `/api/daily_top`); it now lists all
  of them and names the `route_req` route table and
  `crates/cli/src/assets/skill.md` as the authoritative endpoint
  lists. `llms.txt` and `SECURITY.md` no longer say the MCP server
  "binds to 127.0.0.1" — it is a stdio process (JSON-RPC over
  stdin/stdout) with no network socket of any kind; only the
  dashboard HTTP server binds to 127.0.0.1.

- Core: all console-tool spawns (`powershell`, `tasklist`,
  `taskkill`, `schtasks`, and self-relaunches) now create their
  process windows through a single `kynoptic_core::spawn::no_window`
  helper instead of scattering `CREATE_NO_WINDOW` /
  `DETACHED_PROCESS` flags at each call site, so no code path can
  flash a console window from the resident tray or the per-minute
  watchdog task. Behavior is unchanged — the flags were already
  correct at every site; centralizing makes the next miss impossible.

- Docs: the documentation set and its release gates were extended: a
  new `check-doc-version` CI gate keeps the version badge and download
  links in sync with the released tag; SECURITY.md's version table
  now names its update-tooling blind spot; APP.md is regenerated
  alongside each release as part of the doc set; and README documents
  `collect --help` for the per-monitor flag list.

### Removed

- The last two topmost (`MB_TOPMOST`) modal message boxes are gone,
  per the "monitors must not pop dialogs over the user" rule. The CLI
  `watchdog` alert path (`watchdog_alert` and the tray-exit-flag
  notice) no longer opens a foreground-stealing message box, and the
  tray's watchdog-task health alert no longer opens one on its first
  self-check failure of each tray process. Alerts are surfaced through
  the tray notification / heartbeat channels instead.

## [0.3.1] - 2026-09-25

### Added

- Dashboard: `/api/status` now returns `token_required` (opt-in access
  token enabled), `actual_port` (the port `serve` actually bound, so a
  busy-port auto-fallback is visible), and `watchdog_alert` (the most
  recent `event_action='notification'` watchdog event within 24h —
  previously these events had no UI consumer at all). The dashboard
  renders the watchdog alert as a top banner, and the footer auth note
  now follows the real token state instead of always claiming "no auth
  needed".
- Dashboard: new `GET /api/autostart-status` returns the saved
  `autostart` value together with the current registry Run-key state, so
  the settings page can verify the tray-side registry sync after saving
  and downgrade its confirmation when the sync failed.
- Dashboard: `/api/anomalies` responses now include `baseline_days`
  (days with data in the last 7 local days), letting the anomalies page
  tell "baseline still accruing" apart from a genuine "no anomalies".
- Dashboard: `/api/status` now returns `paused` (the tray-written
  `paused.flag` marker), and the status line shows "paused by user"
  instead of "stale"/"collector may be down" while collection is paused
  from the tray menu.
- Tray: on first launch the tray now opens the dashboard in the default
  browser automatically (tracked by a `ran-first-open.txt` marker in the
  data folder), so a fresh install reaches its UI without hunting for
  the port.
- Tray: the tray menu gains a dynamic "update failed to start" entry
  (backed by `update-error.txt` in the data folder) when a one-click
  update could not launch, with the recorded error details.

### Fixed

- Dashboard: an HTTP 401 from the API is no longer narrated as
  "disconnected — retrying…" — the status line now shows "access token
  required" with a re-entry button, the unauthorized error gets a stable
  `code` plus Chinese mapping, and parameter-validation echoes no longer
  use Rust Debug formatting (`{:?}`).
- Dashboard: the daily heatmap zero level now uses the idle color
  (contrast vs level 1 was 1.14:1), the keyboard heatmap gains the same
  empty state as the mouse chart, language switching immediately redraws
  the status line on every tab, the settings save confirmation fades
  after 9s with a brief "✓ saved" on the button, the report page uses
  one consistent raw-input annotation on all dates, and the
  unattended/automation card notes are rewritten in plain language with
  a shared glossary (away = 无输入, mouse buttons = 个按键).

- Tray: the watchdog scheduled task is now self-checked on tray startup —
  if it is missing or disabled, the tray raises an alert (log + a
  database notification event the dashboard banner shows + a persistent
  tray tooltip) instead of failing silently. The tray does not recreate
  the task: to restore it, re-run the installer, which re-registers the
  watchdog task.
- MCP: `get_summary` responses now include a `baseline_semantic` field
  identifying the baseline window's semantics, so AI clients can tell
  what the baseline numbers actually mean (API contract addition).
- Dashboard: `/api/anomalies` now rejects `days=0` with 400, matching
  the MCP `get_anomalies` contract (`days` must be >= 1) — previously it
  was silently clamped to a 1-day window.
- CLI (probe): `kynoptic probe` accepts a new
  `--first-collect-timeout <SECS>` option to override the hard cap on
  waiting for a monitor's first collection (0, the default, derives it
  from `--secs`; any value is still capped at 420 s).

- Core: the minute aggregate cache (`agg_minute`) now keeps injected
  (automation) input in dedicated `input_keys_injected` /
  `input_clicks_injected` buckets (mirrored by the rebuild/backfill paths so
  incremental maintenance and a full rebuild agree), and the late-night
  activity and APM-spike anomaly checks now count human-side input only —
  automation injection no longer registers as human activity.

### Changed

- Core (presence): `bridge_count` no longer bridges across long gaps — when
  the distance between two human-input minutes exceeds the bridge window + 1
  minute, the hole is no longer counted as presence. This changes the
  `presence_minutes` numbers shared by overview, insights, and the CLI
  presence output whenever activity contains holes longer than the bridge
  window (reviewed invariant: do not rewrite this to a bare
  `min(gap, diff-1)`).
- CLI (collect): an unknown monitor id on the command line is now reported
  as a warning and the run continues with the known monitors; it used to
  hard-fail. Note the asymmetry with `POST /api/settings`, which still
  rejects unknown ids.
- Tray (watchdog): with `--once`, the stall re-check wait is now 30 s
  (was 40 s), and it can be overridden with the new
  `KYNOPTIC_WATCHDOG_RECHECK_SECS` environment variable (seconds).
- CLI (probe): the default first-collection timeout is no longer a fixed
  420 s — when not set explicitly it is now `min(--secs * 10, 420)`, so
  `--secs 15` skips a slow monitor after ~150 s and still produces a
  result instead of idling up to 7 minutes. Pass
  `--first-collect-timeout` to keep the old ceiling for a given run.
- Core (privacy): window-title redaction (opt-in) no longer matches only
  `http://`/`https://` URLs — it truncates at any RFC 3986-shaped
  `scheme://` prefix (e.g. `ws://`, `wss://`, `vscode://`, `file://`),
  so titles containing those schemes are now redacted too when the
  switch is on. The switch default remains off.
- CLI (probe): the probe subcommand no longer pre-installs the global
  env_logger, so registering a CaptureLogger (e.g. the polling monitor
  watchdog) can no longer fail and get misreported as a FAIL after the
  420 s wait.

- Core (privacy): the location monitor no longer performs any outbound
  network calls — its former geolocation path was removed and the
  monitor is local-only now; this tightens the "data never leaves the
  machine" guarantee for a default-on surface.
- CLI (read commands): read-only subcommands (`stats`, `query`,
  `report`, `export`, `analyze`, …) no longer create a database file
  when none exists, reject future dates instead of silently returning
  empty results, and cap WAL growth on long read sessions.
- Tray: a second instance's start attempt is no longer blocked on a
  modal message box — the rejection notice now pops on an independent
  thread, closes itself after 3.5 s, and the rejected instance exits
  with status 1 about 4 s later.
- CLI (migrate): `kynoptic migrate` now runs
  `scripts/migrate_legacy_db.py` itself and writes the target database,
  instead of only printing the python command for the user to run. This
  adds a Python interpreter dependency for that subcommand; calling it
  without `--legacy` surfaces the script's own error and the CLI only
  echoes its exit code.
- Tray: tray menu labels are now consistently Chinese-first in both
  languages (the bilingual "中文 / English" ordering is unified).

### Fixed

- Dashboard (timeline): the hourly timeline's human-presence minutes now merge
  `raw_human_minutes_window` (raw press/click/scroll rows with `injected=0`)
  the same way the overview's `classify_minutes` does — previously raw-mode
  minutes only reached the overview, so under the same bridge threshold the
  two views systematically disagreed (e.g. 65 vs 81 minutes) and raw-mode
  users' timeline dropped every raw minute. Requires `kynoptic-core`
  `raw_human_minutes_window` to be `pub` (one-line visibility change).
- Dashboard (overview): `unattended_fg_minutes` could go negative
  (foreground minutes < presence + automation) despite the API note
  promising "negative values are clamped to 0" — the clamp is now applied.
- Dashboard (page): the Apps ranking no longer overflows horizontally at a
  320 px viewport (WCAG 1.4.10 Reflow) — the fixed 150 px app label now
  shrinks (min 80 px, ellipsis + `title` kept) and the event count can
  compress.
- Dashboard (light theme): canvas-drawn pixels are theme-aware again —
  keyboard heatmap keys/strokes/keycaps and the hourly bar charts (grid,
  axis labels, bars) read new `--chart-*`/`--kb-*` CSS variables (with
  light and forced-colors overrides) instead of hardcoded dark colors;
  axis labels now hit 4.5:1 contrast on the light background (was 2.43:1).
- Dashboard (settings): a corrupted `settings.json` is now rebuilt on disk
  with the fallback defaults right after being archived as
  `settings.json.corrupt.bak`, honoring the "rename and rebuild" contract
  — previously the file stayed absent until the next save, and
  `GET /api/settings` silently served defaults with no indication.
- Docs (README/APP): new "Writing settings from scripts" section documents
  the four headers a script must send to `POST /api/settings` and where the
  session token comes from — the 0.2.x-style plain POST is blocked by
  design (loopback CSRF defenses).


- Docs: the status lines in `README.md`, `README.zh-CN.md`, and `APP.md` now
  say v0.3.0 (they still said v0.2 / v0.2.x while `Cargo.toml`, the website,
  and the GitHub release already said 0.3.0). The same sync missed
  `llms.txt` — its status line still said v0.3.0 when 0.3.1 shipped — and it
  has since been corrected to v0.3.1; its "Full command reference: APP.md"
  note also carries a version-snapshot caveat, since APP.md documents the
  CLI surface as of a specific release (`kynoptic --help` stays
  authoritative).
- Dashboard (security, opt-in token): the optional access token is no longer
  accepted via the `?token=` URL query string (a query string ends up in
  browser history and bookmarks); pass it via the `X-Kynoptic-Access-Token`
  header or `Authorization: Bearer` instead. The dashboard page likewise no
  longer picks a token up from the URL — it keeps the sessionStorage flow and
  the on-page prompt. When no token is enabled, behavior is unchanged.

- Dashboard (high-contrast/forced-colors): the 24h timeline three-state
  bands, their legend markers, and both five-swatch heatmap legends are now
  drawn via CSS classes/`--hm-*` variables with distinct system-color
  mappings (plus a stripe pattern on the away band), instead of inline
  backgrounds that Windows high contrast collapses into one indistinguishable
  color; the forced-colors heatmap levels are five distinct system colors
  (previously 5 levels degraded to 3) and the legend swatches follow the same
  variables as the chart (the fifth swatch no longer shows a stale blue).
- Dashboard (accessibility): the daily heatmap and today's hourly input SVGs
  now have accessible names and the heatmap exposes its per-day values as a
  collapsible text summary (values previously lived only in hover tooltips,
  unreachable by keyboard, touch, and screen readers); the keyboard/mouse
  device SVGs got accessible names too.
- Dashboard (accessibility): the tab bar now implements the ARIA APG tabs
  keyboard contract (roving tabindex, Arrow/Home/End to move and activate,
  `aria-controls`/`aria-labelledby` linkage); heatmap range buttons use
  radiogroup semantics with `aria-checked` and arrow-key support; the
  today/yesterday/2-days-ago buttons expose `aria-pressed`; the status line,
  update banner, and anomaly badge are live regions that no longer re-announce
  on every 30s poll when the text is unchanged.
- Dashboard (error messages): API errors now carry a stable `code` field;
  English UIs map validation codes to English text instead of showing raw
  Chinese; 404/405 return bilingual text with a refresh hint; serde JSON
  parse errors report only line/column (details go to the log); SQLite errors
  are downgraded to a friendly bilingual message with the original logged;
  the 403 write-block and session-token responses are bilingual and
  distinguishable, and the settings save path offers a token re-entry prompt
  on 401.
- Dashboard (`/api/input`): requesting more than the 90-day cap is no longer
  silently truncated — the response includes `days_requested` and a note.
- Dashboard (reflow): settings/cards/host grids use `minmax(min(Npx,100%),1fr)`
  and monitor rows wrap, so a 320px viewport no longer gets a horizontal
  scrollbar (WCAG 1.4.10).
- Dashboard (settings): a failed hardware-info load now shows an error with a
  retry button instead of silently leaving placeholder dashes.

- Dashboard (insights): the human-side row filter now counts scroll-only
  input_agg minutes (keys/clicks/scroll_ticks each minus their injected
  counterpart, aligned with the presence definition), so scroll-only
  reading time shows up in longest-presence / late-night / golden-hours
  cards; the empty state now reports the real gate (`gate.events` /
  `gate.required`) instead of promising "after a day of collection".
- Dashboard (report): the goal card for a historical date now looks up the
  selected day in the trends `daily` array instead of always showing the
  last row (today), and the raw input-minute fallback is explicitly
  labelled instead of silently posing as presence minutes.
- Dashboard (`/api/hours`): the hourly values are now input counts
  (keystrokes + clicks; raw press/click fallback), not total event counts
  that were dominated by heartbeat/sampling noise; the response and the AI
  skill doc no longer describe it as "golden hours".
- Dashboard (wording): timeline legend "无人" renamed to "无输入" (away) to
  match what the band actually computes; heatmap top color no longer
  shares the presence-blue value and the daily heatmap is titled "incl.
  automation-injected input"; the trends note is phrased in plain language;
  the presence-grace hint mentions scroll; nav label is "今日概览" and the
  presence card says first/last "在场" (presence).
- Dashboard (overview): the "vs yesterday" delta is suppressed in the first
  3 hours after local midnight (yesterday is a full day; early-morning
  comparisons were systematically misleading).
- Dashboard (API): `/api/summary` accepts `today` like report/hours/
  apps_grid; `/api/input` accepts `date=` for the hourly series (previously
  silently ignored; an empty or invalid `date` now returns 400 instead of
  falling back to today) and returns `hourly_date`.
- Core (anomalies): marathon-session detection now reads the same
  human-presence minute source as the overview "presence" number
  (`human_minutes_by_date`: excludes automation-injected input, includes
  scroll ticks) instead of raw keyboard/mouse input minutes; sessions with
  only injected or scroll-only activity are no longer counted as marathons.
- Dashboard (report) / MCP: foreground dwell segments are now capped at
  2 hours per switch (gaps longer than the stall cap — shutdown, sleep,
  leaving the desk — are no longer attributed to the app). This applies to
  the report page dwell breakdown and MCP `get_top_apps`; values for a
  day with one long uninterrupted session are lower than before and now
  agree with overview/CLI presence.
- Core (sessions): `idle_seconds` is now backfilled from the
  `idle_start`/`idle_end` event pairs inside a session on both close paths
  (startup sweep and ghost-session cleanup); it was always written as 0
  before.
- Core (daily_agg): recompute no longer writes an all-zero row for days
  without any input and clears legacy all-zero rows (the trend chart used
  to show phantom zero points for those days). To keep consumers aligned,
  `/api/trends` now zero-fills missing calendar days so `daily` always
  covers the full 28-day window.
- CLI (watchdog): stall alerts are now recorded in the events table as
  `system`/`notification` rows and surfaced as a system notification, so
  watchdog alerts appear in the timeline/event views like other
  notifications.
- Dashboard (API): the `/api/hours` note and the AI skill doc now state
  that the hourly input counts include automation-injected input (the
  values themselves are unchanged; only the labeling, matching the daily
  heatmap).
- Core (privacy, salted digests): the clipboard/notification summary salt
  was persisted to the database metadata (`clipboard_salt`) but never used —
  `salt()` and `set_salt()` each declared their own `OnceLock`, so digests
  were computed with a fresh per-process random salt. The same clipboard
  text produced a different digest after every restart, breaking
  digest-based matching across sessions, and the stored salt was dead data.
  The two locks are now one module-level `OnceLock`: the salt is loaded
  from (or first created in) the database metadata at collection start and
  is the one actually used, so a given plaintext yields a stable digest
  within a library and a different digest across libraries, as documented.
- Core (privacy, title redaction): the collection-side title-redaction
  switch (`CollectorSettings::redact_titles`, `monitors::title_privacy`)
  is documented as reserved but **not yet wired** — no dashboard setting,
  tray, or CLI path can turn it on, so titles are still stored verbatim.
  The previously misleading "already wired" code comments now say so
  explicitly; behavior is unchanged (default off).
- Core (storage): a corrupted database is now surfaced to the caller with
  a clear error plus a degraded flag instead of being swallowed, so the
  dashboard/tray can show an explicit degraded state instead of
  pretending data is fine.
- Installer: the failure path is now idempotent — a failed install cleans
  up its own partial changes so re-running the installer does not stack
  duplicate state, and the installer writes the install directory to the
  user `PATH` so the CLI is reachable from a fresh terminal.
- Self-update: the updater now avoids file-in-use conflicts (backs off /
  renames targets that are locked) and swaps only the files that actually
  differ in a release, instead of touching every asset.
- Docs: the AI skill doc (`SKILL.md`) now states the data-retention cap
  in the same terms the code enforces, so AI clients no longer promise a
  different retention window than the product keeps.
- Dashboard (API): `/api/insights` no longer returns the internal
  `dbg` debug field (raw intermediate counts); the endpoint contract did
  not declare it and no frontend consumed it.
- Dashboard (security): the per-session CSRF token is now compared with
  the same constant-time comparison as the access token (previously a
  plain string compare), closing the timing-contrast inconsistency.
- Dashboard (security): the CSP nonce now mixes the high-resolution clock
  into both halves of its 128-bit value (previously only one), removing
  the theoretical correlation between the two `RandomState` outputs.
- Docs: `llms.txt` corrected — status line now says v0.3.0, the privacy
  section acknowledges the tray's once-a-day version check (the `update`
  subcommand remains user-invoked only), and the CLI subcommand list now
  matches the actual 18-command dispatch table; `CONTRIBUTING.md`'s
  release checklist step 6 now counts 10 release assets (matching the
  `release.yml` upload list); `README.md`'s quick-start `collect` comment
  now says foreground; `APP.md`'s CLI section version labels unified.

## [0.3.0] - 2026-09-23

### Fixed

- Collector: the writer connection now applies the same pragmas on open
  (WAL + busy_timeout) as the read paths. The background aggregation
  backfill thread holds its own write connection (`BEGIN IMMEDIATE`), so a
  freshly restarted writer with no busy timeout could immediately hit
  `SQLITE_BUSY` after a hot reload and silently drop the batch it was
  about to write; that race window is closed.
- Dashboard (access control): all responses now also send
  `Referrer-Policy: no-referrer`, so a token accidentally embedded in a
  bookmarked `/api/*?token=…` URL is not leaked to other origins if the
  JSON error page is ever loaded in a browser tab; the 401 body now hints
  at using the dashboard page (which strips the token from the URL) instead
  of linking directly to `/api/*`.
- Dashboard: invalid calendar dates such as `2026-13-45` now report
  "date format error" instead of the misleading "date is in the future"
  (the old string comparison ran before any real date parsing).
- Dashboard: an explicitly empty `?date=` on read endpoints now returns
  the same 400 "date format error" as other invalid values, instead of
  silently falling back to today (only a fully omitted parameter falls
  back).
- Dashboard: the "unknown monitor id" error no longer echoes the raw
  attacker-controlled string (response amplification / log-injection
  surface); it is sanitized the same way as date inputs.
- Dashboard: settings.json writes clean up their temp file when the final
  rename fails (file locked by antivirus/backup, ACL denial, disk full) —
  previously each failed save leaked one `json.tmp.<pid>.<nanos>` file.
  The tray now logs a warning when the startup autostart sync-back fails
  instead of discarding the error.
- Dashboard: hourly buckets for `/api/input`, `/api/apps_grid` and
  `/api/daily_top` are localized with SQLite `localtime` (timezone
  resolved per event timestamp) instead of a fixed offset captured at
  query time, matching every other view; historical hour distributions no
  longer shift by one hour in timezones with past DST transitions.
- Dashboard (hardening): the inline dashboard script is now served with a
  per-request CSP nonce and all inline `onclick`/`onchange` attributes
  were replaced by delegated listeners; `script-src` no longer allows
  `unsafe-inline`, restoring a second line of defense behind `esc()` for
  the 30+ `innerHTML` render paths.

- Dashboard (web): `loadApps` now guards the second `await`
  (`/api/daily_top`) with the same sequence check as the first, so a
  stale response can no longer overwrite the "daily Top 3" table with
  data from a previously selected date.
- Dashboard (web): non-numeric input in the settings numeric fields
  (daily goal / bridge minutes / port) no longer silently saves `0`
  via `parseInt(...) || 0`; the previous saved value (or the default)
  is kept and a note is shown. A changed port now also surfaces the
  "takes effect after tray restart" hint (previously dead i18n copy).
- Dashboard (web): with per-keystroke (raw) granularity the input page
  no longer renders the misleading "N unattributed clicks from an old
  version" banner — raw mode never produces per-button rows, so all
  clicks were falsely reported as unattributed.
- Dashboard (web): a last-event timestamp in the future (dirty data /
  clock skew) no longer reports "collecting" forever; it is shown as a
  timestamp anomaly instead.
- Dashboard (web): the 24h timeline no longer draws a full 60-minute
  "away" span for the in-progress current hour; away is capped at the
  minutes elapsed since the hour start (computed from the response's
  `generated_at`, not the client clock).
- Dashboard (web): `jget` now surfaces the backend `{"error": ...}`
  reason on non-200 responses (e.g. future-date validation) instead of
  only "URL → HTTP 400".
- Dashboard (web): the Top Apps list uses the same display names as the
  timeline / app grid / daily Top 3 (`.exe` suffix stripped; full name
  kept in the tooltip).

- Dashboard: `/api/overview` no longer waits on the `nvidia-smi`
  subprocess — GPU utilization now refreshes on a background thread
  (stale-while-revalidate, 30s TTL, single-flight), so request threads
  return the last known value immediately instead of blocking up to 3s+.
- Dashboard: read endpoints (`/api/trends`, `/api/apps`, `/api/daily_top`,
  `/api/heatmap`) map database-level failures to `400` with the root cause
  instead of degrading to `200` with silently empty payloads (which was
  indistinguishable from a legitimate empty result).
- Dashboard: the report page's `data_since` now uses the local calendar
  date (`datetime(timestamp,'localtime')`), consistent with the
  "a day = local day" convention everywhere else; in UTC+8 the calendar no
  longer opens one extra blank day.
- Dashboard settings: hand-edited `settings.json` with more than 100
  categories or a pattern longer than 200 characters is clamped on load
  (with a warning) instead of amplifying match cost; category rules
  precompile their lowercase tokens once at load so per-segment matching no
  longer re-lowercases the pattern for every rule.
  **Migration note:** category-rule matching is now genuinely
  case-insensitive. Previously a pattern token containing uppercase
  letters (e.g. "GitHub", "VSCode") never matched — the haystack was
  lowercased but the token was not — so such rules were silently dead.
  After upgrading, existing rules with uppercase tokens may start
  matching, and report/category results can differ from previous runs.
  Lowercase your patterns if you need the old (dead-token) behavior.
- Dashboard page: light-color-scheme palette and `forced-colors` support;
  the heatmap SVG colors are read from CSS variables at render time instead
  of hardcoded dark hex values, so canvas/SVG pixels follow the system
  theme and keep contrast in Windows high-contrast mode.
- Dashboard: report page "today" goal card now recomputes the local date
  per render, so a page left open across midnight no longer applies the
  server's today presence to yesterday's report.
- Dashboard: `/api/timeline` DST caveat (`local_offset_note`) is now
  rendered as a small note when the hour-window crosses a local offset
  change (or the server offset differs from the browser's).
- Dashboard: keyboard heatmap uses five distinct color steps (the bottom
  two quantiles previously shared one color); zero-activity days no longer
  draw a 2px phantom bar in the trend chart; `fmtBytes` supports TB;
  per-app colors are derived from a stable name hash so the apps grid and
  daily-top charts agree; monitors KPI falls back to `0 / 0` on missing
  fields; unparsable `last_event_ts` no longer claims "collector down".
- Security: POST /api/settings additionally requires a per-session random
  token injected into the served page (the previous three CSRF checks were
  all client-controlled headers); settings-audit.log refuses to follow
  symlink/reparse-point targets on open.

### Changed

- Dashboard: the HTTP service keeps a small pool (3) of read-only
  connections instead of one shared mutex-guarded connection, so a slow
  endpoint no longer serializes every other card request behind it.
- Dashboard: `/api/input` aggregates keys/clicks/button/scroll/move
  scalars and per-key frequency in SQL (`SUM`/`json_each`) instead of
  pulling every `input_agg` row into memory and serde-parsing each one;
  results are unchanged, cost no longer grows with the window's row count.
- Dashboard: `/api/timeline` results are cached for 60s in the long-running
  server (keyed by hours/bridge/db path) to avoid re-running the
  expensive multi-day aggregation on every panel poll; pure
  `api_timeline_at` used by tests is unaffected.
- Single-instance: while upgrading from the session-scoped
  `Local\KynopticTrayMutex` to the per-user `Global\KynopticTrayMutex-<SID>`
  name (single-level name — kernel named objects do not support path-like
  nesting; the nested `Global\A\B` form fails with ERROR_PATH_NOT_FOUND,
  the root cause of the 2026-09-23 deployment incident), new builds also
  acquire and probe the legacy `Local\` name (bridge mutex), so an old
  tray/collect still running during the upgrade window is detected by new
  binaries and vice versa — the no-double-writer guarantee no longer lapses
  across version mixes.
- Dashboard: new `GET /api/diagnostics` enumerates the known log/archive
  files (collector-error/dashboard-error/watchdog/tray/update/
  dashboard-port) across the data and exe directories with existence,
  size, mtime and a sanitized 20-line tail (no paths exposed); the
  settings page consumes it in a collapsible "diagnostics" block.
- Docs: README/README.zh-CN clarify the 8422→8432→18422/28422 port
  fallback applies to the tray entry only (the CLI dashboard exits on a
  taken port); APP.md updated to the v0.2.x state (six MCP tools incl.
  `get_top_apps`, `--metric`/`--join` still not implemented).

## [0.2.2] - 2026-09-19

### Fixed

- Timeline: bridged presence minutes were shifted by the timezone offset
  (most hourly buckets showed 0 presence despite activity), and the
  current in-progress hour was missing from the response.
- Dashboard UX: terminology unified (presence/automation/unattended/
  foreground time) in plain language; mixed hours no longer rendered as
  unattended; stuck "loading..." placeholders replaced with proper empty
  states; KPI values no longer truncated; date buttons follow the
  selected day; design tokens unified (one orange/green/gray per
  meaning, presence-blue exclusive); anomalies tab alert badge;
  accessibility labels on views and charts.

## [0.2.1] - 2026-09-19

### Fixed

Eleven review waves (16-26) of parallel deep audits: about 120 fixes with
zero data-loss findings left open. Highlights:

- Session identity: events.session_id is now stamped (was 100% NULL).
- Watchdog safety: heartbeat pid extraction hardened and the pid's image
  name verified before any kill; the image-name global-kill fallback was
  removed (it could kill unrelated kynoptic-tray processes); system sleep
  no longer looks like a stall.
- Timeline API: /api/timeline hours clamped to 744 (31 days); year-scale
  views belong to the heatmap (daily_agg, milliseconds).
- Retention cleanup prunes pre-aggregation tables in the same
  transaction as raw events so heatmap/trends can never show deleted
  months.
- MCP: read-only connections no longer run WAL pragma (all tools failed
  on non-WAL databases); SQL errors are no longer reported as empty
  results.
- Release: fixed the double v-prefix download link that 404'd on every
  release; workspace-level version; aggrepair/ctl now ship in the
  installer.
- Settings: POST /api/settings rejects non-object bodies; UTF-8 BOM in
  settings.json no longer silently resets all settings.

## [0.2.0] - 2026-09-18

### Added

- Corrupt-database visibility: collector startup failures now persist a
  diagnosis (including `PRAGMA quick_check` result) to
  `data\collector-error.log` and auto-retry every 60 s instead of
  failing silently behind a healthy-looking tray icon.
- Watchdog collector-liveness detection: the heartbeat carries the last
  successful DB flush timestamp and a `stalled` flag — a hung collector
  behind a live tray is now killed and restarted like a dead one.
- Watchdog hardening: state-file mutation lock, sleep-window guard
  against wrongful failure counting across suspend/resume, spawn-failure
  backoff (missing exe / blocked taskkill are no longer silent infinite
  retry loops), and log-rotation fallback.
- Self-update ships and refreshes `SKILL.md` (verified against
  SHA256SUMS), relaunches a tray it killed to unlock files, filters
  pre-release tags, and never kills its own process image.
- Input statistics in event time: minute buckets land in the minute the
  keystrokes happened (robust across aggregator stalls and DST repeated
  hours), with a `final` flag distinguishing complete minutes from
  per-second snapshots so restarts merge instead of discarding.
- First-run experience: collecting hints on all overview cards,
  first-day provisional note, heatmap "no data yet" state.
- Settings page: daily-goal minutes and dashboard-port controls.

### Fixed

- Silent autostart loss: the installer task is default-checked and
  upgrades no longer remember the old unchecked state; the watchdog
  scheduled task is always (re)created.
- Installer packaged stale binaries from `dist\`; packaging now follows
  the freshly built exes (build-order hygiene).
- Unattended metric double-subtracted human/automation mixed minutes.
- Presence/active-minute metrics: raw mode now reports first/last
  activity; all-time active minutes include minute-mode data; DST
  fall-back no longer collapses the repeated hour.
- MCP: batch requests answered, client responses absorbed silently,
  UTF-8 vs terminal IO errors distinguished, `wait_for` concurrency
  capped with prompt EOF shutdown, non-RFC3339 time bounds rejected
  readably, `days` wrong-type hard error, negative idle clamped.
- Insufficient-capacity file watcher channels are bounded with visible
  drop counters; clipboard polling no longer fabricates change events.
- Dashboard: transient disconnection indicator now clears, report/apps
  stale-response races guarded, Input tab tolerant of missing fields,
  settings edits survive re-entering the tab.

## [0.1.0] - 2026-09

### Added

- Initial workspace: `kynoptic-core` (14 default monitors, numbered SQL
  migrations), `kynoptic-cli`, `kynoptic-mcp`, later `kynoptic-tray`.
- Performance work: sargable day-range queries, covering indexes,
  aggregate read caches, 30 s flush interval, per-minute input
  aggregation (opt-in).

[Unreleased]: https://github.com/ardss/kynoptic/compare/v0.8.1...HEAD
[0.8.1]: https://github.com/ardss/kynoptic/compare/v0.8.0...v0.8.1
[0.8.0]: https://github.com/ardss/kynoptic/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/ardss/kynoptic/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/ardss/kynoptic/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/ardss/kynoptic/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/ardss/kynoptic/compare/v0.3.2...v0.4.0
[0.3.2]: https://github.com/ardss/kynoptic/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/ardss/kynoptic/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/ardss/kynoptic/compare/v0.2.2...v0.3.0
[0.1.0]: https://github.com/ardss/kynoptic/releases/tag/v0.1.0
