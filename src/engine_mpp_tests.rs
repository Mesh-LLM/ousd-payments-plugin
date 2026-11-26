use super::super::admission::MINTED;
use super::*;
use crate::contract::{
    Asset, WalletInfo,
    mpp::{ChannelFunding, ChannelTerms, CloseEvents, Closure, Request, Response, Voucher},
};
use crate::{
    config::{Mode, Policy, Price},
    wallet::{Backend, WalletFuture},
};
use serde_json::json;
use std::{
    collections::BTreeMap,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

/// One channel of the in-memory reserve.
struct Fake {
    funding: ChannelFunding,
    deposit: u64,
    topups: u32,
    close_requested_at: u64,
    closure: Option<Closure>,
}
#[derive(Default)]
struct Chain {
    channels: Mutex<BTreeMap<String, Fake>>,
    opens: AtomicUsize,
    topups: AtomicUsize,
    signs: AtomicUsize,
    verifies: AtomicUsize,
    closes: AtomicUsize,
    recovers: AtomicUsize,
    inspects: AtomicUsize,
    pending: AtomicBool,
    /// Close events by block and channel ID; `None` means no event logs, so
    /// every scan is skipped as with the default backend.
    logs: Mutex<Option<Vec<(u64, String)>>>,
    height: std::sync::atomic::AtomicU64,
    logs_fail: AtomicBool,
    /// Public wallet attempts, such as bond locks and withdrawals.
    attempts: Mutex<Vec<Value>>,
    /// Bond transactions the engine asked the wallet for.
    bonds: Mutex<Vec<(String, crate::bonds::Intent)>>,
}
fn known<'a>(
    channels: &'a mut BTreeMap<String, Fake>,
    funding: &ChannelFunding,
) -> Result<&'a mut Fake> {
    let fake = channels
        .get_mut(&funding.channel_id)
        .context("unknown channel")?;
    ensure!(fake.funding == *funding, "funding changed");
    Ok(fake)
}
impl Chain {
    fn count(counter: &AtomicUsize) -> usize {
        counter.load(Ordering::SeqCst)
    }
    /// Sets the payer's close request time of a channel.
    fn request_close(&self, channel_id: &str, at: u64) {
        let mut channels = self.channels.lock().unwrap();
        channels.get_mut(channel_id).unwrap().close_requested_at = at;
    }
    fn open(
        &self,
        channels: &mut BTreeMap<String, Fake>,
        terms: ChannelTerms,
        deposit: u64,
    ) -> Result<Response> {
        let id = terms.salt()?;
        let fake = channels.entry(id.clone()).or_insert_with(|| {
            self.opens.fetch_add(1, Ordering::SeqCst);
            Fake {
                funding: ChannelFunding {
                    terms: terms.clone(),
                    open_tx: "fixture-open".into(),
                    channel_id: id,
                    expiring_nonce_hash: "fixture-context".into(),
                },
                deposit,
                topups: 0,
                close_requested_at: 0,
                closure: None,
            }
        });
        ensure!(fake.funding.terms == terms, "open intent changed");
        Ok(Response {
            funding: Some(fake.funding.clone()),
            deposit,
            ..Default::default()
        })
    }
    fn apply(&self, request: Request) -> Result<Response> {
        let mut channels = self.channels.lock().unwrap();
        if self.pending.load(Ordering::SeqCst)
            && matches!(request, Request::Open { .. } | Request::TopUp { .. })
        {
            return Ok(Response {
                pending: true,
                ..Default::default()
            });
        }
        Ok(match request {
            Request::Open { terms, deposit, .. } => self.open(&mut channels, terms, deposit)?,
            Request::TopUp {
                funding,
                amount,
                index,
                ..
            } => {
                let fake = known(&mut channels, &funding)?;
                ensure!(fake.closure.is_none(), "channel closed");
                if index == fake.topups + 1 {
                    self.topups.fetch_add(1, Ordering::SeqCst);
                    fake.topups = index;
                    fake.deposit += amount;
                    fake.close_requested_at = 0;
                }
                Response {
                    deposit: fake.deposit,
                    ..Default::default()
                }
            }
            Request::SignVoucher { funding, cum } => {
                self.signs.fetch_add(1, Ordering::SeqCst);
                Response {
                    voucher: Some(Voucher {
                        channel_id: funding.channel_id,
                        cum,
                        signature: format!("signed-{cum}"),
                    }),
                    ..Default::default()
                }
            }
            Request::VerifyVoucher { funding, voucher } => {
                self.verifies.fetch_add(1, Ordering::SeqCst);
                let fake = known(&mut channels, &funding)?;
                ensure!(
                    fake.closure.is_none() && fake.close_requested_at == 0,
                    "channel closed or close requested"
                );
                ensure!(
                    voucher.channel_id == funding.channel_id
                        && voucher.signature == format!("signed-{}", voucher.cum)
                        && voucher.cum <= fake.deposit,
                    "unbacked or unauthorized voucher"
                );
                Response {
                    deposit: fake.deposit,
                    ..Default::default()
                }
            }
            Request::Inspect { funding } => {
                self.inspects.fetch_add(1, Ordering::SeqCst);
                known(&mut channels, &funding)?.status()
            }
            Request::Close {
                funding,
                voucher,
                capture,
            } => {
                ensure!(
                    capture <= voucher.map_or(0, |v| v.cum),
                    "capture exceeds voucher"
                );
                let fake = known(&mut channels, &funding)?;
                if fake.closure.is_none() {
                    self.closes.fetch_add(1, Ordering::SeqCst);
                    fake.close("fixture-close", capture);
                }
                fake.status()
            }
            Request::Recover { funding, .. } => {
                self.recovers.fetch_add(1, Ordering::SeqCst);
                let fake = known(&mut channels, &funding)?;
                if fake.closure.is_none() {
                    if fake.close_requested_at == 0 {
                        fake.close_requested_at = now();
                    } else if now() >= fake.close_requested_at + 900 {
                        fake.close("fixture-withdraw", 0);
                    }
                }
                fake.status()
            }
        })
    }
}
impl Fake {
    fn close(&mut self, tx_hash: &str, capture: u64) {
        self.closure = Some(Closure {
            tx_hash: tx_hash.into(),
            channel_id: self.funding.channel_id.clone(),
            captured: capture,
            refunded: self.deposit - capture,
            block_number: 2,
        });
    }
    fn status(&self) -> Response {
        Response {
            deposit: self.deposit,
            close_requested_at: self.close_requested_at,
            closure: self.closure.clone(),
            ..Default::default()
        }
    }
}
impl Backend for Chain {
    fn execute(&self, request: Request) -> WalletFuture<'_, Response> {
        Box::pin(async move { self.apply(request) })
    }
    fn balance(&self) -> WalletFuture<'_, crate::contract::Balance> {
        Box::pin(async { anyhow::bail!("engine fixture balance is not used") })
    }
    fn attempts(&self) -> Result<Vec<Value>> {
        Ok(self.attempts.lock().unwrap().clone())
    }
    fn bond<'a>(
        &'a self,
        _settings: &'a crate::bonds::Settings,
        id: &'a str,
        intent: crate::bonds::Intent,
    ) -> WalletFuture<'a, Value> {
        self.bonds.lock().unwrap().push((id.into(), intent));
        Box::pin(async { Ok(json!({"status": "pending"})) })
    }
    fn close_events(&self, from: Option<u64>) -> WalletFuture<'_, CloseEvents> {
        Box::pin(async move {
            ensure!(!self.logs_fail.load(Ordering::SeqCst), "logs unavailable");
            let to = self.height.load(Ordering::SeqCst);
            let logs = self.logs.lock().unwrap();
            let (Some(logs), Some(from)) = (logs.as_ref(), from) else {
                return Ok(CloseEvents {
                    to,
                    skipped: true,
                    ..Default::default()
                });
            };
            let channels = logs
                .iter()
                .filter(|(block, _)| (from..=to).contains(block))
                .map(|(_, id)| id.to_ascii_lowercase())
                .collect();
            Ok(CloseEvents {
                channels,
                to,
                skipped: false,
            })
        })
    }
}

fn engine(dir: &std::path::Path, seller: bool, chain: Arc<Chain>) -> Arc<Engine> {
    let node = (if seller { "bb" } else { "aa" }).repeat(32);
    let info = WalletInfo {
        wallet_id: node.clone(),
        address: format!("0x{}", (if seller { "22" } else { "11" }).repeat(20)),
        asset: Asset::default(),
        can_send: true,
    };
    engine_with_wallet(dir, seller, chain, info, 0)
}
fn engine_with_wallet(
    dir: &std::path::Path,
    seller: bool,
    wallet: Arc<dyn Backend>,
    info: WalletInfo,
    max_fee: u64,
) -> Arc<Engine> {
    let node = (if seller { "bb" } else { "aa" }).repeat(32);
    let peer = (if seller { "aa" } else { "bb" }).repeat(32);
    let price = Price {
        input_per_million: 1_000_000,
        output_per_million: 1_000_000,
        cache_discount_percent: 0,
    };
    let config = Config {
        bonds: Default::default(),
        data_dir: dir.into(),
        wallet: crate::tempo::Settings {
            network: info.asset.network().unwrap(),
            max_fee,
            ..Default::default()
        },
        prices: if seller {
            BTreeMap::from([("model".into(), price.clone())])
        } else {
            BTreeMap::new()
        },
        policy: if seller {
            Policy::default()
        } else {
            automatic(10000)
        },
        // One top-up covers the cap, and the cap covers a test request's worst case.
        cap: 200,
        buy_peers: [peer].into(),
        buy_prices: BTreeMap::from([("model".into(), price)]),
        admission_deposit: Some(10),
        channel_topup: 200,
    };
    config.validate().unwrap();
    let ledger = Ledger::open(dir).unwrap();
    ledger.pin_node(&node).unwrap();
    ledger.seed(&config).unwrap();
    ledger.mpp_restart().unwrap();
    Arc::new(Engine {
        bonds: crate::bonds::Service::new(&config).unwrap(),
        config,
        ledger,
        wallet,
        wallet_info: info,
        node,
        context: None,
        changed: Default::default(),
        peers: Default::default(),
        ingress: Default::default(),
        unsynced: Default::default(),
        watch: Default::default(),
        standing: Default::default(),
        bond_unsynced: Default::default(),
        outbox: Default::default(),
        unreachable: Default::default(),
        requirements: Default::default(),
        published: Default::default(),
    })
}
type Pair = (
    tempfile::TempDir,
    tempfile::TempDir,
    Arc<Chain>,
    Arc<Engine>,
    Arc<Engine>,
);
/// A buyer and a seller sharing one in-memory reserve.
fn pair() -> Pair {
    let bd = tempfile::tempdir().unwrap();
    let sd = tempfile::tempdir().unwrap();
    let chain = Arc::new(Chain::default());
    let buyer = engine(bd.path(), false, chain.clone());
    let seller = engine(sd.path(), true, chain.clone());
    (bd, sd, chain, buyer, seller)
}
fn event(id: &str) -> Value {
    json!({"phase":"backend_selected","endpoint":"chat_completions","target_peer_id":"bb".repeat(32),"model":"model","observation_id":format!("buyer-{id}"),
        "body":{"model":"model","messages":[{"role":"user","content":"test"}],"max_tokens":4}})
}
fn automatic(daily_budget: u64) -> Policy {
    Policy {
        mode: Mode::Automatic,
        daily_budget: Some(daily_budget),
    }
}
/// Rates of `rate` micro-OUSD per million input and output tokens.
fn price(rate: u64) -> Price {
    Price {
        input_per_million: rate,
        output_per_million: rate,
        cache_discount_percent: 0,
    }
}
/// The worst case of a test request: every body byte as an input token plus
/// its four output tokens, at one micro-OUSD per token.
fn worst(id: &str) -> u64 {
    let payer = format!("0x{}", "11".repeat(20));
    request_intent(&event(id), &payer).unwrap().input_bytes + 4
}
fn usage(input: u64, output: u64) -> Value {
    json!({"prompt_tokens":input,"completion_tokens":output})
}
fn mpp(id: &str, step: Wire) -> Message {
    Message::Mpp {
        version: MPP_VERSION,
        id: id.into(),
        message: Box::new(step),
    }
}
/// Delivers messages both ways until neither side has more to send.
async fn pump(buyer: &Engine, seller: &Engine) -> Result<()> {
    for _ in 0..20 {
        let messages: Vec<_> = buyer.outbox.lock().unwrap().drain(..).collect();
        let replies: Vec<_> = seller.outbox.lock().unwrap().drain(..).collect();
        if messages.is_empty() && replies.is_empty() {
            return Ok(());
        }
        for (_, message) in messages {
            seller.message(&buyer.node, message).await?;
        }
        for (_, message) in replies {
            buyer.message(&seller.node, message).await?;
        }
    }
    bail!("messages never settled")
}
/// The buyer's admission decision while its messages are delivered.
async fn decide(
    buyer: &Arc<Engine>,
    seller: &Arc<Engine>,
    request: Value,
) -> OpenAiExchangeDecision {
    let b = buyer.clone();
    let task = tokio::spawn(async move { b.hook(request).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !task.is_finished() {
            pump(buyer, seller).await.unwrap();
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let decision = task.await.unwrap().unwrap();
    pump(buyer, seller).await.unwrap();
    decision
}
async fn admit(buyer: &Arc<Engine>, seller: &Arc<Engine>, id: &str) -> Value {
    let request = event(id);
    let decision = decide(buyer, seller, request.clone()).await;
    assert_eq!(
        decision.decision,
        OpenAiAdmissionDecision::Allow,
        "{:?}",
        decision.reason
    );
    let mut serving = request;
    serving["target_peer_id"] = Value::Null;
    serving["requested_by_node_id"] = json!(buyer.node);
    serving["observation_id"] = json!(format!("seller-{id}"));
    assert_eq!(
        seller.hook(serving.clone()).await.unwrap().decision,
        OpenAiAdmissionDecision::Allow
    );
    serving
}
async fn terminal(engine: &Engine, role: &str, id: &str, usage: &Value) {
    engine
        .hook(json!({"phase":"exchange_finished","observation_id":format!("{role}-{id}"),"usage":usage}))
        .await
        .unwrap();
}
/// Ends inference on both sides; the bill reaches the buyer before or after
/// its own usage.
async fn finish(buyer: &Engine, seller: &Engine, id: &str, usage: Value, bill_first: bool) {
    terminal(seller, "seller", id, &usage).await;
    if bill_first {
        pump(buyer, seller).await.unwrap();
    }
    terminal(buyer, "buyer", id, &usage).await;
    pump(buyer, seller).await.unwrap();
}
async fn paid_request(buyer: &Arc<Engine>, seller: &Arc<Engine>, usage: Value) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    admit(buyer, seller, &id).await;
    finish(buyer, seller, &id, usage, false).await;
    id
}
fn channel(engine: &Engine) -> Channel {
    let peer = (if engine.node.starts_with("aa") {
        "bb"
    } else {
        "aa"
    })
    .repeat(32);
    engine
        .ledger
        .channel_active(&peer, engine.node.starts_with("bb"))
        .unwrap()
        .unwrap()
}
/// The request ID the buyer minted for the request tagged `tag` by [`event`],
/// or `tag` itself for a request a test inserted under its own ID.
fn rid(tag: &str) -> String {
    let minted = MINTED.lock().unwrap();
    minted
        .get(&format!("buyer-{tag}"))
        .cloned()
        .unwrap_or_else(|| tag.into())
}
fn row(engine: &Engine, tag: &str) -> Row {
    engine.ledger.mpp_get(&rid(tag)).unwrap().unwrap()
}
/// A buyer request a test inserts itself, attached to the HTTP request of
/// [`event`] as the admission hook would attach it.
fn attached(id: &str, peer: &str, intent: crate::protocol::Intent) -> Row {
    let mut row = Row::new(id.into(), peer.into(), false, intent);
    row.observation = Some(format!("buyer-{id}"));
    row
}

#[tokio::test]
async fn first_request_pays_upfront_and_later_requests_reuse_the_channel_without_one() {
    let (_bd, _sd, chain, buyer, seller) = pair();
    let mut quotes = Vec::new();
    for (n, cum) in [(1, 10), (2, 10), (3, 15)] {
        let id = uuid::Uuid::new_v4().to_string();
        admit(&buyer, &seller, &id).await;
        quotes.push(row(&seller, &id).terms.unwrap());
        finish(&buyer, &seller, &id, usage(1, 4), n % 2 == 0).await;
        let (bought, sold) = (row(&buyer, &id), row(&seller, &id));
        assert_eq!(
            (&bought.state, &sold.state),
            (&State::Closed, &State::Closed)
        );
        assert_eq!((&bought.error, &sold.error), (&None, &None));
        assert_eq!(bought.bill, sold.bill);
        assert_eq!(bought.cum_after, Some(cum));
        for side in [channel(&buyer), channel(&seller)] {
            assert_eq!((side.cum(), side.running_total), (cum, 5 * n));
        }
    }
    let views: Vec<_> = quotes
        .iter()
        .map(|q| (q.upfront, q.cum_before, q.cost_before))
        .collect();
    assert_eq!(views, [(10, 0, 0), (0, 10, 5), (0, 10, 10)]);
    assert!(quotes[0].channel_id.is_none());
    assert_eq!(quotes[1].channel_id, quotes[2].channel_id);
    assert_eq!(
        quotes[1].channel_id.as_deref(),
        Some(channel(&buyer).funding.unwrap().channel_id.as_str())
    );
    assert_eq!(Chain::count(&chain.opens), 1);
    assert_eq!(Chain::count(&chain.signs), 2);
    assert_eq!(Chain::count(&chain.closes), 0);
    assert_eq!(buyer.ledger.spent_today().unwrap(), 15);
}

#[tokio::test]
async fn top_up_only_when_the_next_voucher_does_not_fit() {
    let (_bd, _sd, chain, buyer, seller) = pair();
    // Three bills of 64 fit the opening deposit of 200.
    for _ in 0..3 {
        paid_request(&buyer, &seller, usage(60, 4)).await;
    }
    assert_eq!(Chain::count(&chain.topups), 0);
    assert_eq!((channel(&buyer).cum(), channel(&buyer).deposit), (192, 200));
    paid_request(&buyer, &seller, usage(60, 4)).await;
    assert_eq!(Chain::count(&chain.topups), 1);
    let topped = channel(&buyer);
    assert_eq!((topped.cum(), topped.deposit, topped.topups), (256, 400, 1));
    assert_eq!(channel(&seller).deposit, 400);
}

#[tokio::test]
async fn a_worst_case_one_top_up_cannot_pay_is_refused_before_dispatch() {
    let (_bd, _sd, chain, mut buyer, seller) = pair();
    let engine = Arc::get_mut(&mut buyer).unwrap();
    engine.config.channel_topup = 30;
    engine.config.cap = 0;
    engine.config.policy = automatic(30);
    engine.config.validate().unwrap();
    // The operator raises the budget at runtime, beyond what 30 plus one
    // top-up of 30 could pay for a test request's worst case.
    buyer.ledger.set_policy(&automatic(10000)).unwrap();
    let decision = decide(&buyer, &seller, event(&uuid::Uuid::new_v4().to_string())).await;
    assert_eq!(decision.decision, OpenAiAdmissionDecision::Deny);
    assert!(
        decision
            .reason
            .unwrap()
            .contains("one channel top-up cannot cover the worst-case bill")
    );
    assert_eq!(Chain::count(&chain.opens), 0);
}

#[tokio::test]
async fn bills_are_signed_only_when_they_match_the_buyers_own_usage_and_quote() {
    let own = Usage {
        prompt_tokens: 1,
        completion_tokens: 4,
        cached_tokens: 0,
    };
    let other = Usage {
        prompt_tokens: 2,
        ..own
    };
    for (bill, reason) in [
        (
            Bill {
                seq: 0,
                usage: Some(other),
                cost: 6,
                running_total: 6,
            },
            "billed usage differs",
        ),
        (
            Bill {
                seq: 0,
                usage: Some(own),
                cost: 6,
                running_total: 6,
            },
            "cost differs",
        ),
        (
            Bill {
                seq: 0,
                usage: Some(own),
                cost: 5,
                running_total: 6,
            },
            "running total differs",
        ),
        (
            Bill {
                seq: 0,
                usage: None,
                cost: 5,
                running_total: 5,
            },
            "must waive",
        ),
    ] {
        let (_bd, _sd, chain, buyer, seller) = pair();
        let id = uuid::Uuid::new_v4().to_string();
        admit(&buyer, &seller, &id).await;
        terminal(&buyer, "buyer", &id, &usage(1, 4)).await;
        buyer
            .message(&seller.node, mpp(&rid(&id), Wire::Bill(bill)))
            .await
            .unwrap();
        let refused = row(&buyer, &id);
        assert_eq!(refused.state, State::Closed);
        assert!(refused.error.unwrap().contains(reason));
        assert_eq!(
            (channel(&buyer).cum(), channel(&buyer).running_total),
            (10, 0)
        );
        assert_eq!(Chain::count(&chain.signs), 1);
        assert!(buyer.outbox.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn bills_apply_the_cache_discount_and_never_bill_output_beyond_the_limit() {
    let (_bd, _sd, _chain, buyer, seller) = pair();
    let discounted = Price {
        cache_discount_percent: 50,
        ..price(1_000_000)
    };
    seller
        .ledger
        .set_pricing("model", Some(&discounted))
        .unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    admit(&buyer, &seller, &id).await;
    // 2 uncached and 2 cached input tokens at half price, and 4 of 8 output tokens.
    let reported = json!({"prompt_tokens":4,"completion_tokens":8,"prompt_tokens_details":{"cached_tokens":2}});
    finish(&buyer, &seller, &id, reported, true).await;
    let bought = row(&buyer, &id);
    assert_eq!((bought.state, bought.error), (State::Closed, None));
    assert_eq!(bought.bill.unwrap().cost, 7);
    assert_eq!(channel(&seller).running_total, 7);
}

#[tokio::test]
async fn an_unpaid_bill_closes_the_channel_with_the_best_voucher_after_the_payment_time() {
    let (_bd, _sd, chain, buyer, seller) = pair();
    let id = uuid::Uuid::new_v4().to_string();
    admit(&buyer, &seller, &id).await;
    terminal(&seller, "seller", &id, &usage(7, 4)).await;
    terminal(&buyer, "buyer", &id, &usage(6, 4)).await;
    pump(&buyer, &seller).await.unwrap();
    assert!(row(&buyer, &id).error.unwrap().contains("bill refused"));
    seller.retry().await.unwrap();
    pump(&buyer, &seller).await.unwrap();
    assert_eq!(row(&seller, &id).state, State::Billed);
    assert_eq!(Chain::count(&chain.closes), 0);
    seller
        .ledger
        .mpp_update(&rid(&id), |r| {
            r.billed_at = now() - channel::BILL_PAYMENT_SECONDS;
            Ok(())
        })
        .unwrap();
    let key = channel(&seller).key;
    seller.retry().await.unwrap();
    assert!(row(&seller, &id).error.unwrap().contains("bill unpaid"));
    let closed = seller.ledger.channel_get(&key).unwrap().unwrap();
    assert_eq!(closed.lost_bills, 1);
    let closure = closed.closure.unwrap();
    assert_eq!((closure.captured, closure.refunded), (10, 190));
    assert_eq!(Chain::count(&chain.closes), 1);
    pump(&buyer, &seller).await.unwrap();
    assert!(
        buyer
            .ledger
            .channel_active(&seller.node, false)
            .unwrap()
            .is_none()
    );
    // The buyer paid its first upfront, so its new channel needs none.
    let next = uuid::Uuid::new_v4().to_string();
    admit(&buyer, &seller, &next).await;
    assert_eq!(row(&seller, &next).terms.unwrap().upfront, 0);
    assert_eq!(channel(&buyer).cum(), 0);
    assert_eq!(Chain::count(&chain.opens), 2);
}

#[tokio::test]
async fn seller_closes_an_idle_channel_and_one_whose_buyer_requested_close() {
    for requested in [false, true] {
        let (_bd, _sd, chain, buyer, seller) = pair();
        paid_request(&buyer, &seller, usage(1, 4)).await;
        seller.retry().await.unwrap();
        assert_eq!(Chain::count(&chain.closes), 0);
        let open = channel(&seller);
        if requested {
            chain.request_close(&open.funding.unwrap().channel_id, now());
        } else {
            seller
                .ledger
                .channel_update(&open.key, |c| {
                    c.active_at = now() - 3600;
                    Ok(())
                })
                .unwrap();
        }
        seller.retry().await.unwrap();
        assert_eq!(Chain::count(&chain.closes), 1);
        let closed = seller.ledger.channel_get(&open.key).unwrap().unwrap();
        let closure = closed.closure.unwrap();
        assert_eq!((closure.captured, closure.refunded), (5, 195));
        pump(&buyer, &seller).await.unwrap();
        let bought = buyer.ledger.channel_get(&open.key).unwrap().unwrap();
        assert_eq!((bought.state, bought.error), (ChannelState::Closed, None));
    }
}

#[tokio::test]
async fn buyer_recovers_an_idle_channel_by_requesting_close_then_withdrawing() {
    let (_bd, _sd, chain, buyer, seller) = pair();
    paid_request(&buyer, &seller, usage(1, 4)).await;
    let open = channel(&buyer);
    buyer.retry().await.unwrap();
    assert_eq!(Chain::count(&chain.recovers), 0);
    buyer
        .ledger
        .channel_update(&open.key, |c| {
            c.active_at = now() - 7200;
            Ok(())
        })
        .unwrap();
    buyer.retry().await.unwrap();
    buyer.retry().await.unwrap();
    assert_eq!(Chain::count(&chain.recovers), 2);
    assert_eq!(channel(&buyer).state, ChannelState::Closing);
    let id = open.funding.unwrap().channel_id;
    chain.request_close(&id, now() - 900);
    buyer.retry().await.unwrap();
    let closed = buyer.ledger.channel_get(&open.key).unwrap().unwrap();
    assert_eq!(closed.state, ChannelState::Closed);
    assert_eq!(closed.closure.unwrap().refunded, 200);
}

#[tokio::test]
async fn the_buyer_closes_its_channel_by_seller() {
    let (_bd, _sd, chain, buyer, seller) = pair();
    assert!(buyer.buyer_close(None).await.is_err());
    paid_request(&buyer, &seller, usage(1, 4)).await;
    let open = channel(&buyer);
    // Only the buyer's channels count, and a prefix needs eight characters.
    assert!(seller.buyer_close(None).await.is_err());
    assert!(buyer.buyer_close(Some(&open.peer[..7])).await.is_err());
    assert!(buyer.buyer_close(Some("cccccccc")).await.is_err());
    // Without a seller, the only open channel closes.
    assert_eq!(buyer.buyer_close(None).await.unwrap(), open.peer);
    assert_eq!(Chain::count(&chain.recovers), 1);
    assert_eq!(channel(&buyer).state, ChannelState::Closing);
    // A prefix names the same seller.
    let prefix = open.peer[..8].to_ascii_uppercase();
    assert_eq!(buyer.buyer_close(Some(&prefix)).await.unwrap(), open.peer);
    assert_eq!(Chain::count(&chain.recovers), 2);
}

#[tokio::test]
async fn a_seller_offline_through_the_grace_records_the_capture_it_lost() {
    let (_bd, _sd, chain, buyer, seller) = pair();
    paid_request(&buyer, &seller, usage(1, 4)).await;
    let open = channel(&seller);
    let earned = open.cum().min(open.running_total);
    assert!(earned > 0 && open.capture.is_none());
    // The buyer withdrew after the grace while the seller never started closing.
    chain
        .channels
        .lock()
        .unwrap()
        .get_mut(&open.funding.clone().unwrap().channel_id)
        .unwrap()
        .close("fixture-withdraw", 0);
    seller.retry().await.unwrap();
    let closed = seller.ledger.channel_get(&open.key).unwrap().unwrap();
    assert_eq!(closed.state, ChannelState::Closed);
    assert_eq!(Chain::count(&chain.closes), 0);
    let error = closed.error.unwrap();
    assert!(
        error.contains(&format!("{earned} earned, 0 captured")),
        "{error}"
    );
}

#[tokio::test]
async fn a_quote_whose_upfront_exceeds_the_admission_deposit_parses_and_only_buyer_policy_refuses_it()
 {
    let (_bd, _sd, chain, buyer, seller) = pair();
    seller.ledger.set_admission_deposit(1000).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let decision = decide(&buyer, &seller, event(&id)).await;
    assert_eq!(decision.decision, OpenAiAdmissionDecision::Deny);
    assert!(
        decision
            .reason
            .unwrap()
            .contains("upfront exceeds the buyer's admission deposit")
    );
    let quote = row(&seller, &id).terms.unwrap();
    let wire = serde_json::to_string(&mpp(
        &id,
        Wire::Quote {
            terms: quote.clone(),
        },
    ))
    .unwrap();
    let parsed = serde_json::from_str::<Message>(&wire).unwrap();
    assert!(
        matches!(&parsed, Message::Mpp { message, .. } if matches!(message.as_ref(), Wire::Quote { terms } if terms.upfront == 1000 && *terms == quote))
    );
    assert_eq!(row(&buyer, &id).state, State::Closed);
    assert_eq!(row(&seller, &id).state, State::Closed);
    assert_eq!(Chain::count(&chain.opens), 0);
}

#[tokio::test]
async fn the_voucher_is_persisted_before_it_is_sent_and_reused_after_a_restart() {
    let (bd, _sd, chain, buyer, seller) = pair();
    let id = uuid::Uuid::new_v4().to_string();
    let intent = request_intent(&event(&id), &buyer.wallet_info.address).unwrap();
    buyer
        .ledger
        .mpp_insert(&attached(&id, &seller.node, intent.clone()))
        .unwrap();
    seller.mpp_hello(&buyer.node, &id, intent).await.unwrap();
    let quotes: Vec<_> = seller.outbox.lock().unwrap().drain(..).collect();
    for (_, message) in quotes {
        buyer.message(&seller.node, message).await.unwrap();
    }
    // The signed voucher is durable even though its message is lost.
    buyer.outbox.lock().unwrap().clear();
    assert_eq!(row(&buyer, &id).cum_after, Some(10));
    assert_eq!(buyer.ledger.spent_today().unwrap(), 10);
    drop(buyer);
    let buyer = engine(bd.path(), false, chain.clone());
    // The restart ended the HTTP request, so the request is cancelled and
    // its voucher stays as credit for the next one.
    buyer.retry().await.unwrap();
    pump(&buyer, &seller).await.unwrap();
    assert_eq!(row(&buyer, &id).state, State::Closed);
    assert_eq!(row(&seller, &id).state, State::Closed);
    let next = uuid::Uuid::new_v4().to_string();
    admit(&buyer, &seller, &next).await;
    assert_eq!(row(&buyer, &next).cum_after, Some(10));
    assert_eq!(Chain::count(&chain.signs), 1);
    assert_eq!(buyer.ledger.spent_today().unwrap(), 10);
}

#[tokio::test]
async fn terminal_usage_is_billed_durably_before_the_bill_is_sent() {
    let (_bd, sd, chain, buyer, seller) = pair();
    let id = uuid::Uuid::from_u128(44).to_string();
    admit(&buyer, &seller, &id).await;
    let finished = json!({"phase":"exchange_finished","observation_id":format!("seller-{id}"),"usage":usage(2, 3)});
    assert_eq!(seller.record_terminal(&finished).unwrap(), Some(rid(&id)));
    assert!(seller.outbox.lock().unwrap().is_empty());
    drop(seller);
    let seller = engine(sd.path(), true, chain.clone());
    let restored = row(&seller, &id);
    assert_eq!(restored.state, State::Billed);
    assert_eq!(
        restored.bill.map(|b| (b.cost, b.running_total)),
        Some((5, 5))
    );
    assert_eq!(channel(&seller).running_total, 5);
    seller.retry().await.unwrap();
    assert_eq!(row(&seller, &id).state, State::Closed);
    assert!(
        matches!(&seller.outbox.lock().unwrap()[0].1, Message::Mpp { message, .. } if matches!(message.as_ref(), Wire::Bill(bill) if bill.cost == 5))
    );
}

#[tokio::test]
async fn terminal_usage_after_the_channel_closed_is_saved_without_billing() {
    let (_bd, sd, chain, buyer, seller) = pair();
    let id = uuid::Uuid::from_u128(0xdef).to_string();
    admit(&buyer, &seller, &id).await;
    let open = channel(&seller);
    // The running request had its whole margin to be billed.
    chain.request_close(&open.funding.unwrap().channel_id, now() - 600);
    seller.retry().await.unwrap();
    assert_eq!(row(&seller, &id).state, State::Closed);
    let finished = json!({"phase":"exchange_finished","observation_id":format!("seller-{id}"),"usage":usage(2, 3)});
    assert!(seller.record_terminal(&finished).unwrap().is_none());
    drop(seller);
    let seller = engine(sd.path(), true, chain.clone());
    let saved = row(&seller, &id);
    assert_eq!(saved.usage.map(|u| u.prompt_tokens), Some(2));
    assert_eq!((saved.state, saved.bill), (State::Closed, None));
    let closed = seller.ledger.channel_get(&open.key).unwrap().unwrap();
    assert_eq!(closed.closure.unwrap().captured, 0);
    seller.retry().await.unwrap();
    assert_eq!(Chain::count(&chain.closes), 1);
}

#[tokio::test]
async fn an_expired_quote_closes_the_request_without_opening_a_channel() {
    let (_bd, _sd, chain, buyer, seller) = pair();
    let id = uuid::Uuid::new_v4().to_string();
    let intent = request_intent(&event(&id), &buyer.wallet_info.address).unwrap();
    let mut row = Row::new(id.clone(), seller.node.clone(), false, intent.clone());
    seller.mpp_hello(&buyer.node, &id, intent).await.unwrap();
    seller.outbox.lock().unwrap().clear();
    let mut terms = self::row(&seller, &id).terms.unwrap();
    terms.expires_at = now() - 1;
    row.terms = Some(terms);
    buyer.ledger.mpp_insert(&row).unwrap();
    buyer.retry().await.unwrap();
    let closed = self::row(&buyer, &id);
    assert_eq!(closed.state, State::Closed);
    assert!(closed.error.unwrap().contains("expired"));
    assert_eq!(Chain::count(&chain.opens), 0);
    assert!(buyer.ledger.channels_active().unwrap().is_empty());
}

#[tokio::test]
async fn unsupported_protocol_versions_fail_before_state_or_funding() {
    let (_bd, _sd, chain, buyer, seller) = pair();
    let id = uuid::Uuid::new_v4().to_string();
    let intent = request_intent(&event(&id), &buyer.wallet_info.address).unwrap();
    let message = Message::Mpp {
        version: 2,
        id: id.clone(),
        message: Box::new(Wire::Hello { intent }),
    };
    assert!(
        seller
            .message(&buyer.node, message.clone())
            .await
            .unwrap_err()
            .to_string()
            .contains("unsupported MPP protocol version")
    );
    let mut unversioned = serde_json::to_value(message).unwrap();
    unversioned.as_object_mut().unwrap().remove("version");
    assert!(serde_json::from_value::<Message>(unversioned).is_err());
    assert!(seller.ledger.mpp_get(&rid(&id)).unwrap().is_none());
    assert_eq!(Chain::count(&chain.opens), 0);
}

#[tokio::test]
async fn peer_errors_are_recorded_only_against_the_authenticated_mpp_request() {
    let (_bd, _sd, _chain, buyer, _seller) = pair();
    let id = uuid::Uuid::new_v4().to_string();
    let intent = request_intent(&event(&id), &buyer.wallet_info.address).unwrap();
    let peer = "bb".repeat(32);
    buyer
        .ledger
        .mpp_insert(&Row::new(id.clone(), peer.clone(), false, intent))
        .unwrap();
    let message = Message::Error {
        id: id.clone(),
        reason: "quote refused".into(),
    };
    assert!(
        buyer
            .message(&"cc".repeat(32), message.clone())
            .await
            .is_err()
    );
    assert!(row(&buyer, &id).error.is_none());
    buyer.message(&peer, message).await.unwrap();
    let row = row(&buyer, &id);
    assert_eq!(row.error.as_deref(), Some("quote refused"));
    assert_eq!(row.state, State::Negotiating);
}

#[tokio::test]
async fn body_identity_and_live_backing_are_required() {
    let (_bd, _sd, chain, buyer, seller) = pair();
    let id = uuid::Uuid::new_v4().to_string();
    let mut invalid_requests = Vec::new();
    for limit in [0, 4097] {
        let mut request = event(&id);
        request["body"]["max_tokens"] = json!(limit);
        invalid_requests.push(request);
    }
    let mut request = event(&id);
    request["body"]
        .as_object_mut()
        .unwrap()
        .remove("max_tokens");
    request["body"]["max_output_tokens"] = json!(4);
    invalid_requests.push(request);
    let mut request = event(&id);
    request["body"]["messages"][0]["content"] = json!("x".repeat(64 * 1024));
    invalid_requests.push(request);
    let mut reasons = Vec::new();
    for request in invalid_requests {
        let decision = buyer.hook(request).await.unwrap();
        assert_eq!(decision.decision, OpenAiAdmissionDecision::Deny);
        reasons.push(decision.reason.unwrap());
    }
    // A paid route needs a finite output limit for its worst case.
    assert!(reasons[..3].iter().all(|r| r.contains("max_tokens")));
    assert_eq!(Chain::count(&chain.opens), 0);
    let intent = request_intent(&event(&id), &buyer.wallet_info.address).unwrap();
    buyer
        .ledger
        .mpp_insert(&attached(&id, &seller.node, intent.clone()))
        .unwrap();
    seller.mpp_hello(&buyer.node, &id, intent).await.unwrap();
    pump(&buyer, &seller).await.unwrap();
    assert_eq!(row(&seller, &id).state, State::Ready);
    let mut serving = event(&id);
    serving["requested_by_node_id"] = json!(buyer.node);
    serving["target_peer_id"] = Value::Null;
    serving["body"]["messages"][0]["content"] = json!("different inference");
    assert_eq!(
        seller.hook(serving.clone()).await.unwrap().decision,
        OpenAiAdmissionDecision::Deny
    );
    serving["body"]["messages"][0]["content"] = json!("test");
    // Dispatch trusts the voucher verified at admission and the channel state,
    // which the seller's tick moves to closing on a close request.
    let open = channel(&seller);
    chain.request_close(&open.funding.unwrap().channel_id, now());
    let before = Chain::count(&chain.verifies);
    seller.retry().await.unwrap();
    assert_eq!(
        seller.hook(serving).await.unwrap().decision,
        OpenAiAdmissionDecision::Deny
    );
    assert_eq!(Chain::count(&chain.verifies), before);
}

#[tokio::test]
async fn dispatch_needs_no_chain_call_and_an_open_channel() {
    let (_bd, _sd, chain, buyer, seller) = pair();
    let id = uuid::Uuid::new_v4().to_string();
    let intent = request_intent(&event(&id), &buyer.wallet_info.address).unwrap();
    buyer
        .ledger
        .mpp_insert(&attached(&id, &seller.node, intent.clone()))
        .unwrap();
    seller.mpp_hello(&buyer.node, &id, intent).await.unwrap();
    pump(&buyer, &seller).await.unwrap();
    assert_eq!(Chain::count(&chain.verifies), 1);
    let mut serving = event(&id);
    serving["requested_by_node_id"] = json!(buyer.node);
    serving["target_peer_id"] = Value::Null;
    serving["observation_id"] = json!(format!("seller-{id}"));
    let key = channel(&seller).key;
    seller
        .ledger
        .channel_update(&key, |c| {
            c.state = ChannelState::Closing;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        seller.hook(serving.clone()).await.unwrap().decision,
        OpenAiAdmissionDecision::Deny
    );
    seller
        .ledger
        .channel_update(&key, |c| {
            c.state = ChannelState::Open;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        seller.hook(serving).await.unwrap().decision,
        OpenAiAdmissionDecision::Allow
    );
    assert_eq!(Chain::count(&chain.verifies), 1);
}

#[tokio::test]
async fn unsupported_endpoints_and_unpaid_resolved_models_fail_before_funding() {
    let (_bd, _sd, chain, buyer, seller) = pair();
    let id = uuid::Uuid::new_v4().to_string();
    for endpoint in ["responses", "completions"] {
        let mut request = event(&id);
        request["endpoint"] = json!(endpoint);
        assert_eq!(
            buyer.hook(request.clone()).await.unwrap().decision,
            OpenAiAdmissionDecision::Deny
        );
        request["requested_by_node_id"] = json!(buyer.node);
        request["target_peer_id"] = Value::Null;
        assert_eq!(
            seller.hook(request).await.unwrap().decision,
            OpenAiAdmissionDecision::Deny
        );
    }
    let mut ingress = event(&id);
    ingress["phase"] = json!("request_received");
    ingress["requested_by_node_id"] = json!(buyer.node);
    ingress["target_peer_id"] = Value::Null;
    ingress["model"] = json!("unpriced-alias");
    seller.hook(ingress).await.unwrap();
    let selected = json!({"phase":"backend_selected", "model":"model", "observation_id":format!("buyer-{id}")});
    assert_eq!(
        seller.hook(selected).await.unwrap().decision,
        OpenAiAdmissionDecision::Deny
    );
    assert_eq!(Chain::count(&chain.opens), 0);
    assert!(
        serde_json::from_value::<Message>(
            json!({"type":"hello","id":id,"model":"model","payer":"payer"})
        )
        .is_err()
    );
}

#[tokio::test]
async fn an_aborted_pending_buyer_hook_detaches_and_its_request_is_cancelled() {
    let (_bd, _sd, chain, buyer, _seller) = pair();
    let id = uuid::Uuid::new_v4().to_string();
    let task = {
        let buyer = buyer.clone();
        let request = event(&id);
        tokio::spawn(async move { buyer.hook(request).await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if buyer
                .ledger
                .mpp_get(&rid(&id))
                .unwrap()
                .is_some_and(|r| r.observation.is_some())
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(row(&buyer, &id).observation.is_none());
    // No HTTP request can dispatch it now, so the tick cancels it.
    buyer.retry().await.unwrap();
    assert_eq!(row(&buyer, &id).state, State::Closed);
    assert!(matches!(
        &buyer.outbox.lock().unwrap().last().unwrap().1,
        Message::Mpp { message, .. } if matches!(message.as_ref(), Wire::Cancel)
    ));
    assert_eq!(Chain::count(&chain.opens), 0);
}

#[tokio::test]
async fn recovery_and_messages_share_bounded_peer_coordination() {
    let (_bd, _sd, chain, buyer, _seller) = pair();
    let known_peer = "bb".repeat(32);
    let known_lock = buyer.peer_lock(&known_peer).unwrap();
    let mut held = Vec::new();
    {
        let mut peers = buyer.peers.lock().unwrap();
        for index in 0..4095 {
            let lock = Arc::default();
            held.push(Arc::clone(&lock));
            peers.insert(format!("{index:064x}"), lock);
        }
    }
    assert!(Arc::ptr_eq(
        &known_lock,
        &buyer.peer_lock(&known_peer).unwrap()
    ));
    let id = uuid::Uuid::new_v4().to_string();
    let peer = "cc".repeat(32);
    let intent = request_intent(&event(&id), &buyer.wallet_info.address).unwrap();
    buyer
        .ledger
        .mpp_insert(&Row::new(id.clone(), peer.clone(), false, intent))
        .unwrap();
    for result in [
        buyer.mpp_retry_one(&rid(&id)).await,
        buyer.mpp_control(&rid(&id), "cancel").await,
        buyer
            .message(
                &peer,
                Message::Error {
                    id: id.clone(),
                    reason: "error".into(),
                },
            )
            .await,
    ] {
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("peer coordination capacity exhausted")
        );
    }
    assert_eq!(row(&buyer, &id).state, State::Negotiating);
    assert_eq!(Chain::count(&chain.opens), 0);
}

#[test]
fn idle_peer_locks_are_reclaimed_without_replacing_live_locks() {
    let dir = tempfile::tempdir().unwrap();
    let buyer = engine(dir.path(), false, Arc::new(Chain::default()));
    let live = buyer.peer_lock(&"bb".repeat(32)).unwrap();
    for number in 0..10_000 {
        drop(buyer.peer_lock(&format!("{number:064x}")).unwrap());
    }
    assert!(Arc::ptr_eq(
        &live,
        &buyer.peer_lock(&"bb".repeat(32)).unwrap()
    ));
    assert!(buyer.peers.lock().unwrap().len() <= 4096);
}

#[tokio::test(start_paused = true)]
async fn a_paid_request_in_flight_blocks_only_an_identical_paid_request_to_that_seller() {
    let dir = tempfile::tempdir().unwrap();
    let buyer = engine(dir.path(), false, Arc::new(Chain::default()));
    let id = uuid::Uuid::from_u128(42).to_string();
    let request = event(&id);
    let intent = request_intent(&request, &buyer.wallet_info.address).unwrap();
    let mut row = Row::new(id.clone(), "bb".repeat(32), false, intent);
    row.state = State::Running;
    buyer.ledger.mpp_insert(&row).unwrap();
    // The paused clock runs out the wait for the identical request.
    let decision = buyer
        .hook(event(&uuid::Uuid::from_u128(43).to_string()))
        .await
        .unwrap();
    assert_eq!(decision.decision, OpenAiAdmissionDecision::Deny);
    assert!(decision.reason.unwrap().contains("still in flight"));
    // Other routes are outside buyer policy and take the free path.
    for peer in [Value::Null, json!("cc".repeat(32))] {
        let mut rerouted = event(&uuid::Uuid::new_v4().to_string());
        rerouted["target_peer_id"] = peer;
        assert_eq!(
            buyer.hook(rerouted).await.unwrap().decision,
            OpenAiAdmissionDecision::Abstain
        );
    }
    assert!(buyer.outbox.lock().unwrap().is_empty());
}

#[tokio::test]
async fn uuid_aliases_are_one_identity_and_uninspectable_bodies_are_denied() {
    let dir = tempfile::tempdir().unwrap();
    let buyer = engine(dir.path(), false, Arc::new(Chain::default()));
    let id = uuid::Uuid::from_u128(0xabc);
    let request = event(&id.to_string());
    let intent = request_intent(&request, &buyer.wallet_info.address).unwrap();
    let mut row = Row::new(id.to_string(), "bb".repeat(32), false, intent);
    row.state = State::Running;
    buyer.ledger.mpp_insert(&row).unwrap();
    for alias in [
        id.simple().to_string(),
        id.to_string().to_uppercase(),
        id.urn().to_string(),
    ] {
        let mut duplicate = row.clone();
        duplicate.id = alias;
        assert!(buyer.ledger.mpp_insert(&duplicate).is_err());
    }
    for body in [
        Value::Null,
        json!({"messages":[{"content":"x".repeat(65*1024)}]}),
    ] {
        let mut replay = request.clone();
        replay["body"] = body;
        replay["target_peer_id"] = Value::Null;
        assert_eq!(
            buyer.hook(replay).await.unwrap().decision,
            OpenAiAdmissionDecision::Deny
        );
    }
}

#[tokio::test]
async fn free_requests_work_with_spending_enabled_and_never_open_a_channel() {
    let (_bd, _sd, chain, mut buyer, seller) = pair();
    // A seller outside a listed `buy_peers` is never paid.
    Arc::get_mut(&mut buyer).unwrap().config.buy_peers = ["cc".repeat(32)].into();
    assert_eq!(
        buyer
            .hook(event(&uuid::Uuid::new_v4().to_string()))
            .await
            .unwrap()
            .decision,
        OpenAiAdmissionDecision::Abstain
    );
    let mut request = event(&uuid::Uuid::new_v4().to_string());
    request["requested_by_node_id"] = json!(buyer.node);
    request["target_peer_id"] = Value::Null;
    for price in [None, Some(Price::default())] {
        seller.ledger.set_pricing("model", price.as_ref()).unwrap();
        assert_ne!(
            seller.hook(request.clone()).await.unwrap().decision,
            OpenAiAdmissionDecision::Deny
        );
    }
    assert_eq!(Chain::count(&chain.opens), 0);
    assert!(buyer.outbox.lock().unwrap().is_empty());
    for engine in [&buyer, &seller] {
        assert!(engine.ledger.mpp_active().unwrap().is_empty());
        assert!(engine.ledger.channels_active().unwrap().is_empty());
    }
}

#[tokio::test]
async fn paid_buying_follows_buyer_policy_within_its_limits() {
    let (_bd, _sd, chain, mut buyer, seller) = pair();
    // A priced seller refuses a request that arrives unpaid.
    let mut free = event(&uuid::Uuid::new_v4().to_string());
    free["requested_by_node_id"] = json!(buyer.node);
    assert_eq!(
        seller.hook(free).await.unwrap().decision,
        OpenAiAdmissionDecision::Deny
    );
    // A free_only policy sends the request free, before any message,
    // voucher or channel.
    buyer.ledger.set_policy(&Policy::default()).unwrap();
    let decision = buyer
        .hook(event(&uuid::Uuid::new_v4().to_string()))
        .await
        .unwrap();
    assert_eq!(decision.decision, OpenAiAdmissionDecision::Abstain);
    assert!(buyer.outbox.lock().unwrap().is_empty());
    buyer.ledger.set_policy(&automatic(10000)).unwrap();
    // The cap bounds the upfront of 10.
    Arc::get_mut(&mut buyer).unwrap().config.cap = 9;
    let decision = decide(&buyer, &seller, event(&uuid::Uuid::new_v4().to_string())).await;
    assert_eq!(decision.decision, OpenAiAdmissionDecision::Deny);
    assert!(decision.reason.unwrap().contains("per-request cap"));
    assert_eq!(Chain::count(&chain.opens), 0);
    assert!(buyer.ledger.channels_active().unwrap().is_empty());
}

#[tokio::test]
async fn removing_a_price_keeps_original_agreements_available_for_reconciliation() {
    let (_bd, _sd, chain, buyer, seller) = pair();
    let id = uuid::Uuid::new_v4().to_string();
    let intent = request_intent(&event(&id), &buyer.wallet_info.address).unwrap();
    seller
        .mpp_hello(&buyer.node, &id, intent.clone())
        .await
        .unwrap();
    let original = row(&seller, &id).terms.unwrap();
    seller.ledger.set_pricing("model", None).unwrap();
    seller.outbox.lock().unwrap().clear();
    seller
        .mpp_hello(&buyer.node, &id, intent.clone())
        .await
        .unwrap();
    {
        let messages = seller.outbox.lock().unwrap();
        assert!(
            matches!(&messages[0].1, Message::Mpp {message, ..} if matches!(message.as_ref(), Wire::Quote {terms} if terms == &original))
        );
    }
    let refused = uuid::Uuid::new_v4().to_string();
    seller
        .mpp_hello(&buyer.node, &refused, intent)
        .await
        .unwrap();
    assert!(seller.ledger.mpp_get(&rid(&refused)).unwrap().is_none());
    assert!(
        matches!(&seller.outbox.lock().unwrap().last().unwrap().1, Message::Mpp {message, ..} if matches!(message.as_ref(), Wire::Unpriced))
    );
    assert_eq!(Chain::count(&chain.opens), 0);
}

#[tokio::test]
async fn removing_a_price_cannot_turn_a_dispatched_seller_request_into_a_free_replay() {
    let (_bd, _sd, _chain, buyer, seller) = pair();
    let id = uuid::Uuid::from_u128(0xabc).to_string();
    let mut serving = admit(&buyer, &seller, &id).await;
    drop(buyer);
    seller.ledger.set_pricing("model", None).unwrap();
    serving["observation_id"] = json!("another-serving-attempt");
    let mut ingress = serving.clone();
    ingress["phase"] = json!("request_received");
    seller.hook(ingress).await.unwrap();
    // Mesh supplies the authenticated peer on ingress, not on backend selection.
    serving["requested_by_node_id"] = Value::Null;
    serving["body"] = Value::Null;
    assert_eq!(
        seller.hook(serving).await.unwrap().decision,
        OpenAiAdmissionDecision::Deny
    );
}

#[tokio::test]
async fn locked_seller_keeps_its_channel_without_reissuing_ready_notices() {
    let (_bd, _sd, chain, buyer, mut seller) = pair();
    let id = uuid::Uuid::new_v4().to_string();
    let mut request = event(&id);
    let intent = request_intent(&request, &buyer.wallet_info.address).unwrap();
    buyer
        .ledger
        .mpp_insert(&attached(&id, &seller.node, intent.clone()))
        .unwrap();
    seller
        .mpp_hello(&buyer.node, &id, intent.clone())
        .await
        .unwrap();
    pump(&buyer, &seller).await.unwrap();
    Arc::get_mut(&mut seller).unwrap().wallet_info.can_send = false;
    assert!(
        seller
            .mpp_hello(&buyer.node, &id, intent)
            .await
            .unwrap_err()
            .to_string()
            .contains("unlock")
    );
    seller.retry().await.unwrap();
    assert!(seller.outbox.lock().unwrap().is_empty());
    request["target_peer_id"] = Value::Null;
    request["requested_by_node_id"] = json!(buyer.node);
    request["observation_id"] = json!(format!("locked-{id}"));
    assert_eq!(
        seller.hook(request).await.unwrap().decision,
        OpenAiAdmissionDecision::Deny
    );
    let saved = row(&seller, &id);
    assert_eq!(saved.state, State::Ready);
    assert!(saved.channel.is_some());
    Arc::get_mut(&mut seller).unwrap().wallet_info.can_send = true;
    seller.retry().await.unwrap();
    assert!(matches!(&seller.outbox.lock().unwrap().last().unwrap().1,
        Message::Mpp { message, .. } if matches!(message.as_ref(), Wire::Ready)));
    assert_eq!(Chain::count(&chain.opens), 1);
}

#[tokio::test]
async fn a_seller_without_a_paid_price_serves_the_model_free() {
    let (_bd, _sd, chain, buyer, seller) = pair();
    seller.ledger.set_pricing("model", Some(&price(0))).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let request = event(&id);
    let decision = decide(&buyer, &seller, request.clone()).await;
    assert_eq!(decision.decision, OpenAiAdmissionDecision::Abstain);
    let bought = row(&buyer, &id);
    assert_eq!(bought.state, State::Closed);
    assert_eq!(bought.error.as_deref(), Some(SERVED_FREE));
    assert!(seller.ledger.mpp_get(&rid(&id)).unwrap().is_none());
    let mut serving = request;
    serving["target_peer_id"] = Value::Null;
    serving["requested_by_node_id"] = json!(buyer.node);
    serving["observation_id"] = json!(format!("seller-{id}"));
    assert_eq!(
        seller.hook(serving).await.unwrap().decision,
        OpenAiAdmissionDecision::Abstain
    );
    assert_eq!(Chain::count(&chain.opens), 0);
    let paid = Price {
        input_per_million: 1_000_000,
        ..price(0)
    };
    seller.ledger.set_pricing("model", Some(&paid)).unwrap();
    let next = uuid::Uuid::new_v4().to_string();
    admit(&buyer, &seller, &next).await;
    buyer
        .message(
            &seller.node,
            Message::Error {
                id: rid(&next),
                reason: "transient failure".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(row(&buyer, &next).state, State::Running);
}

#[tokio::test]
async fn a_generic_locked_seller_error_keeps_a_lost_quote_retryable() {
    let (_bd, _sd, chain, buyer, mut seller) = pair();
    let id = uuid::Uuid::new_v4().to_string();
    let intent = request_intent(&event(&id), &buyer.wallet_info.address).unwrap();
    buyer
        .ledger
        .mpp_insert(&attached(&id, &seller.node, intent.clone()))
        .unwrap();
    seller
        .mpp_hello(&buyer.node, &id, intent.clone())
        .await
        .unwrap();
    seller.outbox.lock().unwrap().clear();
    Arc::get_mut(&mut seller).unwrap().wallet_info.can_send = false;
    let error = seller
        .message(&buyer.node, mpp(&rid(&id), Wire::Hello { intent }))
        .await
        .unwrap_err();
    buyer
        .message(
            &seller.node,
            Message::Error {
                id: rid(&id),
                reason: error.to_string(),
            },
        )
        .await
        .unwrap();
    assert_eq!(row(&buyer, &id).state, State::Negotiating);
    Arc::get_mut(&mut seller).unwrap().wallet_info.can_send = true;
    // The buyer repeats its hello and gets the original quote.
    buyer.retry().await.unwrap();
    pump(&buyer, &seller).await.unwrap();
    assert_eq!(row(&buyer, &id).state, State::Ready);
    assert_eq!(Chain::count(&chain.opens), 1);
    assert!(
        buyer
            .mpp_message(
                &seller.node,
                &id,
                Wire::QuoteRejected {
                    reason: "late refusal".into()
                }
            )
            .await
            .is_err()
    );
    assert_eq!(row(&buyer, &id).state, State::Ready);
}

/// Native wallets against the loopback RPC fixture, for both networks.
async fn native_pair(
    url: &str,
    network: crate::contract::Network,
    dirs: &[tempfile::TempDir; 4],
) -> (Arc<Engine>, Arc<Engine>) {
    use crate::tempo::rpc_fixture_tests;
    let buyer_wallet = Arc::new(rpc_fixture_tests::wallet_on_network(
        dirs[2].path(),
        url,
        1,
        network,
    ));
    let seller_wallet = Arc::new(rpc_fixture_tests::wallet_on_network(
        dirs[3].path(),
        url,
        2,
        network,
    ));
    let buyer_info = buyer_wallet.info().await.unwrap();
    let seller_info = seller_wallet.info().await.unwrap();
    (
        engine_with_wallet(dirs[0].path(), false, buyer_wallet, buyer_info, 1000),
        engine_with_wallet(dirs[1].path(), true, seller_wallet, seller_info, 1000),
    )
}

#[tokio::test]
async fn engine_uses_native_wallet_directly_for_one_channel_bills_and_close() {
    for network in [
        crate::contract::Network::Testnet,
        crate::contract::Network::Mainnet,
    ] {
        let (chain, url, server) = crate::tempo::rpc_fixture_tests::server().await;
        chain.lock().unwrap().chain_id = network.chain_id();
        let dirs = std::array::from_fn(|_| tempfile::tempdir().unwrap());
        let (buyer, seller) = native_pair(&url, network, &dirs).await;
        for _ in 0..2 {
            let id = uuid::Uuid::new_v4().to_string();
            let request = event(&id);
            let task = {
                let buyer = buyer.clone();
                let request = request.clone();
                tokio::spawn(async move { buyer.hook(request).await })
            };
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while !task.is_finished() {
                    pump(&buyer, &seller).await.unwrap();
                    buyer.retry().await.unwrap();
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert_eq!(
                task.await.unwrap().unwrap().decision,
                OpenAiAdmissionDecision::Allow
            );
            let mut serving = request;
            serving["target_peer_id"] = Value::Null;
            serving["requested_by_node_id"] = json!(buyer.node);
            serving["observation_id"] = json!(format!("seller-{id}"));
            assert_eq!(
                seller.hook(serving).await.unwrap().decision,
                OpenAiAdmissionDecision::Allow
            );
            finish(&buyer, &seller, &id, usage(1, 4), false).await;
            assert_eq!(row(&buyer, &id).state, State::Closed);
        }
        let open = channel(&seller);
        assert_eq!(
            (open.cum(), open.running_total, open.deposit),
            (10, 10, 200)
        );
        seller
            .ledger
            .channel_update(&open.key, |c| {
                c.active_at = now() - 3600;
                Ok(())
            })
            .unwrap();
        seller.retry().await.unwrap();
        seller.retry().await.unwrap();
        pump(&buyer, &seller).await.unwrap();
        let closed = buyer.ledger.channel_get(&open.key).unwrap().unwrap();
        let closure = closed.closure.unwrap();
        assert_eq!((closure.captured, closure.refunded), (10, 190));
        assert_eq!(closed.error, None);
        assert_eq!(buyer.wallet.attempts().unwrap().len(), 1);
        assert_eq!(seller.wallet.attempts().unwrap().len(), 1);
        server.abort();
    }
}

#[tokio::test]
async fn a_pending_open_is_replayed_from_its_original_bytes_after_a_buyer_restart() {
    let network = crate::contract::Network::Testnet;
    let (chain, url, server) = crate::tempo::rpc_fixture_tests::server().await;
    let dirs = std::array::from_fn(|_| tempfile::tempdir().unwrap());
    let (buyer, seller) = native_pair(&url, network, &dirs).await;
    chain.lock().unwrap().hide_receipt = true;
    let id = uuid::Uuid::new_v4().to_string();
    let intent = request_intent(&event(&id), &buyer.wallet_info.address).unwrap();
    buyer
        .ledger
        .mpp_insert(&attached(&id, &seller.node, intent.clone()))
        .unwrap();
    seller.mpp_hello(&buyer.node, &id, intent).await.unwrap();
    pump(&buyer, &seller).await.unwrap();
    assert_eq!(channel(&buyer).state, ChannelState::Opening);
    let raw = buyer.wallet.attempts().unwrap()[0]["hash"].clone();
    drop(buyer);
    let (buyer, seller) = {
        drop(seller);
        native_pair(&url, network, &dirs).await
    };
    chain.lock().unwrap().hide_receipt = false;
    buyer.retry().await.unwrap();
    pump(&buyer, &seller).await.unwrap();
    // The restart ended the HTTP request, so the request is cancelled while
    // its channel still opens.
    buyer.retry().await.unwrap();
    assert_eq!(channel(&buyer).state, ChannelState::Open);
    assert_eq!(row(&buyer, &id).state, State::Closed);
    assert_eq!(row(&seller, &id).state, State::Closed);
    let attempts = buyer.wallet.attempts().unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0]["hash"], raw);
    server.abort();
}

#[tokio::test]
async fn a_refused_first_request_opens_no_channel() {
    let (_bd, _sd, chain, buyer, seller) = pair();
    buyer.ledger.set_policy(&automatic(5)).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let decision = decide(&buyer, &seller, event(&id)).await;
    assert_eq!(decision.decision, OpenAiAdmissionDecision::Deny);
    assert!(decision.reason.unwrap().contains("daily budget"));
    assert_eq!(Chain::count(&chain.opens), 0);
    assert!(buyer.ledger.channels_active().unwrap().is_empty());
    assert_eq!(row(&seller, &id).state, State::Closed);
}

#[tokio::test]
async fn a_channel_without_a_voucher_is_still_new_to_the_seller() {
    let (_bd, _sd, chain, buyer, seller) = pair();
    let id = uuid::Uuid::new_v4().to_string();
    let intent = request_intent(&event(&id), &buyer.wallet_info.address).unwrap();
    buyer
        .ledger
        .mpp_insert(&attached(&id, &seller.node, intent.clone()))
        .unwrap();
    seller.mpp_hello(&buyer.node, &id, intent).await.unwrap();
    chain.pending.store(true, Ordering::SeqCst);
    pump(&buyer, &seller).await.unwrap();
    assert_eq!(channel(&buyer).state, ChannelState::Opening);
    // The request expires before its open finalizes, so no voucher is signed.
    buyer
        .ledger
        .mpp_update(&rid(&id), |r| {
            r.terms.as_mut().unwrap().expires_at = now() - 1;
            Ok(())
        })
        .unwrap();
    seller.mpp_close(&id, "expired").unwrap();
    chain.pending.store(false, Ordering::SeqCst);
    buyer.retry().await.unwrap();
    let opened = channel(&buyer);
    assert_eq!(
        (opened.state.clone(), opened.voucher.is_none()),
        (ChannelState::Open, true)
    );
    let next = uuid::Uuid::new_v4().to_string();
    admit(&buyer, &seller, &next).await;
    let quote = row(&seller, &next).terms.unwrap();
    assert_eq!((quote.channel_id, quote.upfront), (None, 10));
    assert_eq!(channel(&buyer).key, opened.key);
    assert_eq!(Chain::count(&chain.opens), 1);
}

#[tokio::test]
async fn a_dispatched_request_without_a_bill_closes_after_its_quote_expires() {
    let (_bd, _sd, _chain, buyer, seller) = pair();
    let id = uuid::Uuid::new_v4().to_string();
    admit(&buyer, &seller, &id).await;
    terminal(&buyer, "buyer", &id, &usage(1, 4)).await;
    buyer.retry().await.unwrap();
    assert_eq!(row(&buyer, &id).state, State::Running);
    buyer
        .ledger
        .mpp_update(&rid(&id), |r| {
            r.terms.as_mut().unwrap().expires_at = now() - channel::UNBILLED_SECONDS;
            Ok(())
        })
        .unwrap();
    buyer.retry().await.unwrap();
    let closed = row(&buyer, &id);
    assert_eq!(closed.state, State::Closed);
    assert!(closed.error.unwrap().contains("no bill"));
    assert_eq!(
        (channel(&buyer).cum(), channel(&buyer).running_total),
        (10, 0)
    );
}

#[tokio::test]
async fn retry_closes_requests_whose_channel_already_closed() {
    let (_bd, _sd, _chain, buyer, seller) = pair();
    let id = uuid::Uuid::new_v4().to_string();
    admit(&buyer, &seller, &id).await;
    let key = channel(&buyer).key;
    buyer
        .ledger
        .channel_update(&key, |c| {
            c.state = ChannelState::Closed;
            Ok(())
        })
        .unwrap();
    buyer.retry().await.unwrap();
    let closed = row(&buyer, &id);
    assert_eq!(closed.state, State::Closed);
    assert!(closed.error.unwrap().contains("channel closed"));
}

#[tokio::test]
async fn the_payment_time_starts_when_the_bill_is_first_sent() {
    let (_bd, _sd, chain, buyer, seller) = pair();
    let id = uuid::Uuid::new_v4().to_string();
    admit(&buyer, &seller, &id).await;
    let finished = json!({"phase":"exchange_finished","observation_id":format!("seller-{id}"),"usage":usage(7, 4)});
    seller.record_terminal(&finished).unwrap();
    assert_eq!(row(&seller, &id).billed_at, 0);
    // A bill records no creation time, so while no send succeeds no payment
    // time runs, however long the outage lasts.
    seller.unreachable.store(true, Ordering::SeqCst);
    for _ in 0..3 {
        assert!(seller.retry().await.is_err());
    }
    let waiting = row(&seller, &id);
    assert_eq!((waiting.state, waiting.billed_at), (State::Billed, 0));
    seller.unreachable.store(false, Ordering::SeqCst);
    seller.retry().await.unwrap();
    let billed = row(&seller, &id);
    assert_eq!(billed.state, State::Billed);
    assert!(billed.billed_at > 0);
    assert_eq!(Chain::count(&chain.closes), 0);
    // The payment time runs from that first send.
    seller
        .ledger
        .mpp_update(&rid(&id), |r| {
            r.billed_at -= channel::BILL_PAYMENT_SECONDS;
            Ok(())
        })
        .unwrap();
    seller.retry().await.unwrap();
    assert!(row(&seller, &id).error.unwrap().contains("bill unpaid"));
    assert_eq!(Chain::count(&chain.closes), 1);
}

#[tokio::test]
async fn a_bill_covered_by_credit_settles_only_once_it_was_sent() {
    let (_bd, _sd, _chain, buyer, seller) = pair();
    let id = uuid::Uuid::new_v4().to_string();
    admit(&buyer, &seller, &id).await;
    terminal(&buyer, "buyer", &id, &usage(1, 4)).await;
    seller.unreachable.store(true, Ordering::SeqCst);
    let finished = json!({"phase":"exchange_finished","observation_id":format!("seller-{id}"),"usage":usage(1, 4)});
    assert_eq!(seller.record_terminal(&finished).unwrap(), Some(rid(&id)));
    assert!(seller.retry().await.is_err());
    assert_eq!(row(&seller, &id).state, State::Billed);
    seller.unreachable.store(false, Ordering::SeqCst);
    seller.retry().await.unwrap();
    assert_eq!(row(&seller, &id).state, State::Closed);
    pump(&buyer, &seller).await.unwrap();
    let bought = row(&buyer, &id);
    assert_eq!((bought.state, bought.error), (State::Closed, None));
    assert_eq!(channel(&buyer).running_total, 5);
}

#[tokio::test]
async fn a_close_request_lets_the_running_request_bill_first() {
    for late in [false, true] {
        let (_bd, _sd, chain, buyer, seller) = pair();
        let id = uuid::Uuid::new_v4().to_string();
        admit(&buyer, &seller, &id).await;
        let open = channel(&seller);
        let requested = if late { now() - 600 } else { now() };
        chain.request_close(&open.funding.unwrap().channel_id, requested);
        seller.retry().await.unwrap();
        let closure = |seller: &Engine| {
            let closed = seller.ledger.channel_get(&open.key).unwrap().unwrap();
            closed.closure.unwrap().captured
        };
        if late {
            // Past the safe margin the seller closes with what is billed so far.
            assert_eq!(closure(&seller), 0);
            assert_eq!(row(&seller, &id).state, State::Closed);
            continue;
        }
        assert_eq!(Chain::count(&chain.closes), 0);
        assert_eq!(channel(&seller).state, ChannelState::Open);
        terminal(&seller, "seller", &id, &usage(1, 4)).await;
        seller.retry().await.unwrap();
        assert_eq!(Chain::count(&chain.closes), 1);
        assert_eq!(closure(&seller), 5);
    }
}

#[tokio::test]
async fn a_lost_close_notice_is_repeated_on_the_next_hello() {
    let (_bd, _sd, chain, buyer, seller) = pair();
    paid_request(&buyer, &seller, usage(1, 4)).await;
    let old = channel(&seller);
    seller
        .ledger
        .channel_update(&old.key, |c| {
            c.active_at = now() - 3600;
            Ok(())
        })
        .unwrap();
    seller.retry().await.unwrap();
    seller.outbox.lock().unwrap().clear();
    assert_eq!(channel(&buyer).state, ChannelState::Open);
    let next = uuid::Uuid::new_v4().to_string();
    admit(&buyer, &seller, &next).await;
    let closed = buyer.ledger.channel_get(&old.key).unwrap().unwrap();
    assert_eq!((closed.state, closed.error), (ChannelState::Closed, None));
    assert_eq!(Chain::count(&chain.opens), 2);
}

#[tokio::test]
async fn a_new_channel_pays_the_models_own_deposit_until_its_price_is_removed() {
    let (_bd, _sd, _chain, buyer, seller) = pair();
    seller
        .ledger
        .set_model_admission_deposit("model", Some(7))
        .unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    admit(&buyer, &seller, &id).await;
    assert_eq!(row(&seller, &id).terms.unwrap().upfront, 7);
    assert_eq!(channel(&buyer).cum(), 7);
    seller.ledger.set_pricing("model", None).unwrap();
    seller
        .ledger
        .set_pricing("model", Some(&price(1_000_000)))
        .unwrap();
    assert_eq!(seller.ledger.model_admission_deposit("model").unwrap(), 10);
}

#[path = "engine_bond_tests.rs"]
mod bond;
#[path = "engine_buyer_tests.rs"]
mod buyer;
#[path = "engine_control_tests.rs"]
mod control;
#[path = "engine_parallel_tests.rs"]
mod parallel;
#[path = "engine_sequence_tests.rs"]
mod sequence;
#[path = "engine_watch_tests.rs"]
mod watch;

#[tokio::test(start_paused = true)]
async fn a_request_refused_while_its_channel_opens_is_cancelled_so_the_next_one_proceeds() {
    let (_bd, _sd, chain, buyer, seller) = pair();
    chain.pending.store(true, Ordering::SeqCst);
    let id = uuid::Uuid::new_v4().to_string();
    let task = {
        let buyer = buyer.clone();
        tokio::spawn(async move { buyer.hook(event(&id)).await })
    };
    while buyer.outbox.lock().unwrap().is_empty() {
        tokio::task::yield_now().await;
    }
    pump(&buyer, &seller).await.unwrap();
    assert_eq!(channel(&buyer).state, ChannelState::Opening);
    // The paused clock runs out the admission wait.
    let decision = task.await.unwrap().unwrap();
    assert_eq!(decision.decision, OpenAiAdmissionDecision::Deny);
    assert!(decision.reason.unwrap().contains("not ready yet"));
    pump(&buyer, &seller).await.unwrap();
    assert!(seller.ledger.mpp_active().unwrap().is_empty());
    chain.pending.store(false, Ordering::SeqCst);
    buyer.retry().await.unwrap();
    let next = uuid::Uuid::new_v4().to_string();
    admit(&buyer, &seller, &next).await;
    assert_eq!(row(&buyer, &next).cum_after, Some(10));
    assert_eq!(Chain::count(&chain.opens), 1);
}
