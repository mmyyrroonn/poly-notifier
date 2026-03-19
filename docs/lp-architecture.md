# LP 架构说明

## 总体结构

当前实现是单市场、事件驱动的 LP daemon。入口在 [main.rs](../crates/poly-notifier/src/main.rs)，核心模块拆成四层：

- `pn-polymarket`
  Polymarket 官方 Rust SDK 的封装层，负责 market data、user stream、挂单、撤单、heartbeat、split/merge、approval 检查与自动 approve
- `pn-lp`
  运行时核心，维护内存状态、做决策、执行风控、对账、周期报告
- `pn-admin`
  本地控制面，暴露 `/admin/lp/*`
- `pn-common`
  配置、数据库 helper、公共模型

## 事件流

运行时有三类输入：

1. 市场事件
   盘口、tick size
2. 用户事件
   订单状态、成交
3. 本地事件
   心跳、周期对账、operator 控制命令

这些事件进入 `LpService` 后，会更新一份统一的 `RuntimeState`。这份状态包含：

- 市场元数据
- 每个 outcome token 的 orderbook
- 当前 open orders
- 成交历史
- 当前 positions
- USDC 和 token 库存
- 信号状态
- 运行标志位
  `paused` / `flattening` / `heartbeat_healthy` / `market_feed_healthy` / `user_feed_healthy`

## 决策系统

决策逻辑在 [decision.rs](../crates/pn-lp/src/decision.rs)。

当前版本只做基础 orderbook-driven quoting，不带 inventory skew 和多层报价。流程是：

1. 如果 runtime 不健康、手动暂停、正在 flatten，直接停止报价并撤单
2. 如果任一 signal gate 不允许报价，直接停止报价并撤单
3. 对每个 outcome token 检查：
   - 是否有 bid / ask
   - spread 是否够宽
   - 顶档深度是否足够
4. 生成一组 `desired_quotes`
5. `ExecutionEngine` 把 `desired_quotes` 和 `open_orders` 做 diff，只撤必要的单，只补必要的新单

当前参数暴露已经足够覆盖 v1：

- `quote_size`
- `min_spread`
- `min_depth`
- `quote_offset_ticks`
- `max_quote_age_secs`

## 风控系统

风控逻辑在 [risk.rs](../crates/pn-lp/src/risk.rs)。

主要风控条件：

- fill 之后自动 flatten
- 持仓超过 `max_position`
- 市场流、用户流或 heartbeat 超时
- 手动 pause / cancel-all / flatten

当前默认行为是偏保守的：

- 一旦 fill，先撤相关工作单
- 然后发对侧 `FAK` flatten 单
- flatten 成功前，`flattening = true`
- runtime 在 flatten 期间不继续报价

这保证了 v1 的第一优先级是“先不裸奔”，不是“尽量持续挂单”。

## 执行层

执行层封装在 [lp.rs](../crates/pn-polymarket/src/lp.rs)。

关键行为：

- passive quote：`GTC + post-only`
- flatten：`FAK` 或 `FOK`
- 撤单：支持单撤、批量撤、按 market 撤、全撤
- heartbeat：默认每 5 秒一次
- split / merge：通过 CTF 合约直接发链上交易

approval 也在这一层处理。原因是：

- 它本质是和 Polymarket / Polygon 交互
- 不应该混进决策或风控状态机
- 启动时检查一次最合适

## approve 设计

当前支持两种模式：

1. `require_on_startup = true`
   缺授权直接报错退出
2. `auto_approve_on_startup = true`
   启动时自动发 approve，然后再检查一次确认成功

检查目标：

- 当前市场对应的 exchange
- neg-risk market 的 adapter
- 如果启用了启动自动 split，再额外检查 Conditional Tokens 合约对 USDC 的 allowance

当前自动授权发的是：

- `USDC approve(MAX)`
- `CTF setApprovalForAll(true)`

默认不会静默自动授权，必须显式打开。

## 数据与日志

落库内容：

- `lp_orders`
- `lp_trades`
- `lp_positions`
- `lp_risk_events`
- `lp_heartbeat`
- `lp_reports`
- `lp_control_actions`

日志分三层：

- stdout
- `logs/` 滚动文件
- SQLite 结构化事件

关键运行日志包括：

- signal 变化
- 决策变化
- quote 提交和失败
- order update
- trade fill
- flatten 成功/失败
- risk 事件
- heartbeat
- reconcile

## 目前边界

当前版本故意保守，边界也明确：

- 单市场
- 单层、对称报价
- 不做 inventory skew
- 不做多层 ladder
- 不做运行中自动补 approve
- 不做自动 continuous split/merge

如果下一阶段要继续演进，优先级建议是：

1. bid / ask 分开配置
2. 多层报价
3. inventory skew
4. 外部撤单信号接入
5. 运行中手动触发 approve / inventory 补充
