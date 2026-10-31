# ENGINEERING — 工程规范

> Kynoptic 的流程/测试/发版/复盘/汇报规范。每条标注【依据】（外部权威或本仓事故）
> 与【闸】（机械强制点，落在哪里）。2026-10-30/31 两轮事故（空 squash 缺料发版、
> Inno 挂死、错树替换）为本规范的直接动因；外部调研：Google SRE Book
> （Postmortem Culture / Release Engineering）、Martin Fowler《TestDrivenDevelopment》、
> GitHub merge queue 文档。

## 1. 变更与合并

- **R1.1 分批提交**：一个逻辑修复一批；commit message 必须含根因、证据
  （文件:行/命令/输出）与 TDD 红绿轨迹。【闸】PR 描述按批次列表核对。
- **R1.2 PR 内容核实**：凡出现过推送被拒、通道重试、远端分支状态存疑，
  PR 必须以 `gh pr diff --name-only` 的文件清单与预期批次文件集核对，
  **CI 绿不作数**——绿只证明「跑过的树」，不证明「该在的改动都在」。
  【事故】#61：推送被拒后 `gh pr create` 基于旧分支建了空 PR。
- **R1.3 合并后抽验**：每个 squash 合并立即 `git show --stat HEAD` 确认非空，
  并对本批关键符号 `git grep` 抽验（如新设置字段、新测试名）。
  【闸】release.yml「Reject empty tag commit」。
- **R1.4 合并竞争**：PR 并发多时启用 GitHub merge queue（在合并后的最终内容上
  跑检查）。【依据】docs.github.com merge-queue。当前单人维护暂缓，PR 模板
  预留「预期文件集」声明。

## 2. 测试（严格 TDD）

- **R2.1 先红后绿**：生产代码改动必须先有**实测失败**的测试。live（真实树）
  或 historical（实现已在树时，临时副本桩态重现，附命令与失败输出原文）二选一；
  红证据必须真实，不编造、不回退实现让测试变红。
- **R2.2 最小实现到绿，重构分离**：只写让测试过的最少代码；结构改进放到
  重构步骤单独做。【依据】martinfowler.com《TestDrivenDevelopment》：最常见的
  TDD 失败是跳过第三步。
- **R2.3 测试纪律**：不读真实时钟（日期一律注入）；既有断言期望的修改必须
  给出理由并在 diff 内留注释；每个测试的失败原因要看得懂。
- **R2.4 门禁**：每批过 fmt / clippy -D warnings / test --workspace / check-js；
  发布前以全量门禁为准，低层门禁过了不豁免高层。

## 3. 发版

- **R3.1 定版链（固定顺序）**：bump 全源 → CHANGELOG 三步定版 →
  check-doc-version 全 ok → PR/CI → 合并 → 抽验合并提交 → tag 指向该提交 →
  release 构建 → 资产哈希对 SHA256SUMS → 安装 → **装后字段级验证**（如
  GET /api/settings 出现本版新字段；本步直接抓住了 v0.6.0 缺料事故）→ 记档。
- **R3.2 tag 纪律**：只打在「内容已抽验」的合并提交上；已公开的 Release 发现
  缺料，立即删除（release+tag）重建，不补丁式解释。【事故】v0.6.0 首发缺批2-5。
- **R3.3 发布物一致性**：本机归档的 Setup/散 exe 必须对官方 SHA256SUMS 逐字节
  核验；失效副本删除重下，不混放新旧。【闸】release.yml：工作区清洁 +
  tag↔Cargo.toml 版本一致 + 文档一致性终检（均为既有/本次新增步骤）。
- **R3.4 回滚预案**：升级前旧 exe 备份至 `bak-<旧版>/`；装后验证不过立即回替，
  数据目录不动。【依据】sre.google Release Engineering：发布必须可回退。
- **R3.5 安装 runbook 优先**：安装器异常（挂死/静默降级）时按双机 runbook 手工
  替换（哈希校验→停进程→备份→替换→注册表版本→任务恢复→字段级验证），
  设 10 分钟无进展上限，不陪安装器干等。【事故】Inno 关闭应用阶段两次挂死。

## 4. 多机运维

- **R4.1 先读档案再动手**：操作任何机器前先查其安装档案（安装根、看门狗任务
  名、Run 键名、数据目录、任务属主与电池形态）；禁止凭惯例假设路径。
  【事故】A 机真身在 `D:\Kynoptic`，误替换 LOCALAPPDATA 旧树。
- **R4.2 档案字段**：安装根 / 看门狗任务名 / Run 键名 / 数据目录 / 任务属主 /
  电池形态 / 端口。发现漂移当场更新档案。
- **R4.3 采集红线**：只绑 127.0.0.1；设置变更后回读验证；监控器尽量全开
  （HeavySemaphore 预算内，v0.5.0 实测 40-on 可控）。

## 5. 复盘（SRE 无责复盘）

- **R5.1 触发标准（预先定义）**：用户可见故障、数据风险、发版缺料、流程被
  证伪、需人工干预回滚——发生即复盘。【依据】sre.google Postmortem Culture：
  触发标准要在事故前定义。
- **R5.2 文档模板**：影响 / 时间线 / 全部促成根因（不止表层）/ 处置 /
  What went well–poorly–lucky / 行动项（owner + 优先级 + 类型
  avoidance/mitigation/process）。【闸】MAINTENANCE_LOG 记档，行动项进下轮批次。
- **R5.3 无责**：默认每个参与者基于当时信息做了合理的事；修系统、修流程、
  修门禁，不修人。范例：v0.6.0 补合并事故记档（2026-10-31 节）。

## 6. 汇报纪律

- **R6.1** 「已验证」必须附亲验凭证（哈希、grep 输出、run id、接口字段值）；
  拿不出凭证就写「待验证」。
- **R6.2** 汇报固定含「我亲验了什么」；子代理回报、CI 绿、上游结果一律不算
  完成依据。
- **R6.3** 安装/发版/合并等短链路前台同步推进并主动阻塞轮询，不沉默等通知。

## 7. 机械闸清单

| 闸 | 位置 | 拦什么 |
| --- | --- | --- |
| fmt / clippy -D warnings / test --workspace / check-js | ci.yml | 代码质量与回归 |
| check-doc-version.mjs | ci.yml + release.yml | 版本/文档漂移（本次新增） |
| 工作区清洁 | release.yml | 临时文件混入发布 |
| tag↔Cargo.toml 版本 | release.yml | 错 tag 构建 |
| 空提交拒绝（diff-tree 计数>0） | release.yml | 空 squash 缺料发版（本次新增） |
| 装后字段级验证 | 发版 runbook | 内容缺失的最后防线（人工，固定步骤） |
| merge queue / PR 模板 | 待办 | 合并竞争与文件集声明 |
