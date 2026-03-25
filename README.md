# poly-lp

支持单市场和多市场的 Polymarket LP daemon。当前版本是事件驱动架构，负责：

- 共享订阅市场盘口和用户订单/成交流
- 根据盘口深度和价差决定是否挂单、撤单
- 成交后触发风控和平仓
- 记录订单、成交、风控事件、心跳和运行日志
- 通过本地 admin API 做暂停、恢复、按 market 撤单、flatten、split、merge

更详细的运行逻辑见 [lp-architecture.md](docs/lp-architecture.md)。

如果你想看一份把认证、YES/NO token 挂单方式、完整运行链路、以及配置参数逐项解释整理在一起的说明，见 [polymarket-lp-parameter-guide.md](docs/polymarket-lp-parameter-guide.md)。

## 快速启动

1. 复制 [.env.example](.env.example) 为 `.env`
2. 修改 [config/default.toml](config/default.toml)
3. 启动：

```bash
cargo run -p poly-lp --release
```

如果你要跑独立的多市场配置文件：

```bash
cargo run -p poly-lp --release -- --config config/rewards-markets.toml
```

## `poly-lp` 必配项

- `POLYMARKET_PRIVATE_KEY`
- `APP__LP__TRADING__CONDITION_ID`
  这是 legacy 单市场入口。多市场模式下改成在 TOML 里写 `[[lp.markets]]`。
- `ADMIN_PASSWORD`

推荐也一起配：

- `POLYMARKET_RPC_URL`
  现在不只是 `split/merge` 用，启动时 approval 检查和自动 approve 也会用到。
- `APP__DATABASE__URL`
  如果你用 SQLite，本地缺失的数据库文件和父目录会自动创建。
- `APP__LP__REPORTING__OPERATOR_CHAT_IDS`
- `TELEGRAM_BOT_TOKENS`

## 外部 guard

`poly-guard` 现在是直接连 Polymarket 做全账户 `cancel_all`，不再回调主程序的 `/admin/lp/cancel-all`。

两机部署时建议这样配：

- 主程序机器
  `APP__GUARD__ENABLED=true`
  `APP__GUARD__BIND_ADDR=<guard机器IP或域名>`
  `APP__GUARD__PORT=37373`
  `APP__LP__CONTROL__BIND_ADDR` 只影响主程序自己的 admin API 监听地址，不再是 guard 的取消依赖。
- guard 机器
  也需要自己的 `POLYMARKET_PRIVATE_KEY`
  如果你用 proxy/safe 账户，也要配 `POLYMARKET_FUNDER_ADDRESS` / `POLYMARKET_SIGNATURE_TYPE`
  直连撤单只需要能认证到同一个交易账户；不依赖 `ADMIN_PASSWORD`
  需要正确的 `clob_base_url` 和 `chain_id`
  `APP__GUARD__ENABLED=true`
  `APP__GUARD__BIND_ADDR=0.0.0.0`
  `APP__GUARD__PORT=37373`

行为上，guard 触发的是全账户全撤；主程序自己的 `cancel-all` 只会撤当前配置 market。guard 触发后，主程序会在下一次 reconciliation 把外部撤单同步回来；如果你希望收敛更快，可以把 `APP__LP__CONTROL__RECONCILIATION_INTERVAL_SECS` 调小一点，例如 `5`。

## 当前可配置参数

主要配置都在 [config/default.toml](config/default.toml)：

- `[lp.trading]`
  legacy `condition_id`、API base URL、`chain_id`
- `[lp.inventory]`
  最低 USDC / token 库存、是否启动自动 split、启动 split 数量
- `[lp.strategy]`
  `quote_size`、`min_spread`、`min_depth`、`quote_offset_ticks`、`max_quote_age_secs`
- `[lp.risk]`
  `max_position`、`flat_position_tolerance`、`auto_flatten_after_fill`、`flatten_use_fok`
- `[lp.approvals]`
  `require_on_startup`、`auto_approve_on_startup`
- `[lp.reporting]`
  Telegram operator 报告
- `[lp.control]`
  admin bind、heartbeat interval、reconciliation interval
- `[lp.logging]`
  snapshot 周期、日志目录、滚动文件前缀、保留文件数、JSON 输出

## 多市场配置

多市场模式在同一个进程里启动多个独立 market runtime，但行情和用户流是共享连接，不会按 market 线性增加整套 WS 连接。运行时会共享：

- 一套 market data 订阅
- 一套 user orders 订阅
- 一套 user trades 订阅
- 一套 authenticated REST 执行入口

每个 market 继续有自己的决策、风控、订单状态和 admin 控制句柄。主程序内的 `cancel-all` 只作用目标 market；`poly-guard` 仍然保留全账户全撤语义。

TOML 结构：

```toml
[[lp.markets]]
condition_id = "0x..."
enabled = true
slug = "example-market"
question = "Will X happen?"

[lp.markets.strategy]
quote_size = 20.0
quote_mode = "both"

[lp.markets.capital]
buy_budget_usdc = 15.0
```

支持的 per-market override：

- `[lp.markets.strategy]`
  `quote_mode`、`quote_size`、`quote_offset_ticks`、`min_inside_ticks`、`min_inside_depth_multiple`
- `[lp.markets.inventory]`
  `min_usdc_balance`、`min_token_balance`、`auto_split_on_startup`、`startup_split_amount`
- `[lp.markets.risk]`
  `max_position`、`flat_position_tolerance`、`auto_flatten_after_fill`、`flatten_use_fok`、`stale_feed_after_secs`
- `[lp.markets.capital]`
  `buy_budget_usdc`

顶层 `[lp.inventory]`、`[lp.strategy]`、`[lp.risk]` 继续作为默认值。`buy_budget_usdc` 是可选硬上限，用来限制单个 market 能占用的买单资金；多 market 同时发单时，主程序也会做共享 USDC 预约，避免把同一笔余额重复拿去挂单。

## 挂单逻辑

当前策略是单层、对称、被动做市：

- 每边挂单量 = `lp.strategy.quote_size`
- 只有当 spread `>= min_spread` 且顶档深度 `>= min_depth` 才挂
- 买价 = `best_bid + quote_offset_ticks * tick_size`
- 卖价 = `best_ask - quote_offset_ticks * tick_size`
- 如果市场有 liquidity rewards，报价会尽量保持在 reward 合格区间内，不会再为了偏移把价格挪到奖励边界外
- 价格会被钳制，避免穿价
- 被动单使用 `GTC + post-only`
- fill 后 flatten 使用 `FAK`，除非 `flatten_use_fok = true`

## approve 支持

启动时会先检查所需授权：

- 市场 exchange：USDC allowance + CTF `setApprovalForAll`
- neg-risk market 额外检查 adapter
- 如果启用了启动自动 split，还会检查 Conditional Tokens 合约的 USDC allowance

配置行为：

- `lp.approvals.require_on_startup = true`
  缺授权直接拒绝启动
- `lp.approvals.auto_approve_on_startup = true`
  启动时自动发链上 approve 交易，然后再继续启动

当前自动授权模式发的是：

- `USDC approve(MAX)`
- `CTF setApprovalForAll(true)`

默认是安全模式：检查，但不自动放权。

## condition_id 怎么确认

`condition_id` 是市场 ID，不是 YES/NO token id。官方概念说明见：

- `Condition ID` / `Token IDs`：<https://docs.polymarket.com/concepts/markets-events>
- market by slug：<https://docs.polymarket.com/api-reference/markets/get-market-by-slug>

最稳的确认方法有两种：

1. 你直接给我 Polymarket 的 market URL 或 event URL
2. 你自己查官方 Gamma API

按 slug 查单个 market：

```bash
curl "https://gamma-api.polymarket.com/markets/slug/<slug>"
```

返回里重点看：

- `conditionId`
- `question`
- `slug`
- `clobTokenIds`

按 event slug 查整组市场：

```bash
curl "https://gamma-api.polymarket.com/events?slug=<event-slug>"
```

如果你把具体的 event 链接、market 链接、slug，或者完整 question 发我，我可以帮你确认该填哪个 `condition_id`。

如果你想直接在本地交互式修改目标 market，可以用独立 CLI：

```bash
cargo run -p poly-lp --bin poly-target -- "https://polymarket.com/event/..."
```

它支持 4 种输入：

1. `event URL`
2. `market URL`
3. `slug`
4. `condition_id`

如果解析出多个候选市场，会在终端里让你选；确认后会把选择结果写回 `config/default.toml` 里的 `lp.trading.condition_id`。改完后重启 `poly-lp` 才会生效。

如果你只想先演练，不想动默认配置，可以显式指定另一个 TOML 文件：

```bash
cargo run -p poly-lp --bin poly-target -- --config /tmp/next-target.toml "https://polymarket.com/event/..."
```

## Rewards 分析 CLI

如果你想先离线评估哪个 reward pool 更值得做，可以先生成全市场奖励报告：

```bash
cargo run -p poly-lp --bin poly-rewards -- --out-dir outputs/rewards
```

它会：

- 扫描所有 active reward markets
- 拉公开 order book
- 估算 `outer_low_risk` 和 `aggressive_mid` 两种 profile
- 输出 [outputs/rewards/report.json](outputs/rewards/report.json) 和 [outputs/rewards/shortlist.toml](outputs/rewards/shortlist.toml)

`report.json` 是完整原始分析结果，适合后续二次筛选；`shortlist.toml` 是直接可读的排行摘录。

如果只想快速抽样跑一小部分市场：

```bash
cargo run -p poly-lp --bin poly-rewards -- --top-n 10 --max-markets 100 --out-dir outputs/rewards
```

## Rewards 二次筛选 CLI

基于已经生成的 `report.json`，可以继续做纯离线筛选，不需要重新抓网络：

```bash
cargo run -p poly-lp --bin poly-rewards-filter -- --report outputs/rewards/report.json
```

默认是：

- `profile = outer_low_risk`
- `sort = balanced`

这个 `balanced` 排序会在 `roi_daily_conservative` 的基础上，再结合这些风险代理做平衡：

- `inside_ticks`
- `inside_depth / effective_quote_size`
- `reward_gap_headroom / reward_band`
- `crowdedness_score`
- `current_config_eligible`

常用筛选示例：

```bash
cargo run -p poly-lp --bin poly-rewards-filter -- --report outputs/rewards/report.json --profile outer_low_risk --sort balanced --top-n 20
```

```bash
cargo run -p poly-lp --bin poly-rewards-filter -- --report outputs/rewards/report.json --profile outer_low_risk --sort roi_daily --min-inside-ticks 2 --max-crowdedness 20 --eligible-only
```

```bash
cargo run -p poly-lp --bin poly-rewards-filter -- --report outputs/rewards/report.json --sort safety --out outputs/rewards/filtered.json
```

支持的排序键：

- `balanced`
- `roi_daily`
- `roi_annual`
- `estimated_daily_reward`
- `daily_reward`
- `crowdedness`
- `safety`

如果你想直接把筛选结果变成主程序可运行的多市场配置：

```bash
cargo run -p poly-lp --bin poly-rewards-filter -- \
  --report outputs/rewards/report.json \
  --profile outer_low_risk \
  --sort balanced \
  --top-n 8 \
  --emit-multi-config config/rewards-markets.toml
```

也可以基于别的基础配置生成：

```bash
cargo run -p poly-lp --bin poly-rewards-filter -- \
  --report outputs/rewards/report.json \
  --emit-multi-config config/rewards-markets.toml \
  --base-config config/default.toml \
  --config-top-n 5
```

生成文件会：

- 保留 base config 的公共配置
- 为选中的 market 追加 `[[lp.markets]]`
- 默认把 `strategy.quote_size` 设成 `rewards_min_size`
- 在每个 market block 前写参考注释，例如 `daily_reward`、`recommended_price`、`roi_daily_conservative`、`crowdedness_score`

这些注释不会参与运行；你可以直接手动改每个 market 的 `quote_size`、策略覆盖和 `buy_budget_usdc`。

## 启动后检查

健康检查：

```bash
curl -H "Authorization: Bearer <ADMIN_PASSWORD>" http://127.0.0.1:36363/admin/lp/health
curl -H "Authorization: Bearer <ADMIN_PASSWORD>" http://127.0.0.1:36363/admin/lp/state
```

多市场模式下可以按 `condition_id` 查单个 market：

```bash
curl -H "Authorization: Bearer <ADMIN_PASSWORD>" "http://127.0.0.1:36363/admin/lp/health?condition_id=0x..."
curl -H "Authorization: Bearer <ADMIN_PASSWORD>" "http://127.0.0.1:36363/admin/lp/state?condition_id=0x..."
```

常用控制：

```bash
curl -X POST -H "Authorization: Bearer <ADMIN_PASSWORD>" http://127.0.0.1:36363/admin/lp/pause
curl -X POST -H "Authorization: Bearer <ADMIN_PASSWORD>" http://127.0.0.1:36363/admin/lp/resume
curl -X POST -H "Authorization: Bearer <ADMIN_PASSWORD>" http://127.0.0.1:36363/admin/lp/cancel-all
curl -X POST -H "Authorization: Bearer <ADMIN_PASSWORD>" http://127.0.0.1:36363/admin/lp/flatten
```

多市场模式下不带 `condition_id` 会广播到所有配置 market；带上 `condition_id` 时只影响目标 market。`split` / `merge` 在多市场模式下必须带 `condition_id`。

## 日志

- stdout：实时进程日志
- `logs/`：按天滚动的结构化日志
- SQLite：订单、成交、仓位、heartbeat、risk event、control action
