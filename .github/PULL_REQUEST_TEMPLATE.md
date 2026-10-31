## 预期文件集

<!-- 逐个列出本 PR 应改动的文件。合并前以 `gh pr diff --name-only` 核对：多一个、少一个都要在这里说明原因。 -->

-

## 根因/批次

<!-- 本 PR 属于哪个批次、修复哪个根因；无归属时写 non-batch 并说明来源。 -->

## TDD 轨迹或 non-cargo 说明

<!-- cargo 侧改动：先写失败的测试再实现，列出新增/变更的测试名。非 cargo 改动（文档/脚本/CI）：说明为什么不需要测试。 -->

## 亲验凭证

<!-- 合并前实际跑过的命令与关键输出摘要；没跑的检查如实标明未跑，不填"通过"。 -->

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] `gh pr diff --name-only` 与「预期文件集」一致
