use std::collections::HashMap;
use std::env;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use alloy::providers::ProviderBuilder;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use anyhow::{Context, Result};
use futures_util::{future::try_join_all, StreamExt};
use polymarket_client_sdk::auth::{LocalSigner, Signer, Uuid};
use polymarket_client_sdk::clob;
use polymarket_client_sdk::clob::types::request::{
    BalanceAllowanceRequest, CancelMarketOrderRequest, OrderBookSummaryRequest, OrdersRequest,
};
use polymarket_client_sdk::clob::types::{
    Amount, AssetType, OrderStatusType, OrderType, Side, SignatureType, TraderSide,
};
use polymarket_client_sdk::ctf;
use polymarket_client_sdk::data;
use polymarket_client_sdk::data::types::request::PositionsRequest;
use polymarket_client_sdk::data::types::MarketFilter;
use polymarket_client_sdk::types::{Address, Decimal, B256, U256};
use polymarket_client_sdk::{
    contract_config, derive_proxy_wallet, derive_safe_wallet, PRIVATE_KEY_VAR,
};
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::rewards::{RewardClient, RewardSnapshot};

const DEFAULT_POLYGON_RPC_URL: &str = "https://polygon-rpc.com";
const INITIAL_OPEN_ORDERS_CURSOR: &str = "MA==";
const TERMINAL_OPEN_ORDERS_CURSOR: &str = "LTE=";
const FUNDER_ADDRESS_VAR: &str = "POLYMARKET_FUNDER_ADDRESS";
const SIGNATURE_TYPE_VAR: &str = "POLYMARKET_SIGNATURE_TYPE";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TradingAccountKind {
    Eoa,
    Proxy,
    GnosisSafe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TradingAccount {
    signer: Address,
    funder: Option<Address>,
    kind: TradingAccountKind,
}

impl TradingAccountKind {
    fn signature_type(self) -> SignatureType {
        match self {
            Self::Eoa => SignatureType::Eoa,
            Self::Proxy => SignatureType::Proxy,
            Self::GnosisSafe => SignatureType::GnosisSafe,
        }
    }

    fn requires_onchain_approvals(self) -> bool {
        matches!(self, Self::Eoa)
    }
}

impl TradingAccount {
    fn trading_address(self) -> Address {
        self.funder.unwrap_or(self.signer)
    }

    fn signature_type(self) -> SignatureType {
        self.kind.signature_type()
    }

    fn requires_onchain_approvals(self) -> bool {
        self.kind.requires_onchain_approvals()
    }
}

sol! {
    #[sol(rpc)]
    interface IERC20 {
        function approve(address spender, uint256 value) external returns (bool);
        function allowance(address owner, address spender) external view returns (uint256);
    }

    #[sol(rpc)]
    interface IERC1155 {
        function setApprovalForAll(address operator, bool approved) external;
        function isApprovedForAll(address account, address operator) external view returns (bool);
    }
}

#[derive(Debug, Clone)]
pub struct ExecutionConfig {
    pub condition_id: String,
    pub clob_base_url: String,
    pub data_api_base_url: String,
    pub chain_id: u64,
}

#[derive(Debug, Clone)]
pub struct DirectCancelConfig {
    pub clob_base_url: String,
    pub chain_id: u64,
}

#[derive(Debug, Clone)]
pub struct ApprovalTarget {
    pub label: String,
    pub address: Address,
    pub require_usdc_allowance: bool,
    pub require_ctf_approval: bool,
}

#[derive(Debug, Clone)]
pub struct ApprovalCheck {
    pub label: String,
    pub address: Address,
    pub require_usdc_allowance: bool,
    pub require_ctf_approval: bool,
    pub usdc_allowance_ready: bool,
    pub ctf_approval_ready: bool,
}

#[derive(Debug, Clone)]
pub struct ApprovalStatus {
    pub targets: Vec<ApprovalCheck>,
}

#[derive(Debug, Clone)]
pub struct TokenMetadata {
    pub asset_id: String,
    pub outcome: String,
    pub tick_size: Decimal,
}

#[derive(Debug, Clone)]
pub struct MarketMetadata {
    pub condition_id: String,
    pub question: String,
    pub tokens: Vec<TokenMetadata>,
}

#[derive(Debug, Clone)]
pub struct BookLevel {
    pub price: Decimal,
    pub size: Decimal,
}

#[derive(Debug, Clone)]
pub struct BookSnapshot {
    pub asset_id: String,
    pub bids: Vec<BookLevel>,
    pub asks: Vec<BookLevel>,
}

#[derive(Debug, Clone)]
pub struct ManagedOrder {
    pub order_id: String,
    pub asset_id: String,
    pub side: QuoteSide,
    pub price: Decimal,
    pub size: Decimal,
    pub status: String,
}

#[derive(Debug, Clone)]
pub struct TradeFill {
    pub trade_id: String,
    pub order_id: Option<String>,
    pub asset_id: String,
    pub side: QuoteSide,
    pub price: Decimal,
    pub size: Decimal,
    pub status: String,
}

#[derive(Debug, Clone)]
pub struct PositionSnapshot {
    pub asset_id: String,
    pub size: Decimal,
    pub avg_price: Decimal,
}

#[derive(Debug, Clone)]
pub struct AccountSnapshot {
    pub usdc_balance: Decimal,
    pub token_balances: HashMap<String, Decimal>,
}

#[derive(Debug, Clone)]
pub struct QuoteRequest {
    pub asset_id: String,
    pub side: QuoteSide,
    pub price: Decimal,
    pub size: Decimal,
}

#[derive(Debug, Clone)]
pub struct FlattenRequest {
    pub asset_id: String,
    pub side: QuoteSide,
    pub price: Decimal,
    pub size: Decimal,
    pub use_fok: bool,
}

#[derive(Debug, Clone)]
pub struct BootstrapState {
    pub market: MarketMetadata,
    pub books: Vec<BookSnapshot>,
    pub open_orders: Vec<ManagedOrder>,
    pub positions: Vec<PositionSnapshot>,
    pub account: AccountSnapshot,
}

#[derive(Debug, Clone)]
pub struct ReconciliationState {
    pub books: Vec<BookSnapshot>,
    pub open_orders: Vec<ManagedOrder>,
    pub positions: Vec<PositionSnapshot>,
    pub account: AccountSnapshot,
}

#[derive(Debug, Clone)]
pub enum QuoteSide {
    Buy,
    Sell,
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
    Book(BookSnapshot),
    Order(ManagedOrder),
    Trade(TradeFill),
    TickSize {
        asset_id: String,
        new_tick_size: Decimal,
    },
}

#[derive(Clone)]
pub struct PolymarketExecutionClient {
    config: ExecutionConfig,
    signer: PrivateKeySigner,
    account: TradingAccount,
    clob: clob::Client<
        polymarket_client_sdk::auth::state::Authenticated<polymarket_client_sdk::auth::Normal>,
    >,
    market_ws: clob::ws::Client,
    user_ws: clob::ws::Client<
        polymarket_client_sdk::auth::state::Authenticated<polymarket_client_sdk::auth::Normal>,
    >,
    data: data::Client,
    condition_id: B256,
    market: Arc<MarketMetadata>,
    asset_ids: Arc<Vec<U256>>,
    exchange_contract: Address,
    collateral_token: Address,
    conditional_tokens: Address,
    neg_risk_adapter: Option<Address>,
}

pub struct PolymarketDirectCancelClient {
    clob: AuthenticatedClobClient,
}

type AuthenticatedClobClient = clob::Client<
    polymarket_client_sdk::auth::state::Authenticated<polymarket_client_sdk::auth::Normal>,
>;

async fn connect_authenticated_clob(
    clob_base_url: &str,
    chain_id: u64,
) -> Result<(PrivateKeySigner, TradingAccount, AuthenticatedClobClient)> {
    let private_key = env::var(PRIVATE_KEY_VAR)
        .with_context(|| format!("{PRIVATE_KEY_VAR} environment variable is required"))?;
    let signer = LocalSigner::from_str(&private_key)?.with_chain_id(Some(chain_id));
    let funder_address = env::var(FUNDER_ADDRESS_VAR).ok();
    let signature_type = env::var(SIGNATURE_TYPE_VAR).ok();
    let account = resolve_trading_account(
        signer.address(),
        chain_id,
        funder_address.as_deref(),
        signature_type.as_deref(),
    )?;

    let clob_config = clob::Config::builder()
        .heartbeat_interval(Duration::from_secs(5))
        .build();
    let mut auth = clob::Client::new(clob_base_url, clob_config)?
        .authentication_builder(&signer)
        .signature_type(account.signature_type());
    if let Some(funder) = account.funder {
        auth = auth.funder(funder);
    }
    let mut clob = auth.authenticate().await?;

    if clob.heartbeats_active() {
        clob.stop_heartbeats().await?;
    }

    Ok((signer, account, clob))
}

impl PolymarketDirectCancelClient {
    pub async fn connect(config: DirectCancelConfig) -> Result<Self> {
        info!(
            target: "pn_polymarket::direct_cancel",
            clob_base_url = %config.clob_base_url,
            chain_id = config.chain_id,
            "connecting direct Polymarket cancel client"
        );
        let (_signer, account, clob) =
            connect_authenticated_clob(&config.clob_base_url, config.chain_id).await?;

        info!(
            target: "pn_polymarket::direct_cancel",
            signer = %account.signer,
            trading_address = %account.trading_address(),
            signature_type = ?account.signature_type(),
            "direct Polymarket cancel client connected"
        );

        Ok(Self { clob })
    }

    pub async fn cancel_all(&self) -> Result<()> {
        info!(
            target: "pn_polymarket::direct_cancel",
            "canceling all open orders"
        );
        self.clob.cancel_all_orders().await?;
        Ok(())
    }
}

impl PolymarketExecutionClient {
    pub async fn connect(config: ExecutionConfig) -> Result<Self> {
        info!(
            target: "pn_polymarket::execution",
            condition_id = %config.condition_id,
            clob_base_url = %config.clob_base_url,
            data_api_base_url = %config.data_api_base_url,
            chain_id = config.chain_id,
            "connecting authenticated Polymarket execution client"
        );
        let (signer, account, clob) =
            connect_authenticated_clob(&config.clob_base_url, config.chain_id).await?;

        let condition_id = B256::from_str(&config.condition_id)
            .with_context(|| format!("invalid condition id {}", config.condition_id))?;
        let market_response = clob.market(&config.condition_id).await?;
        let question = market_response.question.clone();
        let token_records = market_response.tokens;
        let mut tokens = Vec::with_capacity(token_records.len());
        let mut asset_ids = Vec::with_capacity(token_records.len());
        for token in token_records {
            let tick_size = clob
                .tick_size(token.token_id)
                .await?
                .minimum_tick_size
                .as_decimal();
            tokens.push(TokenMetadata {
                asset_id: token.token_id.to_string(),
                outcome: token.outcome,
                tick_size,
            });
            asset_ids.push(token.token_id);
        }

        let market = Arc::new(MarketMetadata {
            condition_id: config.condition_id.clone(),
            question,
            tokens,
        });

        let credentials = clob.credentials().clone();
        let address = clob.address();
        let market_ws = clob::ws::Client::default();
        let user_ws = clob::ws::Client::default().authenticate(credentials, address)?;
        let data = data::Client::new(&config.data_api_base_url)?;
        let contracts = contract_config(config.chain_id, market_response.neg_risk)
            .with_context(|| format!("missing contract config for chain {}", config.chain_id))?;

        info!(
            target: "pn_polymarket::execution",
            condition_id = %config.condition_id,
            question = %market.question,
            tokens = market.tokens.len(),
            signer = %account.signer,
            trading_address = %account.trading_address(),
            signature_type = ?account.signature_type(),
            "Polymarket execution client connected"
        );

        Ok(Self {
            config,
            signer,
            account,
            clob,
            market_ws,
            user_ws,
            data,
            condition_id,
            market,
            asset_ids: Arc::new(asset_ids),
            exchange_contract: contracts.exchange,
            collateral_token: contracts.collateral,
            conditional_tokens: contracts.conditional_tokens,
            neg_risk_adapter: contracts.neg_risk_adapter,
        })
    }

    pub async fn bootstrap(&self) -> Result<BootstrapState> {
        info!(
            target: "pn_polymarket::execution",
            condition_id = %self.market.condition_id,
            assets = self.asset_ids.len(),
            "bootstrapping market state"
        );
        let books = self.fetch_books().await?;
        let reconcile = self.reconcile().await?;

        info!(
            target: "pn_polymarket::execution",
            condition_id = %self.market.condition_id,
            books = books.len(),
            open_orders = reconcile.open_orders.len(),
            positions = reconcile.positions.len(),
            usdc_balance = %reconcile.account.usdc_balance,
            "bootstrap snapshot ready"
        );

        Ok(BootstrapState {
            market: (*self.market).clone(),
            books,
            open_orders: reconcile.open_orders,
            positions: reconcile.positions,
            account: reconcile.account,
        })
    }

    pub async fn fetch_reward_snapshot(
        &self,
        attempts: usize,
        backoff: Duration,
    ) -> Result<Option<RewardSnapshot>> {
        RewardClient::with_base_url(self.config.clob_base_url.clone())
            .fetch_market_snapshot(
                &self.market.condition_id,
                true,
                attempts,
                backoff,
                chrono::Utc::now().date_naive(),
            )
            .await
    }

    pub async fn check_approvals(&self, include_inventory: bool) -> Result<ApprovalStatus> {
        if !self.account.requires_onchain_approvals() {
            info!(
                target: "pn_polymarket::execution",
                trading_address = %self.account.trading_address(),
                signature_type = ?self.account.signature_type(),
                "skipping on-chain approval checks for proxy or safe trading account"
            );
            return Ok(ApprovalStatus {
                targets: Vec::new(),
            });
        }

        let rpc_url = rpc_url();
        info!(
            target: "pn_polymarket::execution",
            wallet = %self.account.trading_address(),
            rpc_url = %rpc_url,
            include_inventory,
            "checking on-chain approvals"
        );

        let provider = ProviderBuilder::new()
            .wallet(self.signer.clone())
            .connect(&rpc_url)
            .await?;
        let usdc = IERC20::new(self.collateral_token, provider.clone());
        let ctf = IERC1155::new(self.conditional_tokens, provider.clone());
        let mut checks = Vec::new();

        for target in self.approval_targets(include_inventory) {
            let usdc_allowance_ready = if target.require_usdc_allowance {
                allowance_is_ready(
                    check_allowance(&usdc, self.account.trading_address(), target.address).await?,
                )
            } else {
                true
            };
            let ctf_approval_ready = if target.require_ctf_approval {
                check_approval_for_all(&ctf, self.account.trading_address(), target.address).await?
            } else {
                true
            };

            info!(
                target: "pn_polymarket::execution",
                label = %target.label,
                address = %target.address,
                usdc_allowance_ready,
                ctf_approval_ready,
                "approval target checked"
            );

            checks.push(ApprovalCheck {
                label: target.label,
                address: target.address,
                require_usdc_allowance: target.require_usdc_allowance,
                require_ctf_approval: target.require_ctf_approval,
                usdc_allowance_ready,
                ctf_approval_ready,
            });
        }

        Ok(ApprovalStatus { targets: checks })
    }

    pub async fn ensure_approvals(&self, include_inventory: bool) -> Result<ApprovalStatus> {
        if !self.account.requires_onchain_approvals() {
            return self.check_approvals(include_inventory).await;
        }

        let status = self.check_approvals(include_inventory).await?;
        if status.is_ready() {
            return Ok(status);
        }

        warn!(
            target: "pn_polymarket::execution",
            missing = ?status.missing_permissions(),
            "missing approvals detected; submitting chain transactions"
        );

        let provider = ProviderBuilder::new()
            .wallet(self.signer.clone())
            .connect(&rpc_url())
            .await?;
        let usdc = IERC20::new(self.collateral_token, provider.clone());
        let ctf = IERC1155::new(self.conditional_tokens, provider.clone());

        for target in &status.targets {
            if target.require_usdc_allowance && !target.usdc_allowance_ready {
                let tx_hash = approve_usdc(&usdc, target.address, U256::MAX).await?;
                info!(
                    target: "pn_polymarket::execution",
                    label = %target.label,
                    address = %target.address,
                    tx_hash = %tx_hash,
                    "USDC approval submitted"
                );
            }

            if target.require_ctf_approval && !target.ctf_approval_ready {
                let tx_hash = set_ctf_approval(&ctf, target.address, true).await?;
                info!(
                    target: "pn_polymarket::execution",
                    label = %target.label,
                    address = %target.address,
                    tx_hash = %tx_hash,
                    "CTF approval submitted"
                );
            }
        }

        let verified = self.check_approvals(include_inventory).await?;
        if !verified.is_ready() {
            anyhow::bail!(
                "approval verification failed: {}",
                verified.missing_permissions().join(", ")
            );
        }

        Ok(verified)
    }

    pub fn start_streams(
        &self,
        event_tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: CancellationToken,
    ) {
        self.start_market_stream(event_tx.clone(), cancel.clone());
        self.start_tick_size_stream(event_tx.clone(), cancel.clone());
        self.start_order_stream(event_tx.clone(), cancel.clone());
        self.start_trade_stream(event_tx, cancel);
    }

    pub async fn reconcile(&self) -> Result<ReconciliationState> {
        let books = self.fetch_books().await?;
        let open_orders = self.fetch_open_orders().await?;
        let positions = self.fetch_positions().await?;
        let account = self.fetch_account_snapshot().await?;

        info!(
            target: "pn_polymarket::execution",
            condition_id = %self.market.condition_id,
            books = books.len(),
            open_orders = open_orders.len(),
            positions = positions.len(),
            usdc_balance = %account.usdc_balance,
            "reconciled exchange state"
        );

        Ok(ReconciliationState {
            books,
            open_orders,
            positions,
            account,
        })
    }

    pub async fn post_quotes(&self, quotes: &[QuoteRequest]) -> Result<Vec<ManagedOrder>> {
        if quotes.is_empty() {
            return Ok(Vec::new());
        }

        info!(
            target: "pn_polymarket::execution",
            condition_id = %self.market.condition_id,
            quote_count = quotes.len(),
            "posting passive quotes"
        );

        let mut signed_orders = Vec::with_capacity(quotes.len());
        for quote in quotes {
            let token_id = parse_u256(&quote.asset_id)?;
            let signable = self
                .clob
                .limit_order()
                .token_id(token_id)
                .side(side_to_sdk(&quote.side))
                .price(quote.price)
                .size(quote.size)
                .order_type(OrderType::GTC)
                .post_only(true)
                .build()
                .await?;
            signed_orders.push(self.clob.sign(&self.signer, signable).await?);
        }

        let responses = match self.clob.post_orders(signed_orders).await {
            Ok(responses) => responses,
            Err(error) => {
                error!(
                    target: "pn_polymarket::execution",
                    condition_id = %self.market.condition_id,
                    quote_count = quotes.len(),
                    error = %error,
                    "passive quote request failed"
                );
                return Err(error.into());
            }
        };

        let posted = responses
            .into_iter()
            .zip(quotes.iter())
            .filter_map(|(response, quote)| {
                if !response.success {
                    warn!(
                        target: "pn_polymarket::execution",
                        order_id = %response.order_id,
                        status = %response.status,
                        asset_id = %quote.asset_id,
                        side = %quote.side,
                        price = %quote.price,
                        size = %quote.size,
                        "CLOB rejected quote; not tracking rejected order"
                    );
                    return None;
                }

                Some(ManagedOrder {
                    order_id: response.order_id,
                    asset_id: quote.asset_id.clone(),
                    side: quote.side.clone(),
                    price: quote.price,
                    size: quote.size,
                    status: response.status.to_string(),
                })
            })
            .collect::<Vec<_>>();

        for order in &posted {
            info!(
                target: "pn_polymarket::execution",
                order_id = %order.order_id,
                asset_id = %order.asset_id,
                side = %order.side,
                price = %order.price,
                size = %order.size,
                status = %order.status,
                "passive quote accepted by CLOB"
            );
        }

        Ok(posted)
    }

    pub async fn cancel_orders(&self, order_ids: &[String]) -> Result<()> {
        if order_ids.is_empty() {
            return Ok(());
        }
        info!(
            target: "pn_polymarket::execution",
            order_count = order_ids.len(),
            "canceling explicit order set"
        );
        let refs: Vec<&str> = order_ids.iter().map(String::as_str).collect();
        self.clob.cancel_orders(&refs).await?;
        Ok(())
    }

    pub async fn cancel_market_orders(&self, asset_id: Option<&str>) -> Result<()> {
        info!(
            target: "pn_polymarket::execution",
            condition_id = %self.market.condition_id,
            asset_id = asset_id.unwrap_or("all"),
            "canceling market-scoped orders"
        );
        let request = match optional_u256(asset_id)? {
            Some(asset_id) => CancelMarketOrderRequest::builder()
                .market(self.condition_id)
                .asset_id(asset_id)
                .build(),
            None => CancelMarketOrderRequest::builder()
                .market(self.condition_id)
                .build(),
        };
        self.clob.cancel_market_orders(&request).await?;
        Ok(())
    }

    pub async fn cancel_all(&self) -> Result<()> {
        info!(
            target: "pn_polymarket::execution",
            condition_id = %self.market.condition_id,
            "canceling all open orders"
        );
        self.clob.cancel_all_orders().await?;
        Ok(())
    }

    pub async fn flatten(&self, request: &FlattenRequest) -> Result<TradeFill> {
        warn!(
            target: "pn_polymarket::execution",
            asset_id = %request.asset_id,
            side = %request.side,
            size = %request.size,
            use_fok = request.use_fok,
            "submitting flatten market order"
        );
        let token_id = parse_u256(&request.asset_id)?;
        let signable = self
            .clob
            .market_order()
            .token_id(token_id)
            .side(side_to_sdk(&request.side))
            .amount(Amount::shares(request.size)?)
            .order_type(if request.use_fok {
                OrderType::FOK
            } else {
                OrderType::FAK
            })
            .build()
            .await?;
        let signed = self.clob.sign(&self.signer, signable).await?;
        let response = self.clob.post_order(signed).await?;

        info!(
            target: "pn_polymarket::execution",
            order_id = %response.order_id,
            trade_ids = ?response.trade_ids,
            status = %response.status,
            asset_id = %request.asset_id,
            side = %request.side,
            size = %request.size,
            "flatten order accepted by CLOB"
        );

        Ok(flatten_trade_fill(request, response))
    }

    pub async fn post_heartbeat(&self, heartbeat_id: Option<&str>) -> Result<String> {
        let parsed = match heartbeat_id {
            Some(id) if !id.is_empty() => Some(Uuid::parse_str(id)?),
            _ => None,
        };
        let response = match self.clob.post_heartbeat(parsed).await {
            Ok(response) => response,
            Err(error) => {
                if let Some(recovery_id) = heartbeat_recovery_id_from_error(&error.to_string()) {
                    warn!(
                        target: "pn_polymarket::execution",
                        heartbeat_id = %recovery_id,
                        "heartbeat rejected; retrying with server-provided heartbeat id"
                    );
                    self.clob.post_heartbeat(Some(recovery_id)).await?
                } else {
                    return Err(error.into());
                }
            }
        };
        info!(
            target: "pn_polymarket::execution",
            heartbeat_id = %response.heartbeat_id,
            "heartbeat posted to CLOB"
        );
        Ok(response.heartbeat_id.to_string())
    }

    pub async fn split_position(&self, amount: Decimal) -> Result<String> {
        info!(target: "pn_polymarket::execution", amount = %amount, "submitting split transaction");
        let rpc_url = env::var("POLYMARKET_RPC_URL")
            .context("POLYMARKET_RPC_URL is required for split operations")?;
        let provider = ProviderBuilder::new()
            .wallet(self.signer.clone())
            .connect(&rpc_url)
            .await?;
        let client = ctf::Client::new(provider, self.config.chain_id)?;
        let request = ctf::types::SplitPositionRequest::for_binary_market(
            self.collateral_token,
            self.condition_id,
            decimal_to_u256(amount, 6)?,
        );
        let response = client.split_position(&request).await?;
        info!(target: "pn_polymarket::execution", amount = %amount, tx_hash = %response.transaction_hash, "split transaction submitted");
        Ok(response.transaction_hash.to_string())
    }

    pub async fn merge_positions(&self, amount: Decimal) -> Result<String> {
        info!(target: "pn_polymarket::execution", amount = %amount, "submitting merge transaction");
        let rpc_url = env::var("POLYMARKET_RPC_URL")
            .context("POLYMARKET_RPC_URL is required for merge operations")?;
        let provider = ProviderBuilder::new()
            .wallet(self.signer.clone())
            .connect(&rpc_url)
            .await?;
        let client = ctf::Client::new(provider, self.config.chain_id)?;
        let request = ctf::types::MergePositionsRequest::for_binary_market(
            self.collateral_token,
            self.condition_id,
            decimal_to_u256(amount, 6)?,
        );
        let response = client.merge_positions(&request).await?;
        info!(target: "pn_polymarket::execution", amount = %amount, tx_hash = %response.transaction_hash, "merge transaction submitted");
        Ok(response.transaction_hash.to_string())
    }

    pub fn market(&self) -> &MarketMetadata {
        &self.market
    }

    async fn fetch_open_orders(&self) -> Result<Vec<ManagedOrder>> {
        let request = open_orders_request();
        let mut orders = Vec::new();
        let mut cursor = next_open_orders_cursor(None);

        while let Some(next_cursor) = cursor.clone() {
            let page = self.clob.orders(&request, Some(next_cursor)).await?;
            orders.extend(
                page.data
                    .into_iter()
                    .filter(|order| order_belongs_to_market(order, self.condition_id))
                    .map(map_open_order)
                    .collect::<Result<Vec<_>>>()?,
            );

            cursor = next_open_orders_cursor(Some(page.next_cursor));
        }

        Ok(orders)
    }

    async fn fetch_books(&self) -> Result<Vec<BookSnapshot>> {
        let mut requests = Vec::with_capacity(self.asset_ids.len());
        for asset_id in self.asset_ids.iter() {
            requests.push(
                OrderBookSummaryRequest::builder()
                    .token_id(*asset_id)
                    .build(),
            );
        }

        Ok(self
            .clob
            .order_books(&requests)
            .await?
            .into_iter()
            .map(|book| BookSnapshot {
                asset_id: book.asset_id.to_string(),
                bids: book
                    .bids
                    .into_iter()
                    .map(|level| BookLevel {
                        price: level.price,
                        size: level.size,
                    })
                    .collect(),
                asks: book
                    .asks
                    .into_iter()
                    .map(|level| BookLevel {
                        price: level.price,
                        size: level.size,
                    })
                    .collect(),
            })
            .collect())
    }

    async fn fetch_positions(&self) -> Result<Vec<PositionSnapshot>> {
        let request = PositionsRequest::builder()
            .user(self.account.trading_address())
            .filter(MarketFilter::markets([self.condition_id]))
            .build();
        let positions = self.data.positions(&request).await?;
        Ok(positions
            .into_iter()
            .map(|position| PositionSnapshot {
                asset_id: position.asset.to_string(),
                size: position.size,
                avg_price: position.avg_price,
            })
            .collect())
    }

    async fn fetch_account_snapshot(&self) -> Result<AccountSnapshot> {
        let usdc = self
            .clob
            .balance_allowance(
                BalanceAllowanceRequest::builder()
                    .asset_type(AssetType::Collateral)
                    .build(),
            )
            .await?;
        let clob = &self.clob;
        let token_balances =
            try_join_all(self.asset_ids.iter().copied().map(|asset_id| async move {
                let response = clob
                    .balance_allowance(
                        BalanceAllowanceRequest::builder()
                            .asset_type(AssetType::Conditional)
                            .token_id(asset_id)
                            .build(),
                    )
                    .await?;
                Ok::<_, anyhow::Error>((asset_id.to_string(), response.balance))
            }))
            .await?
            .into_iter()
            .collect::<HashMap<_, _>>();

        Ok(AccountSnapshot {
            usdc_balance: usdc.balance,
            token_balances,
        })
    }

    fn approval_targets(&self, include_inventory: bool) -> Vec<ApprovalTarget> {
        build_approval_targets(
            self.exchange_contract,
            self.conditional_tokens,
            self.neg_risk_adapter,
            include_inventory,
        )
    }

    fn start_market_stream(
        &self,
        event_tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: CancellationToken,
    ) {
        let client = self.market_ws.clone();
        let asset_ids = (*self.asset_ids).clone();
        tokio::spawn(async move {
            let mut backoff_secs = 1u64;
            loop {
                if cancel.is_cancelled() {
                    break;
                }

                let stream = match client.subscribe_orderbook(asset_ids.clone()) {
                    Ok(stream) => stream,
                    Err(error) => {
                        warn!(?error, "failed to subscribe to market orderbook stream");
                        sleep(Duration::from_secs(backoff_secs)).await;
                        backoff_secs = next_reconnect_backoff_secs(backoff_secs);
                        continue;
                    }
                };
                tokio::pin!(stream);

                let mut disconnected = false;
                let mut received_message = false;
                while !cancel.is_cancelled() {
                    tokio::select! {
                        () = cancel.cancelled() => break,
                        message = stream.next() => {
                            match message {
                                Some(Ok(book)) => {
                                    mark_stream_message_received(&mut received_message, &mut backoff_secs);
                                    let event = StreamEvent::Book(BookSnapshot {
                                        asset_id: book.asset_id.to_string(),
                                        bids: book.bids.into_iter().map(|level| BookLevel {
                                            price: level.price,
                                            size: level.size,
                                        }).collect(),
                                        asks: book.asks.into_iter().map(|level| BookLevel {
                                            price: level.price,
                                            size: level.size,
                                        }).collect(),
                                    });
                                    if event_tx.send(event).is_err() {
                                        return;
                                    }
                                }
                                Some(Err(error)) => {
                                    warn!(?error, "market orderbook stream disconnected");
                                    disconnected = true;
                                    break;
                                }
                                None => {
                                    disconnected = true;
                                    break;
                                }
                            }
                        }
                    }
                }

                if !disconnected || cancel.is_cancelled() {
                    break;
                }

                sleep(Duration::from_secs(backoff_secs)).await;
                backoff_secs = next_reconnect_backoff_secs(backoff_secs);
            }
        });
    }

    fn start_tick_size_stream(
        &self,
        event_tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: CancellationToken,
    ) {
        let client = self.market_ws.clone();
        let asset_ids = (*self.asset_ids).clone();
        tokio::spawn(async move {
            let mut backoff_secs = 1u64;
            loop {
                if cancel.is_cancelled() {
                    break;
                }

                let stream = match client.subscribe_tick_size_change(asset_ids.clone()) {
                    Ok(stream) => stream,
                    Err(error) => {
                        warn!(?error, "failed to subscribe to tick size stream");
                        sleep(Duration::from_secs(backoff_secs)).await;
                        backoff_secs = next_reconnect_backoff_secs(backoff_secs);
                        continue;
                    }
                };
                tokio::pin!(stream);

                let mut disconnected = false;
                let mut received_message = false;
                while !cancel.is_cancelled() {
                    tokio::select! {
                        () = cancel.cancelled() => break,
                        message = stream.next() => {
                            match message {
                                Some(Ok(change)) => {
                                    mark_stream_message_received(&mut received_message, &mut backoff_secs);
                                    if event_tx.send(StreamEvent::TickSize {
                                        asset_id: change.asset_id.to_string(),
                                        new_tick_size: change.new_tick_size,
                                    }).is_err() {
                                        return;
                                    }
                                }
                                Some(Err(error)) => {
                                    warn!(?error, "tick size stream disconnected");
                                    disconnected = true;
                                    break;
                                }
                                None => {
                                    disconnected = true;
                                    break;
                                }
                            }
                        }
                    }
                }

                if !disconnected || cancel.is_cancelled() {
                    break;
                }

                sleep(Duration::from_secs(backoff_secs)).await;
                backoff_secs = next_reconnect_backoff_secs(backoff_secs);
            }
        });
    }

    fn start_order_stream(
        &self,
        event_tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: CancellationToken,
    ) {
        let client = self.user_ws.clone();
        let markets = vec![self.condition_id];
        tokio::spawn(async move {
            let mut backoff_secs = 1u64;
            loop {
                if cancel.is_cancelled() {
                    break;
                }

                let stream = match client.subscribe_orders(markets.clone()) {
                    Ok(stream) => stream,
                    Err(error) => {
                        warn!(?error, "failed to subscribe to user order stream");
                        sleep(Duration::from_secs(backoff_secs)).await;
                        backoff_secs = next_reconnect_backoff_secs(backoff_secs);
                        continue;
                    }
                };
                tokio::pin!(stream);

                let mut disconnected = false;
                let mut received_message = false;
                while !cancel.is_cancelled() {
                    tokio::select! {
                        () = cancel.cancelled() => break,
                        message = stream.next() => {
                            match message {
                                Some(Ok(order)) => {
                                    mark_stream_message_received(&mut received_message, &mut backoff_secs);
                                    let side = match side_from_sdk(order.side) {
                                        Ok(side) => side,
                                        Err(error) => {
                                            warn!(?error, "dropping user order update with unsupported side");
                                            continue;
                                        }
                                    };
                                    let event = StreamEvent::Order(ManagedOrder {
                                        order_id: order.id,
                                        asset_id: order.asset_id.to_string(),
                                        side,
                                        price: order.price,
                                        size: remaining_order_size(
                                            order.original_size.unwrap_or(Decimal::ZERO),
                                            order.size_matched.unwrap_or(Decimal::ZERO),
                                        ),
                                        status: order.status.map(|status| status.to_string()).unwrap_or_else(|| "UNKNOWN".to_string()),
                                    });
                                    if event_tx.send(event).is_err() {
                                        return;
                                    }
                                }
                                Some(Err(error)) => {
                                    warn!(?error, "user order stream disconnected");
                                    disconnected = true;
                                    break;
                                }
                                None => {
                                    disconnected = true;
                                    break;
                                }
                            }
                        }
                    }
                }

                if !disconnected || cancel.is_cancelled() {
                    break;
                }

                sleep(Duration::from_secs(backoff_secs)).await;
                backoff_secs = next_reconnect_backoff_secs(backoff_secs);
            }
        });
    }

    fn start_trade_stream(
        &self,
        event_tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: CancellationToken,
    ) {
        let client = self.user_ws.clone();
        let markets = vec![self.condition_id];
        tokio::spawn(async move {
            let mut backoff_secs = 1u64;
            loop {
                if cancel.is_cancelled() {
                    break;
                }

                let stream = match client.subscribe_trades(markets.clone()) {
                    Ok(stream) => stream,
                    Err(error) => {
                        warn!(?error, "failed to subscribe to user trade stream");
                        sleep(Duration::from_secs(backoff_secs)).await;
                        backoff_secs = next_reconnect_backoff_secs(backoff_secs);
                        continue;
                    }
                };
                tokio::pin!(stream);

                let mut disconnected = false;
                let mut received_message = false;
                while !cancel.is_cancelled() {
                    tokio::select! {
                        () = cancel.cancelled() => break,
                        message = stream.next() => {
                            match message {
                                Some(Ok(trade)) => {
                                    mark_stream_message_received(&mut received_message, &mut backoff_secs);
                                    let side = match side_from_sdk(trade.side) {
                                        Ok(side) => side,
                                        Err(error) => {
                                            warn!(?error, "dropping user trade update with unsupported side");
                                            continue;
                                        }
                                    };
                                    let order_id = trade_order_id(&trade);
                                    let status = trade_status_label(&trade.status);
                                    let event = StreamEvent::Trade(TradeFill {
                                        trade_id: trade.id,
                                        order_id,
                                        asset_id: trade.asset_id.to_string(),
                                        side,
                                        price: trade.price,
                                        size: trade.size,
                                        status,
                                    });
                                    if event_tx.send(event).is_err() {
                                        return;
                                    }
                                }
                                Some(Err(error)) => {
                                    warn!(?error, "user trade stream disconnected");
                                    disconnected = true;
                                    break;
                                }
                                None => {
                                    disconnected = true;
                                    break;
                                }
                            }
                        }
                    }
                }

                if !disconnected || cancel.is_cancelled() {
                    break;
                }

                sleep(Duration::from_secs(backoff_secs)).await;
                backoff_secs = next_reconnect_backoff_secs(backoff_secs);
            }
        });
    }
}

fn map_open_order(
    order: polymarket_client_sdk::clob::types::response::OpenOrderResponse,
) -> Result<ManagedOrder> {
    Ok(ManagedOrder {
        order_id: order.id,
        asset_id: order.asset_id.to_string(),
        side: side_from_sdk(order.side)?,
        price: order.price,
        size: remaining_order_size(order.original_size, order.size_matched),
        status: order.status.to_string(),
    })
}

fn open_orders_request() -> OrdersRequest {
    OrdersRequest::builder().build()
}

fn next_open_orders_cursor(cursor: Option<String>) -> Option<String> {
    match cursor {
        None => Some(INITIAL_OPEN_ORDERS_CURSOR.to_string()),
        Some(cursor) if should_continue_open_orders_pagination(&cursor) => Some(cursor),
        Some(_) => None,
    }
}

fn should_continue_open_orders_pagination(cursor: &str) -> bool {
    !cursor.is_empty() && cursor != TERMINAL_OPEN_ORDERS_CURSOR
}

fn heartbeat_recovery_id_from_error(message: &str) -> Option<Uuid> {
    if !message.contains("Invalid Heartbeat ID") {
        return None;
    }

    let payload_start = message.rfind('{')?;
    let payload: serde_json::Value = serde_json::from_str(&message[payload_start..]).ok()?;
    let heartbeat_id = payload.get("heartbeat_id")?.as_str()?;
    Uuid::parse_str(heartbeat_id).ok()
}

fn order_belongs_to_market(
    order: &polymarket_client_sdk::clob::types::response::OpenOrderResponse,
    condition_id: B256,
) -> bool {
    order.market == condition_id
}

fn side_to_sdk(side: &QuoteSide) -> Side {
    match side {
        QuoteSide::Buy => Side::Buy,
        QuoteSide::Sell => Side::Sell,
    }
}

fn side_from_sdk(side: Side) -> Result<QuoteSide> {
    match side {
        Side::Buy => Ok(QuoteSide::Buy),
        Side::Sell => Ok(QuoteSide::Sell),
        other => anyhow::bail!("unexpected SDK side variant: {other:?}"),
    }
}

fn next_reconnect_backoff_secs(backoff_secs: u64) -> u64 {
    (backoff_secs * 2).min(60)
}

fn flatten_trade_fill(
    request: &FlattenRequest,
    response: polymarket_client_sdk::clob::types::response::PostOrderResponse,
) -> TradeFill {
    let matched = response.success && response.status == OrderStatusType::Matched;
    TradeFill {
        trade_id: response
            .trade_ids
            .first()
            .cloned()
            .unwrap_or_else(|| response.order_id.clone()),
        order_id: Some(response.order_id),
        asset_id: request.asset_id.clone(),
        side: request.side.clone(),
        price: if matched {
            request.price
        } else {
            Decimal::ZERO
        },
        size: if matched { request.size } else { Decimal::ZERO },
        status: format!("{}:unconfirmed", response.status),
    }
}

fn allowance_is_ready(allowance: U256) -> bool {
    allowance > U256::MAX / U256::from(2u8)
}

fn resolve_trading_account(
    signer: Address,
    chain_id: u64,
    funder_address: Option<&str>,
    signature_type: Option<&str>,
) -> Result<TradingAccount> {
    let funder = funder_address.map(parse_trading_address).transpose()?;
    let kind = signature_type.map(parse_trading_account_kind).transpose()?;

    match (funder, kind) {
        (None, None) | (None, Some(TradingAccountKind::Eoa)) => Ok(TradingAccount {
            signer,
            funder: None,
            kind: TradingAccountKind::Eoa,
        }),
        (Some(_), Some(TradingAccountKind::Eoa)) => {
            anyhow::bail!("{FUNDER_ADDRESS_VAR} cannot be combined with {SIGNATURE_TYPE_VAR}=EOA")
        }
        (Some(funder), None) => Ok(TradingAccount {
            signer,
            funder: Some(funder),
            kind: infer_trading_account_kind(signer, chain_id, funder)?,
        }),
        (None, Some(kind)) => Ok(TradingAccount {
            signer,
            funder: derive_trading_funder(signer, chain_id, kind)?,
            kind,
        }),
        (Some(funder), Some(kind)) => {
            let expected = derive_trading_funder(signer, chain_id, kind)?.with_context(|| {
                format!(
                    "{SIGNATURE_TYPE_VAR}={signature} requires a derived trading address on chain {chain_id}",
                    signature = signature_type_name(kind)
                )
            })?;
            if funder != expected {
                anyhow::bail!(
                    "{FUNDER_ADDRESS_VAR}={funder} does not match derived {kind} address {expected} for signer {signer}",
                    kind = signature_type_name(kind),
                );
            }
            Ok(TradingAccount {
                signer,
                funder: Some(funder),
                kind,
            })
        }
    }
}

fn parse_trading_address(value: &str) -> Result<Address> {
    Address::from_str(value).with_context(|| format!("invalid {FUNDER_ADDRESS_VAR} value: {value}"))
}

fn parse_trading_account_kind(value: &str) -> Result<TradingAccountKind> {
    let normalized = value.trim().to_ascii_uppercase();
    match normalized.as_str() {
        "0" | "EOA" => Ok(TradingAccountKind::Eoa),
        "1" | "PROXY" | "POLY_PROXY" => Ok(TradingAccountKind::Proxy),
        "2" | "SAFE" | "GNOSIS_SAFE" | "GNOSISSAFE" | "GNOSIS-SAFE" => {
            Ok(TradingAccountKind::GnosisSafe)
        }
        _ => anyhow::bail!(
            "unsupported {SIGNATURE_TYPE_VAR} value: {value}. Expected one of EOA/0, POLY_PROXY/PROXY/1, or GNOSIS_SAFE/SAFE/2"
        ),
    }
}

fn infer_trading_account_kind(
    signer: Address,
    chain_id: u64,
    funder: Address,
) -> Result<TradingAccountKind> {
    let proxy = derive_trading_funder(signer, chain_id, TradingAccountKind::Proxy)?;
    let safe = derive_trading_funder(signer, chain_id, TradingAccountKind::GnosisSafe)?;

    match (proxy == Some(funder), safe == Some(funder)) {
        (true, false) => Ok(TradingAccountKind::Proxy),
        (false, true) => Ok(TradingAccountKind::GnosisSafe),
        (false, false) => anyhow::bail!(
            "{FUNDER_ADDRESS_VAR}={funder} does not match derived proxy or safe address for signer {signer} on chain {chain_id}"
        ),
        (true, true) => anyhow::bail!(
            "{FUNDER_ADDRESS_VAR}={funder} is ambiguous because it matches both proxy and safe derivations for signer {signer}"
        ),
    }
}

fn derive_trading_funder(
    signer: Address,
    chain_id: u64,
    kind: TradingAccountKind,
) -> Result<Option<Address>> {
    match kind {
        TradingAccountKind::Eoa => Ok(None),
        TradingAccountKind::Proxy => Ok(derive_proxy_wallet(signer, chain_id)),
        TradingAccountKind::GnosisSafe => Ok(derive_safe_wallet(signer, chain_id)),
    }
}

fn signature_type_name(kind: TradingAccountKind) -> &'static str {
    match kind {
        TradingAccountKind::Eoa => "EOA",
        TradingAccountKind::Proxy => "POLY_PROXY",
        TradingAccountKind::GnosisSafe => "GNOSIS_SAFE",
    }
}

fn remaining_order_size(original_size: Decimal, size_matched: Decimal) -> Decimal {
    let remaining = original_size - size_matched;
    if remaining.is_sign_negative() {
        Decimal::ZERO
    } else {
        remaining
    }
}

fn mark_stream_message_received(received_message: &mut bool, backoff_secs: &mut u64) {
    if !*received_message {
        *received_message = true;
        *backoff_secs = 1;
    }
}

fn trade_order_id(
    trade: &polymarket_client_sdk::clob::ws::types::response::TradeMessage,
) -> Option<String> {
    select_trade_order_id(
        trade.trader_side.clone(),
        trade.taker_order_id.clone(),
        trade
            .maker_orders
            .first()
            .map(|maker_order| maker_order.order_id.clone()),
    )
}

fn select_trade_order_id(
    trader_side: Option<TraderSide>,
    taker_order_id: Option<String>,
    maker_order_id: Option<String>,
) -> Option<String> {
    match trader_side {
        Some(TraderSide::Maker) => maker_order_id.or(taker_order_id),
        Some(TraderSide::Taker) => taker_order_id.or(maker_order_id),
        _ => taker_order_id.or(maker_order_id),
    }
}

fn trade_status_label(
    status: &polymarket_client_sdk::clob::ws::types::response::TradeMessageStatus,
) -> String {
    match status {
        polymarket_client_sdk::clob::ws::types::response::TradeMessageStatus::Matched => {
            "MATCHED".to_string()
        }
        polymarket_client_sdk::clob::ws::types::response::TradeMessageStatus::Mined => {
            "MINED".to_string()
        }
        polymarket_client_sdk::clob::ws::types::response::TradeMessageStatus::Confirmed => {
            "CONFIRMED".to_string()
        }
        polymarket_client_sdk::clob::ws::types::response::TradeMessageStatus::Unknown(value) => {
            value.to_uppercase()
        }
        _ => "UNKNOWN".to_string(),
    }
}

fn parse_u256(value: &str) -> Result<U256> {
    U256::from_str(value).with_context(|| format!("invalid asset id {value}"))
}

fn optional_u256(value: Option<&str>) -> Result<Option<U256>> {
    value.map(parse_u256).transpose()
}

fn build_approval_targets(
    exchange: Address,
    conditional_tokens: Address,
    neg_risk_adapter: Option<Address>,
    include_inventory: bool,
) -> Vec<ApprovalTarget> {
    let mut targets = vec![ApprovalTarget {
        label: "market exchange".to_string(),
        address: exchange,
        require_usdc_allowance: true,
        require_ctf_approval: true,
    }];

    if let Some(adapter) = neg_risk_adapter {
        targets.push(ApprovalTarget {
            label: "neg-risk adapter".to_string(),
            address: adapter,
            require_usdc_allowance: true,
            require_ctf_approval: true,
        });
    }

    if include_inventory {
        targets.push(ApprovalTarget {
            label: "conditional tokens".to_string(),
            address: conditional_tokens,
            require_usdc_allowance: true,
            require_ctf_approval: false,
        });
    }

    targets
}

fn decimal_to_u256(value: Decimal, scale: u32) -> Result<U256> {
    let scaled = (value * Decimal::from(10u64.pow(scale))).trunc();
    let integer = scaled.to_string();
    U256::from_str(&integer).with_context(|| format!("unable to convert {value} into base units"))
}

impl std::fmt::Display for QuoteSide {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Buy => f.write_str("BUY"),
            Self::Sell => f.write_str("SELL"),
        }
    }
}

impl std::fmt::Display for TradeFill {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {} {} @ {}",
            self.side, self.size, self.asset_id, self.price
        )
    }
}

impl ApprovalCheck {
    fn missing_permissions(&self) -> Vec<String> {
        let mut missing = Vec::new();
        if self.require_usdc_allowance && !self.usdc_allowance_ready {
            missing.push(format!("{}: USDC allowance", self.label));
        }
        if self.require_ctf_approval && !self.ctf_approval_ready {
            missing.push(format!("{}: CTF approval", self.label));
        }
        missing
    }
}

impl ApprovalStatus {
    pub fn is_ready(&self) -> bool {
        self.targets
            .iter()
            .all(|target| target.missing_permissions().is_empty())
    }

    pub fn missing_permissions(&self) -> Vec<String> {
        self.targets
            .iter()
            .flat_map(ApprovalCheck::missing_permissions)
            .collect()
    }
}

fn rpc_url() -> String {
    env::var("POLYMARKET_RPC_URL").unwrap_or_else(|_| DEFAULT_POLYGON_RPC_URL.to_string())
}

async fn check_allowance<P: alloy::providers::Provider>(
    usdc: &IERC20::IERC20Instance<P>,
    owner: Address,
    spender: Address,
) -> Result<U256> {
    Ok(usdc.allowance(owner, spender).call().await?)
}

async fn check_approval_for_all<P: alloy::providers::Provider>(
    ctf: &IERC1155::IERC1155Instance<P>,
    owner: Address,
    operator: Address,
) -> Result<bool> {
    Ok(ctf.isApprovedForAll(owner, operator).call().await?)
}

async fn approve_usdc<P: alloy::providers::Provider>(
    usdc: &IERC20::IERC20Instance<P>,
    spender: Address,
    amount: U256,
) -> Result<String> {
    Ok(usdc
        .approve(spender, amount)
        .send()
        .await?
        .watch()
        .await?
        .to_string())
}

async fn set_ctf_approval<P: alloy::providers::Provider>(
    ctf: &IERC1155::IERC1155Instance<P>,
    operator: Address,
    approved: bool,
) -> Result<String> {
    Ok(ctf
        .setApprovalForAll(operator, approved)
        .send()
        .await?
        .watch()
        .await?
        .to_string())
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use polymarket_client_sdk::auth::Uuid;
    use polymarket_client_sdk::clob::types::response::{OpenOrderResponse, PostOrderResponse};
    use polymarket_client_sdk::clob::types::{OrderStatusType, Side, TraderSide};
    use polymarket_client_sdk::types::{address, B256, U256};
    use polymarket_client_sdk::ToQueryParams as _;
    use polymarket_client_sdk::{derive_proxy_wallet, derive_safe_wallet, POLYGON};
    use rust_decimal_macros::dec;

    use super::{
        allowance_is_ready, build_approval_targets, flatten_trade_fill,
        heartbeat_recovery_id_from_error, map_open_order, mark_stream_message_received,
        next_open_orders_cursor, next_reconnect_backoff_secs, open_orders_request,
        order_belongs_to_market, resolve_trading_account, select_trade_order_id,
        should_continue_open_orders_pagination, side_from_sdk, ApprovalCheck, ApprovalStatus,
        FlattenRequest, QuoteSide, TradingAccountKind,
    };

    #[test]
    fn build_approval_targets_for_standard_market_only_requires_exchange() {
        let exchange = address!("0x1000000000000000000000000000000000000001");
        let conditional_tokens = address!("0x2000000000000000000000000000000000000002");

        let targets = build_approval_targets(exchange, conditional_tokens, None, false);

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].label, "market exchange");
        assert_eq!(targets[0].address, exchange);
        assert!(targets[0].require_usdc_allowance);
        assert!(targets[0].require_ctf_approval);
    }

    #[test]
    fn build_approval_targets_adds_adapter_and_split_contract_when_needed() {
        let exchange = address!("0x1000000000000000000000000000000000000001");
        let conditional_tokens = address!("0x2000000000000000000000000000000000000002");
        let adapter = address!("0x3000000000000000000000000000000000000003");

        let targets = build_approval_targets(exchange, conditional_tokens, Some(adapter), true);

        assert_eq!(targets.len(), 3);
        assert_eq!(targets[1].label, "neg-risk adapter");
        assert_eq!(targets[1].address, adapter);
        assert!(targets[1].require_usdc_allowance);
        assert!(targets[1].require_ctf_approval);

        assert_eq!(targets[2].label, "conditional tokens");
        assert_eq!(targets[2].address, conditional_tokens);
        assert!(targets[2].require_usdc_allowance);
        assert!(!targets[2].require_ctf_approval);
    }

    #[test]
    fn approval_status_reports_only_missing_permissions() {
        let exchange = address!("0x1000000000000000000000000000000000000001");
        let adapter = address!("0x3000000000000000000000000000000000000003");
        let status = ApprovalStatus {
            targets: vec![
                ApprovalCheck {
                    label: "market exchange".to_string(),
                    address: exchange,
                    require_usdc_allowance: true,
                    require_ctf_approval: true,
                    usdc_allowance_ready: false,
                    ctf_approval_ready: true,
                },
                ApprovalCheck {
                    label: "neg-risk adapter".to_string(),
                    address: adapter,
                    require_usdc_allowance: true,
                    require_ctf_approval: true,
                    usdc_allowance_ready: true,
                    ctf_approval_ready: false,
                },
            ],
        };

        assert!(!status.is_ready());
        assert_eq!(
            status.missing_permissions(),
            vec![
                "market exchange: USDC allowance".to_string(),
                "neg-risk adapter: CTF approval".to_string(),
            ]
        );
    }

    #[test]
    fn reconnect_backoff_doubles_and_caps_at_sixty_seconds() {
        assert_eq!(next_reconnect_backoff_secs(1), 2);
        assert_eq!(next_reconnect_backoff_secs(8), 16);
        assert_eq!(next_reconnect_backoff_secs(60), 60);
    }

    #[test]
    fn flatten_trade_fill_uses_requested_price_and_marks_ack_as_unconfirmed() {
        let request = FlattenRequest {
            asset_id: "123".to_string(),
            side: QuoteSide::Sell,
            price: dec!(0.41),
            size: dec!(17),
            use_fok: false,
        };
        let response = PostOrderResponse::builder()
            .making_amount(dec!(0))
            .taking_amount(dec!(0))
            .order_id("order-1")
            .status(OrderStatusType::Matched)
            .success(true)
            .trade_ids(vec!["trade-1".to_string()])
            .build();

        let fill = flatten_trade_fill(&request, response);

        assert_eq!(fill.trade_id, "trade-1");
        assert_eq!(fill.order_id.as_deref(), Some("order-1"));
        assert_eq!(fill.price, dec!(0.41));
        assert_eq!(fill.size, dec!(17));
        assert_eq!(fill.status, "MATCHED:unconfirmed");
    }

    #[test]
    fn flatten_trade_fill_zeroes_size_for_unmatched_ack() {
        let request = FlattenRequest {
            asset_id: "123".to_string(),
            side: QuoteSide::Sell,
            price: dec!(0.41),
            size: dec!(17),
            use_fok: true,
        };
        let response = PostOrderResponse::builder()
            .making_amount(dec!(0))
            .taking_amount(dec!(0))
            .order_id("order-2")
            .status(OrderStatusType::Unmatched)
            .success(false)
            .trade_ids(Vec::<String>::new())
            .build();

        let fill = flatten_trade_fill(&request, response);

        assert_eq!(fill.price, dec!(0));
        assert_eq!(fill.size, dec!(0));
        assert_eq!(fill.status, "UNMATCHED:unconfirmed");
    }

    #[test]
    fn map_open_order_uses_remaining_size_after_partial_fill() {
        let order = OpenOrderResponse::builder()
            .id("order-1")
            .status(OrderStatusType::Live)
            .owner(Uuid::parse_str("9180014b-33c8-9240-a14b-bdca11c0a465").unwrap())
            .maker_address(address!("0x1000000000000000000000000000000000000001"))
            .market(B256::ZERO)
            .asset_id(U256::from(123u64))
            .side(Side::Buy)
            .original_size(dec!(10))
            .size_matched(dec!(2.5))
            .price(dec!(0.41))
            .associate_trades(Vec::<String>::new())
            .outcome("YES")
            .created_at(chrono::Utc::now())
            .expiration(chrono::Utc::now())
            .order_type(polymarket_client_sdk::clob::types::OrderType::GTC)
            .build();

        let mapped = map_open_order(order).unwrap();

        assert_eq!(mapped.size, dec!(7.5));
    }

    #[test]
    fn open_orders_request_does_not_send_market_filter() {
        let request = open_orders_request();

        assert_eq!(request.query_params(None), "");
    }

    #[test]
    fn next_open_orders_cursor_starts_with_initial_cursor() {
        assert_eq!(next_open_orders_cursor(None), Some("MA==".to_string()));
    }

    #[test]
    fn next_open_orders_cursor_stops_at_terminal_cursor() {
        assert_eq!(next_open_orders_cursor(Some("LTE=".to_string())), None);
        assert_eq!(next_open_orders_cursor(Some(String::new())), None);
    }

    #[test]
    fn should_continue_open_orders_pagination_matches_cursor_contract() {
        assert!(should_continue_open_orders_pagination("MTE="));
        assert!(!should_continue_open_orders_pagination("LTE="));
        assert!(!should_continue_open_orders_pagination(""));
    }

    #[test]
    fn heartbeat_recovery_id_from_error_extracts_uuid() {
        let message = "Status: error(400 Bad Request) making POST call to /v1/heartbeats with {\"heartbeat_id\":\"40727c3a-473d-43ec-8560-ba9dd202b0b3\",\"error_msg\":\"Invalid Heartbeat ID\"}";

        let heartbeat_id = heartbeat_recovery_id_from_error(message).unwrap();

        assert_eq!(
            heartbeat_id.to_string(),
            "40727c3a-473d-43ec-8560-ba9dd202b0b3"
        );
    }

    #[test]
    fn heartbeat_recovery_id_from_error_ignores_other_errors() {
        let message =
            "Status: error(400 Bad Request) making POST call to /v1/heartbeats with {\"error\":\"other\"}";

        assert!(heartbeat_recovery_id_from_error(message).is_none());
    }

    #[test]
    fn order_belongs_to_market_matches_condition_id() {
        let matching_market =
            B256::from_str("0x04c109837e9c83aa410e4a1bfa32d5574b05fafd0a432514965c03a20e2511a6")
                .unwrap();
        let other_market =
            B256::from_str("0x1111111111111111111111111111111111111111111111111111111111111111")
                .unwrap();
        let order = OpenOrderResponse::builder()
            .id("order-1")
            .status(OrderStatusType::Live)
            .owner(Uuid::parse_str("9180014b-33c8-9240-a14b-bdca11c0a465").unwrap())
            .maker_address(address!("0x1000000000000000000000000000000000000001"))
            .market(matching_market)
            .asset_id(U256::from(123u64))
            .side(Side::Buy)
            .original_size(dec!(10))
            .size_matched(dec!(0))
            .price(dec!(0.41))
            .associate_trades(Vec::<String>::new())
            .outcome("YES")
            .created_at(chrono::Utc::now())
            .expiration(chrono::Utc::now())
            .order_type(polymarket_client_sdk::clob::types::OrderType::GTC)
            .build();

        assert!(order_belongs_to_market(&order, matching_market));
        assert!(!order_belongs_to_market(&order, other_market));
    }

    #[test]
    fn select_trade_order_id_uses_maker_order_for_maker_trades() {
        let order_id = select_trade_order_id(
            Some(TraderSide::Maker),
            Some("taker-1".to_string()),
            Some("maker-1".to_string()),
        );

        assert_eq!(order_id.as_deref(), Some("maker-1"));
    }

    #[test]
    fn select_trade_order_id_uses_taker_order_for_taker_trades() {
        let order_id = select_trade_order_id(
            Some(TraderSide::Taker),
            Some("taker-1".to_string()),
            Some("maker-1".to_string()),
        );

        assert_eq!(order_id.as_deref(), Some("taker-1"));
    }

    #[test]
    fn side_from_sdk_rejects_unknown_variant() {
        let error = side_from_sdk(Side::Unknown).unwrap_err();

        assert!(error.to_string().contains("unexpected SDK side variant"));
    }

    #[test]
    fn mark_stream_message_received_resets_backoff_only_once() {
        let mut received_message = false;
        let mut backoff_secs = 16;

        mark_stream_message_received(&mut received_message, &mut backoff_secs);
        assert!(received_message);
        assert_eq!(backoff_secs, 1);

        backoff_secs = 8;
        mark_stream_message_received(&mut received_message, &mut backoff_secs);
        assert_eq!(backoff_secs, 8);
    }

    #[test]
    fn allowance_is_ready_requires_large_allowance() {
        assert!(!allowance_is_ready(U256::from(1u64)));
        assert!(allowance_is_ready(U256::MAX));
    }

    #[test]
    fn resolve_trading_account_defaults_to_eoa() {
        let signer = address!("0x1111111111111111111111111111111111111111");

        let account = resolve_trading_account(signer, POLYGON, None, None).unwrap();

        assert_eq!(account.kind, TradingAccountKind::Eoa);
        assert_eq!(account.signer, signer);
        assert_eq!(account.funder, None);
        assert_eq!(account.trading_address(), signer);
        assert!(account.requires_onchain_approvals());
    }

    #[test]
    fn resolve_trading_account_infers_proxy_from_funder_address() {
        let signer = address!("0x1111111111111111111111111111111111111111");
        let funder = derive_proxy_wallet(signer, POLYGON).unwrap();

        let account =
            resolve_trading_account(signer, POLYGON, Some(&funder.to_string()), None).unwrap();

        assert_eq!(account.kind, TradingAccountKind::Proxy);
        assert_eq!(account.signer, signer);
        assert_eq!(account.funder, Some(funder));
        assert_eq!(account.trading_address(), funder);
        assert!(!account.requires_onchain_approvals());
    }

    #[test]
    fn resolve_trading_account_infers_safe_from_funder_address() {
        let signer = address!("0x1111111111111111111111111111111111111111");
        let funder = derive_safe_wallet(signer, POLYGON).unwrap();

        let account =
            resolve_trading_account(signer, POLYGON, Some(&funder.to_string()), None).unwrap();

        assert_eq!(account.kind, TradingAccountKind::GnosisSafe);
        assert_eq!(account.funder, Some(funder));
        assert_eq!(account.trading_address(), funder);
        assert!(!account.requires_onchain_approvals());
    }

    #[test]
    fn resolve_trading_account_derives_proxy_funder_from_signature_type() {
        let signer = address!("0x1111111111111111111111111111111111111111");
        let expected_funder = derive_proxy_wallet(signer, POLYGON).unwrap();

        let account = resolve_trading_account(signer, POLYGON, None, Some("POLY_PROXY")).unwrap();

        assert_eq!(account.kind, TradingAccountKind::Proxy);
        assert_eq!(account.funder, Some(expected_funder));
        assert_eq!(account.trading_address(), expected_funder);
    }

    #[test]
    fn resolve_trading_account_rejects_funder_with_eoa_signature_type() {
        let signer = address!("0x1111111111111111111111111111111111111111");
        let funder = derive_proxy_wallet(signer, POLYGON).unwrap();

        let error =
            resolve_trading_account(signer, POLYGON, Some(&funder.to_string()), Some("eoa"))
                .unwrap_err();

        assert!(error
            .to_string()
            .contains("cannot be combined with POLYMARKET_SIGNATURE_TYPE=EOA"));
    }

    #[test]
    fn resolve_trading_account_rejects_mismatched_funder_and_signature_type() {
        let signer = address!("0x1111111111111111111111111111111111111111");
        let proxy_funder = derive_proxy_wallet(signer, POLYGON).unwrap();

        let error = resolve_trading_account(
            signer,
            POLYGON,
            Some(&proxy_funder.to_string()),
            Some("GNOSIS_SAFE"),
        )
        .unwrap_err();

        assert!(error.to_string().contains("does not match derived"));
    }

    #[test]
    fn decimal_to_u256_truncates_not_rounds() {
        use super::decimal_to_u256;

        // 1.999999 with scale 6 should give 1999999, not 2000000
        let result = decimal_to_u256(dec!(1.999999), 6).unwrap();
        assert_eq!(result, U256::from(1_999_999u64));

        // 0.5 at the rounding boundary: 1.5000005 with scale 6 should truncate
        let result = decimal_to_u256(dec!(1.5000005), 6).unwrap();
        assert_eq!(result, U256::from(1_500_000u64));

        // exact value should be unchanged
        let result = decimal_to_u256(dec!(1.5), 6).unwrap();
        assert_eq!(result, U256::from(1_500_000u64));
    }
}
