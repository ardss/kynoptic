# kynoptic 标准化 E2E 测试

## 运行
```bash
bash scripts/e2e.sh                # 默认测 http://127.0.0.1:8422
bash scripts/e2e.sh http://127.0.0.1:9999   # 指定面板地址
```

前置：agent-browser CLI；本机托盘/仪表盘正在运行。C 段铁律测试不再要求真实库：
优先复制真实库（路径可用环境变量 `KYNOPTIC_E2E_DB` 覆盖，默认
`D:/Kynoptic/data/kynoptic.db`）；真实库不存在时自动用 CLI 在临时目录初始化
生产 schema 种子库；连 CLI 都没有时退化为 python sqlite3 最小表种子 +
SQL 层守门断言——**C 段不再 SKIP**。

## 覆盖范围
- A 后端接口：首页 200 / overview 三指标 / summary 计数 / insights 结构
- B 真浏览器：7 个页签逐一打开断言（人在场渲染、自动化/前台/首末在场、时间线 >=5 条、
  中英切换、报告目标/热力图/趋势、Top 榜、鼠标左键悬停值与 API 一致、洞察卡或空状态、
  设置硬件面板+桥接阈值）
- C 铁律（临时库）：`cleanup 0` 后 **events / agg_minute / agg_daily / sessions
  四表行数全部不变**（不只数 events）；`--yes` 且 <30 天被拒绝；
  非法天数报错而非静默。CLI 二进制按候选列表自动发现（安装目录 /
  `target/release` / `target/debug` / PATH）

## 已知盲区（诚实清单）
1. 不捕获浏览器 console 错误（agent-browser 无此 API）——JS 崩溃靠"数字未渲染"间接暴露
2. 悬停一致性检查读取的是 title 元素内容，未派发真实 mouseover 事件
3. 无首装空库、监控器全关场景（午夜跨界的桶归属已由
   `crates/core/tests/midnight_bucket_test.rs` 单测覆盖）
4. 无多日数据积累类测试（热力图多周形态）
5. C 段无 CLI 时的降级路径只验证四表 schema 完整性，CLI 级
   `cleanup 0 / --yes / abc` 行为断言在该路径下不执行（输出 NOTE 而非 FAIL）
