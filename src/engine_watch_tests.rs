//! The seller's close watch: close events name the channels a tick inspects,
//! with a backstop that inspects every channel.
use super::*;
use std::sync::atomic::Ordering::SeqCst;

/// Opens a seller channel from buyer `n` on the in-memory reserve.
fn seller_channel(chain: &Chain, seller: &Engine, n: u8) -> Channel {
    let byte = format!("{n:02x}");
    let terms = ChannelTerms {
        buyer_peer: byte.repeat(32),
        seller_peer: seller.node.clone(),
        payer: format!("0x{}", byte.repeat(20)),
        payee: seller.wallet_info.address.clone(),
        asset: seller.wallet_info.asset.clone(),
        nonce: byte.repeat(32),
    };
    let funding = chain
        .apply(Request::Open {
            terms: terms.clone(),
            deposit: 100,
            max_fee: 0,
        })
        .unwrap()
        .funding
        .unwrap();
    let mut channel = Channel::new(terms.clone(), terms.buyer_peer, true, 100).unwrap();
    channel.funding = Some(funding);
    channel.state = ChannelState::Open;
    seller.ledger.channel_insert(&channel).unwrap();
    channel
}
/// A chain with event logs at block 10 and `n` open seller channels, after
/// the first tick, which inspects every channel.
async fn watched(n: u8) -> (Pair, Vec<Channel>) {
    let pair = pair();
    let (_, _, chain, _, seller) = &pair;
    *chain.logs.lock().unwrap() = Some(Vec::new());
    chain.height.store(10, SeqCst);
    let channels: Vec<_> = (1..=n).map(|n| seller_channel(chain, seller, n)).collect();
    seller.retry().await.unwrap();
    assert_eq!(Chain::count(&chain.inspects), usize::from(n));
    assert_eq!(seller.ledger.close_cursor().unwrap(), Some(10));
    (pair, channels)
}
/// Requests close on `channel` at `at`, with a log at `block` when given.
fn request_close(chain: &Chain, channel: &Channel, at: u64, block: Option<u64>) {
    let id = channel.funding.clone().unwrap().channel_id;
    chain.request_close(&id, at);
    if let Some(block) = block {
        chain
            .logs
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .push((block, id));
        chain.height.store(block + 3, SeqCst);
    }
}
fn state(seller: &Engine, channel: &Channel) -> ChannelState {
    seller
        .ledger
        .channel_get(&channel.key)
        .unwrap()
        .unwrap()
        .state
}

#[tokio::test]
async fn a_close_event_inspects_only_the_channel_it_names() {
    let ((_bd, _sd, chain, _buyer, seller), channels) = watched(3).await;
    request_close(&chain, &channels[1], now(), Some(12));
    seller.retry().await.unwrap();
    assert_eq!(Chain::count(&chain.inspects), 4);
    assert_eq!(Chain::count(&chain.closes), 1);
    for (n, c) in channels.iter().enumerate() {
        assert_eq!(state(&seller, c) == ChannelState::Closed, n == 1);
    }
    // A quiet tick reads no channel.
    seller.retry().await.unwrap();
    assert_eq!(Chain::count(&chain.inspects), 4);
}

#[tokio::test]
async fn a_failed_scan_keeps_the_cursor_and_inspects_every_channel() {
    let ((_bd, _sd, chain, _buyer, seller), channels) = watched(2).await;
    request_close(&chain, &channels[0], now(), Some(15));
    chain.logs_fail.store(true, SeqCst);
    // Without a cursor move the close request is still found, by inspection.
    seller.retry().await.unwrap();
    assert_eq!(seller.ledger.close_cursor().unwrap(), Some(10));
    assert_eq!(Chain::count(&chain.inspects), 4);
    assert_eq!(state(&seller, &channels[0]), ChannelState::Closed);
    chain.logs_fail.store(false, SeqCst);
    seller.retry().await.unwrap();
    assert_eq!(seller.ledger.close_cursor().unwrap(), Some(18));
}

#[tokio::test]
async fn the_backstop_finds_a_close_request_without_a_log() {
    let ((_bd, _sd, chain, _buyer, seller), channels) = watched(2).await;
    request_close(&chain, &channels[0], now(), None);
    seller.retry().await.unwrap();
    assert_eq!(state(&seller, &channels[0]), ChannelState::Open);
    seller.watch.lock().unwrap().polled_at = 0;
    seller.retry().await.unwrap();
    assert_eq!(Chain::count(&chain.inspects), 4);
    assert_eq!(state(&seller, &channels[0]), ChannelState::Closed);
}

#[tokio::test]
async fn a_deferred_close_is_checked_every_tick_without_another_log() {
    let (_bd, _sd, chain, buyer, seller) = pair();
    *chain.logs.lock().unwrap() = Some(Vec::new());
    chain.height.store(10, SeqCst);
    admit(&buyer, &seller, &uuid::Uuid::new_v4().to_string()).await;
    seller.retry().await.unwrap();
    let open = channel(&seller);
    // The running request defers the close.
    request_close(&chain, &open, now(), Some(12));
    seller.retry().await.unwrap();
    let inspected = Chain::count(&chain.inspects);
    seller.retry().await.unwrap();
    assert_eq!(Chain::count(&chain.inspects), inspected + 1);
    assert_eq!(state(&seller, &open), ChannelState::Open);
    assert_eq!(Chain::count(&chain.closes), 0);
    // Once the deferral is over, the next tick closes without a new log.
    request_close(&chain, &open, now() - 600, None);
    seller.retry().await.unwrap();
    assert_eq!(Chain::count(&chain.closes), 1);
    assert_eq!(state(&seller, &open), ChannelState::Closed);
}
