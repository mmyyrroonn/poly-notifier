# Polymarket LP 参数与下单逻辑说明

这份文档把当前仓库里和 Polymarket LP 相关的核心概念、认证方式、YES/NO token 挂单方式、完整运行链路，以及配置参数解释整理到一起，方便后续查阅。

相关代码入口：

- 运行入口：[`crates/poly-notifier/src/main.rs`](../crates/poly-notifier/src/main.rs)
- 执行层：[`crates/pn-polymarket/src/lp.rs`](../crates/pn-polymarket/src/lp.rs)
- 决策层：[`crates/pn-lp/src/decision.rs`](../crates/pn-lp/src/decision.rs)
- 风控层：[`crates/pn-lp/src/risk.rs`](../crates/pn-lp/src/risk.rs)
- 架构说明：[`docs/lp-architecture.md`](./lp-architecture.md)

## 1. Polymarket 到底需不需要 API Key

结论：需要交易认证，但这个仓库把认证细节尽量封在 SDK 里了。

Polymarket 认证大致分两层：

- 公开读接口：不需要认证
  - Gamma API
  - Data API
  - CLOB 的只读接口，例如 orderbook、price、spread
- 交易接口：需要认证
  - 下单
  - 撤单
  - 查询用户 open orders
  - heartbeat

官方文档里，CLOB 交易接口是两级认证模型：

- L1：钱包私钥签名
- L2：API key / secret / passphrase

这个仓库里，运行时你通常只需要提供：

- `POLYMARKET_PRIVATE_KEY`

然后 SDK 在 `authenticate()` 时建立 authenticated client，后续下单、撤单、heartbeat 都通过这个 client 走。

这意味着：

- 从“使用者视角”看，你不需要手工管理一堆 header
- 但从“底层机制”看，交易接口仍然不是匿名调用

另外，链上 approve / split / merge 还会用到 Polygon RPC：

- `POLYMARKET_RPC_URL`

## 2. 一个市场里的 YES / NO token 是怎么挂单的

### 2.1 `condition_id` 和 `asset_id` 不是一回事

- `condition_id`：市场 ID
- `asset_id`：某个 outcome token 的 ID

在 binary market 里，通常会有两个 outcome token：

- YES token
- NO token

代码启动时会先根据 `condition_id` 拉市场元数据，然后拿到该市场下所有 token：

- 每个 token 有自己的 `asset_id`
- 每个 token 有自己的 `outcome`
- 每个 token 有自己的 `tick_size`

所以对 LP 来说，不是“对市场挂单”，而是“对具体 token 挂单”。

### 2.2 YES / NO 各自都有自己的 orderbook

当前实现会为市场里的每个 token 分别维护：

- book
- open orders
- 持仓
- token balance

这意味着一个 binary market 在理想情况下最多会同时有四类被动单：

- `BUY YES`
- `SELL YES`
- `BUY NO`
- `SELL NO`

### 2.3 当前 LP 不是做 YES/NO 联动对冲

当前版本是“逐 token 独立做单层被动报价”，不是“把 YES 和 NO 当成互补腿做联动策略”。

也就是说：

- YES 这边看 YES 自己的 best bid / best ask
- NO 这边看 NO 自己的 best bid / best ask
- 是否挂单、挂什么价，都分别独立计算

它不会因为挂了 `BUY YES` 就自动配一张 `SELL NO` 作为策略腿。

### 2.4 买单和卖单分别依赖什么库存

- 挂 `BUY <token>` 需要 USDC 余额足够
- 挂 `SELL <token>` 需要你已经持有该 token

所以：

- 如果你只有 USDC、没有 split 出 YES/NO inventory，刚开始通常只能挂买单
- 要能挂卖单，得先持有对应 token

这也是为什么系统里有：

- `split`
- `merge`
- approval 检查
- inventory 水位配置

## 3. 当前 LP 的完整逻辑链路

### 3.1 启动阶段

程序启动后会依次做这些事：

1. 读取配置和环境变量
2. 校验 `condition_id`
3. 连接 authenticated Polymarket execution client
4. 检查 approvals
5. 如配置允许，自动发 approve
6. bootstrap 当前市场状态
7. 构建 `RuntimeState`
8. 启动 LP service 和本地 admin API

### 3.2 bootstrap 会拿什么

启动时会拉取：

- market metadata
- 每个 token 的 orderbook
- 当前 open orders
- 当前 positions
- USDC 和 token balances

### 3.3 运行时的三类输入

LP service 主要接三类输入：

- 市场事件
  - 盘口变化
  - tick size 更新
- 用户事件
  - 订单状态变化
  - 成交
- 本地事件
  - heartbeat 定时器
  - reconciliation 定时器
  - admin 控制命令

### 3.4 什么时候允许报价

先过几层 gate：

- 没有手动 `paused`
- 没有处于 `flattening`
- `heartbeat_healthy = true`
- `market_feed_healthy = true`
- `user_feed_healthy = true`
- 所有 signal 都允许报价

任意一层不满足，就直接停报价并撤单。

### 3.5 决策逻辑怎么生成 quotes

对于市场里的每个 token：

1. 必须有双边盘口
2. `spread >= min_spread`
3. 顶档深度 `>= min_depth`
4. 根据 `quote_mode`、tick size 和 `quote_offset_ticks` 计算买卖价
5. 如果 USDC 足够，则生成该 token 的买单
6. 如果该 token 库存足够，则生成该 token 的卖单

当前支持三种报价模式：

- `join`
  - `buy_price = best_bid`
  - `sell_price = best_ask`
- `inside`
  - `buy_price = best_bid + quote_offset_ticks * tick_size`
  - `sell_price = best_ask - quote_offset_ticks * tick_size`
- `outside`
  - `buy_price = best_bid - quote_offset_ticks * tick_size`
  - `sell_price = best_ask + quote_offset_ticks * tick_size`

同时会做价格钳制：

- `inside` 模式避免穿价，保持 `post_only`
- `outside` 模式避免跑到小于最小 tick 或大于 `1 - tick`

### 3.6 执行层怎么下单

被动 quote 使用：

- `GTC`
- `post_only = true`

也就是只做 maker，不主动吃单。

如果目标报价和当前 open orders 不一致，执行层会：

1. 先撤旧单
2. 再重挂新单

如果目标报价和当前挂单一致，并且单龄没有超过 `max_quote_age_secs`，则不刷新。

### 3.7 成交后的风控

当前版本非常保守。

一旦收到 fill：

1. `Pause`
2. `CancelAll`
3. 如果启用了 `auto_flatten_after_fill`
   - 对当前成交的同一个 `asset_id` 发反向 flatten 单

例子：

- `BUY YES` 成交后，会尝试 `SELL YES` flatten
- `SELL NO` 成交后，会尝试 `BUY NO` flatten

不会自动改成去交易另一边 outcome。

flatten 用的是：

- `FAK`，如果 `flatten_use_fok = false`
- `FOK`，如果 `flatten_use_fok = true`

### 3.8 对账与恢复

系统会周期性 reconcile：

- open orders
- positions
- account balances

如果已经：

- paused
- 不在 flattening
- 没有 open orders
- 持仓绝对值都小于 `flat_position_tolerance`

那么风控会允许自动 `resume`。

## 4. 具体参数解释

下面按 [`config/default.toml`](../config/default.toml) 分组说明。

说明：

- `[database]`、`[telegram]`、`[monitor]`、`[alert]`、`[scheduler]`、`[admin]` 属于外围系统参数
- `[lp.*]` 才是 LP 核心交易与风控参数

### 4.1 `[database]`

- `url`
  - 数据库连接串
  - 默认是本地 SQLite 文件
- `max_connections`
  - 连接池最大连接数

### 4.2 `[telegram]`

- `rate_limit_per_user`
  - Telegram 发送限频
  - 与 LP 决策本身无关

### 4.3 `[monitor]`

这部分是市场监控相关参数，不直接决定 LP quote 行为：

- `subscription_refresh_interval_secs`
  - 订阅列表刷新周期
- `ws_ping_interval_secs`
  - WebSocket ping 周期
- `reconnect_base_delay_secs`
  - 初始重连等待
- `reconnect_max_delay_secs`
  - 最大重连等待

### 4.4 `[alert]`

这部分是告警系统参数：

- `cache_refresh_interval_secs`
  - 告警缓存刷新周期
- `default_cooldown_minutes`
  - 默认冷却时间
- `price_flush_interval_secs`
  - 内存价格刷盘周期

### 4.5 `[scheduler]`

- `daily_summary_cron`
  - 每日摘要任务 cron

### 4.6 `[admin]`

- `port`
  - 本地 admin HTTP 服务端口

### 4.7 `[lp.trading]`

- `condition_id`
  - 要做市的市场 ID
  - 当前 daemon 只支持单市场
- `clob_base_url`
  - Polymarket CLOB 基础地址
  - 下单、撤单、市场簿、heartbeat 都依赖它
- `gamma_base_url`
  - Gamma API 基础地址
  - 当前 LP 主交易链路基本不依赖它，更偏向市场发现 / 搜索
- `data_api_base_url`
  - Data API 基础地址
  - 用于 positions 和 account snapshot 等对账数据
- `chain_id`
  - Polygon chain ID
  - 主网是 `137`

### 4.8 `[lp.inventory]`

- `min_usdc_balance`
  - 挂买单的最低 USDC 水位
  - 如果账户 USDC 小于它，就不再生成买单
- `min_token_balance`
  - 挂卖单的最低 token 库存水位
  - 对某个 token 库存不足时，不生成该 token 的卖单
- `auto_split_on_startup`
  - 启动时是否允许自动 split inventory
- `startup_split_amount`
  - 启动自动 split 的数量

注意：

- 这两个 `min_*_balance` 是“是否允许继续报价”的开关水位
- 不是“每笔订单必须正好用掉多少资金”

### 4.9 `[lp.strategy]`

- `quote_mode`
  - `join`：挂在 `best_bid / best_ask`
  - `inside`：往中间收，更积极的 maker quote
  - `outside`：往外放，更保守、更不容易成交
- `quote_size`
  - 每张报价单的 size
  - 当前买卖两边共用一个值
- `min_spread`
  - 只有当顶档价差足够宽才报价
- `min_depth`
  - 只有当顶档深度足够厚才报价
  - 代码里用的是 `min(top_bid_size, top_ask_size)`
- `quote_offset_ticks`
  - 报价偏移多少个 tick
  - 在 `inside` 模式下，值越大越靠近中间价
  - 在 `outside` 模式下，值越大越远离盘口
- `max_quote_age_secs`
  - 挂单超过这个年龄，即使目标报价没变，也会刷新
- `default_external_signal`
  - 外部 signal gate 的默认初值
  - 当前没有真实外部信号源时，这个值决定启动后 external gate 默认开还是关

### 4.10 `[lp.risk]`

- `max_position`
  - 单 token 绝对持仓上限
  - 超过就 pause + cancel-all
- `flat_position_tolerance`
  - 认为“已经基本 flat”时允许的残余持仓范围
- `auto_flatten_after_fill`
  - 成交后是否自动 flatten
- `flatten_use_fok`
  - flatten 使用 FOK 还是 FAK
  - `false` 表示 FAK
  - `true` 表示 FOK
- `stale_feed_after_secs`
  - market feed / user feed 超过多久没更新就判 stale

### 4.11 `[lp.approvals]`

- `require_on_startup`
  - 启动时如果缺授权，是否直接拒绝启动
- `auto_approve_on_startup`
  - 启动时如果缺授权，是否自动发链上授权交易

当前启动检查的授权大致包括：

- market exchange 的 USDC allowance
- market exchange 的 CTF approval
- neg-risk adapter 的 allowance / approval
- 如果启用自动 split，还会检查 Conditional Tokens 合约的 USDC allowance

### 4.12 `[lp.reporting]`

- `operator_bot_id`
  - 用哪个 Telegram bot 发 operator 消息
- `operator_chat_ids`
  - 接收报告和风险提醒的 chat 列表
- `summary_interval_secs`
  - 周期摘要发送间隔

### 4.13 `[lp.control]`

- `bind_addr`
  - admin API 绑定地址
- `heartbeat_interval_secs`
  - 向 CLOB 发送 heartbeat 的周期
- `reconciliation_interval_secs`
  - open orders / positions / balances 对账周期

### 4.14 `[lp.logging]`

- `snapshot_interval_secs`
  - 状态快照落库周期
- `directory`
  - 日志目录
- `file_prefix`
  - 日志滚动文件名前缀
- `max_files`
  - 保留多少个滚动日志文件
- `json`
  - 文件日志是否输出 JSON

## 5. 一个具体例子

假设某个市场有两个 token：

- YES：`asset_yes`
- NO：`asset_no`

并且参数是：

- `quote_mode = "inside"`
- `quote_size = 25`
- `min_spread = 0.01`
- `min_depth = 25`
- `quote_offset_ticks = 1`
- tick size 都是 `0.01`

盘口如下：

- YES：best bid = `0.42`，best ask = `0.45`
- NO：best bid = `0.55`，best ask = `0.58`

那么 LP 会分别计算：

- YES buy = `0.43`
- YES sell = `0.44`
- NO buy = `0.56`
- NO sell = `0.57`

如果账户状态是：

- USDC 充足
- YES 库存充足
- NO 库存充足

那就会尝试挂 4 张单：

- `BUY YES 25 @ 0.43`
- `SELL YES 25 @ 0.44`
- `BUY NO 25 @ 0.56`
- `SELL NO 25 @ 0.57`

如果这时：

- `BUY YES 25 @ 0.43` 被成交

当前风控默认会：

1. pause
2. cancel-all
3. 对 `YES` 发反向 flatten

也就是去：

- `SELL YES`

而不是自动去交易 `NO`。

## 6. 和官方文档的关系

当前仓库实现和官方概念基本一致：

- 公开读接口不需要认证，交易接口需要认证
- 订单最终是按 token 下的，不是按整个 market 直接下
- passive quote 适合 `GTC + post-only`
- flatten 适合 `FAK / FOK`
- 买单依赖 USDC allowance
- 卖单依赖 conditional token approval 和 token inventory

可参考官方文档：

- 认证：<https://docs.polymarket.com/cn/api-reference/authentication>
- Orders 概览：<https://docs.polymarket.com/trading/orders/overview>
- 库存管理：<https://docs.polymarket.com/market-makers/inventory>
- 订单生命周期：<https://docs.polymarket.com/concepts/order-lifecycle>

## 7. 当前版本的边界

这套 LP 当前仍然是 v1 风格，边界比较明确：

- 单市场
- 单层报价
- 对称参数
- 不做 inventory skew
- 不做 YES / NO 联动策略
- 不做多层 ladder
- 成交后偏保守，优先 flatten，不追求持续带仓做市

如果后续要增强，比较自然的方向是：

1. bid / ask 分开配置
2. YES / NO 相关性约束
3. inventory skew
4. 多层报价
5. 更细的撤单与重挂策略
