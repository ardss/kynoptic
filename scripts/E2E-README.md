# kynoptic 标准化 E2E 测试

## 运行
```bash
bash scripts/e2e.sh                # 默认测 http://127.0.0.1:8422
bash scripts/e2e.sh http://127.0.0.1:9999   # 指定面板地址
```

前置：agent-browser CLI；本机托盘/仪表盘正在运行；`D:/Kynoptic/data/kynoptic.db` 存在（C 段铁律测试用真实库副本）。

## 覆盖范围
- A 后端接口：首页 200 / overview 三指标 / summary 计数 / insights 结构
- B 真浏览器：7 个页签逐一打开断言（人在场渲染、自动化/前台/首末在场、时间线 >=5 条、
  中英切换、报告目标/热力图/趋势、Top 榜、鼠标左键悬停值与 API 一致、洞察卡或空状态、
  设置硬件面板+桥接阈值）
- C 铁律（临时副本库）：`cleanup 0` 不删任何原始事件；`--yes` 且 <30 天被拒绝；
  非法天数报错而非静默

## 已知盲区（诚实清单）
1. 不捕获浏览器 console 错误（agent-browser 无此 API）——JS 崩溃靠"数字未渲染"间接暴露
2. 悬停一致性检查读取的是 title 元素内容，未派发真实 mouseover 事件
3. 硬编码 D:/Kynoptic 路径——换机器需改 REAL_DB/BIN 两个变量
4. 无首装空库、午夜跨界、监控器全关场景
5. 无多日数据积累类测试（热力图多周形态）
