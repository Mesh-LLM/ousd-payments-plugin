//! Back-to-back requests on one pair: busy answers, unbilled notices,
//! reordered bills, and a node that both buys from and sells to a peer.
use super::*;

/// Delivers every message `from` has queued to `to`.
pub(super) async fn deliver(from: &Engine, to: &Engine) {
    let messages: Vec<_> = from.outbox.lock().unwrap().drain(..).collect();
    for (_, message) in messages {
        to.message(&from.node, message).await.unwrap();
    }
}
/// Starts the buyer's admission hook for a new request and hands its hello
/// to the seller, without delivering the seller's answer.
pub(super) async fn hello(
    buyer: &Arc<Engine>,
    seller: &Engine,
    tag: &str,
) -> tokio::task::JoinHandle<Result<OpenAiExchangeDecision>> {
    let task = {
        let buyer = buyer.clone();
        let request = event(tag);
        tokio::spawn(async move { buyer.hook(request).await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while buyer.outbox.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    deliver(buyer, seller).await;
    task
}
/// The admission decision once all messages flow freely again.
pub(super) async fn settle(
    buyer: &Engine,
    seller: &Engine,
    task: tokio::task::JoinHandle<Result<OpenAiExchangeDecision>>,
) -> OpenAiExchangeDecision {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !task.is_finished() {
            pump(buyer, seller).await.unwrap();
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    task.await.unwrap().unwrap()
}

#[tokio::test]
async fn unknown_usage_after_a_seller_restart_is_never_billed_and_the_next_request_proceeds() {
    let (bd, sd, chain, buyer, seller) = pair();
    let id = uuid::Uuid::new_v4().to_string();
    let serving = admit(&buyer, &seller, &id).await;
    terminal(&buyer, "buyer", &id, &usage(1, 4)).await;
    drop((buyer, seller));
    let buyer = engine(bd.path(), false, chain.clone());
    let seller = engine(sd.path(), true, chain.clone());
    assert_eq!(row(&seller, &id).state, State::UsageUnknown);
    assert_eq!(row(&buyer, &id).state, State::Running);
    // The interrupted inference is never replayed.
    assert_eq!(
        seller.hook(serving).await.unwrap().decision,
        OpenAiAdmissionDecision::Deny
    );
    // The seller's notice closes the buyer's request unpaid at once.
    seller.retry().await.unwrap();
    pump(&buyer, &seller).await.unwrap();
    let unpaid = row(&buyer, &id);
    assert_eq!(unpaid.state, State::Closed);
    assert!(unpaid.error.unwrap().contains("never bills it"));
    let unknown = row(&seller, &id);
    assert_eq!(
        (&unknown.state, &unknown.bill),
        (&State::UsageUnknown, &None)
    );
    assert!(unknown.superseded && unknown.unbilled_seen);
    // An acknowledged notice is not repeated.
    seller.retry().await.unwrap();
    assert!(seller.outbox.lock().unwrap().is_empty());
    let next = uuid::Uuid::new_v4().to_string();
    admit(&buyer, &seller, &next).await;
    assert_eq!(row(&seller, &next).terms.unwrap().upfront, 0);
    finish(&buyer, &seller, &next, usage(1, 4), false).await;
    assert_eq!(row(&buyer, &next).state, State::Closed);
    // Usage that arrives after the notice is recorded, not billed.
    let late = json!({"phase":"exchange_finished","observation_id":format!("seller-{id}"),"usage":usage(2, 3)});
    assert!(seller.record_terminal(&late).unwrap().is_none());
    let recorded = row(&seller, &id);
    assert_eq!(recorded.state, State::Closed);
    assert_eq!(recorded.usage.map(|u| u.prompt_tokens), Some(2));
    assert!(recorded.bill.is_none());
    assert!(recorded.error.unwrap().contains("not billed"));
    for side in [channel(&buyer), channel(&seller)] {
        assert_eq!((side.cum(), side.running_total), (10, 5));
    }
    assert_eq!(Chain::count(&chain.closes), 0);
}

#[tokio::test]
async fn a_waiver_bills_zero_before_a_later_quote_and_only_records_after_one() {
    for later in [false, true] {
        let (_bd, sd, chain, buyer, seller) = pair();
        let id = uuid::Uuid::new_v4().to_string();
        admit(&buyer, &seller, &id).await;
        drop(seller);
        let seller = engine(sd.path(), true, chain.clone());
        terminal(&buyer, "buyer", &id, &usage(1, 4)).await;
        let next = uuid::Uuid::new_v4().to_string();
        if later {
            admit(&buyer, &seller, &next).await;
        }
        seller.mpp_control(&rid(&id), "waive").await.unwrap();
        pump(&buyer, &seller).await.unwrap();
        let (bought, sold) = (row(&buyer, &id), row(&seller, &id));
        assert_eq!((bought.state, sold.state), (State::Closed, State::Closed));
        assert_eq!(sold.usage, None);
        // Before a later quote the zero bill settles the buyer's request.
        assert_eq!(bought.error.is_none(), !later);
        assert_eq!(sold.bill.is_some(), !later);
        assert_eq!(channel(&buyer).running_total, 0);
        if !later {
            admit(&buyer, &seller, &next).await;
        }
        assert_eq!(row(&seller, &next).terms.unwrap().upfront, 0);
        assert_eq!(Chain::count(&chain.closes), 0);
    }
}

#[tokio::test]
async fn an_identical_request_sent_before_the_seller_has_the_previous_payment_waits() {
    let (_bd, _sd, chain, buyer, seller) = pair();
    let id = uuid::Uuid::new_v4().to_string();
    admit(&buyer, &seller, &id).await;
    // A bill of 24 exceeds the credit of 10, so it needs a new voucher.
    terminal(&seller, "seller", &id, &usage(20, 4)).await;
    terminal(&buyer, "buyer", &id, &usage(20, 4)).await;
    deliver(&seller, &buyer).await;
    assert_eq!(row(&buyer, &id).state, State::Closed);
    // The paying voucher reaches the seller only after the next hello, for
    // the same body, which waits for it.
    let voucher: Vec<_> = buyer.outbox.lock().unwrap().drain(..).collect();
    assert_eq!(voucher.len(), 1);
    let next = uuid::Uuid::new_v4().to_string();
    let task = hello(&buyer, &seller, &next).await;
    assert!(matches!(
        &seller.outbox.lock().unwrap()[..],
        [(_, Message::Mpp { message, .. })] if matches!(message.as_ref(), Wire::Busy)
    ));
    deliver(&seller, &buyer).await;
    assert!(row(&buyer, &next).busy);
    for (_, message) in voucher {
        seller.message(&buyer.node, message).await.unwrap();
    }
    let decision = settle(&buyer, &seller, task).await;
    assert_eq!(
        decision.decision,
        OpenAiAdmissionDecision::Allow,
        "{:?}",
        decision.reason
    );
    assert_eq!(row(&seller, &next).terms.unwrap().cost_before, 24);
    assert_eq!(Chain::count(&chain.opens), 1);
}

#[tokio::test]
async fn a_covered_bill_delivered_after_the_next_quote_does_not_recover_the_channel() {
    let (_bd, _sd, chain, buyer, seller) = pair();
    let id = uuid::Uuid::new_v4().to_string();
    admit(&buyer, &seller, &id).await;
    // The upfront credit of 10 covers the bill of 5, so the seller is done.
    terminal(&seller, "seller", &id, &usage(1, 4)).await;
    assert_eq!(row(&seller, &id).state, State::Closed);
    let bill: Vec<_> = seller.outbox.lock().unwrap().drain(..).collect();
    terminal(&buyer, "buyer", &id, &usage(1, 4)).await;
    let next = uuid::Uuid::new_v4().to_string();
    let task = hello(&buyer, &seller, &next).await;
    // The quote, which counts the bill, overtakes it.
    deliver(&seller, &buyer).await;
    let waiting = row(&buyer, &next);
    assert_eq!(waiting.state, State::Negotiating);
    assert_eq!(waiting.terms.unwrap().cost_before, 5);
    assert_eq!(channel(&buyer).state, ChannelState::Open);
    for (_, message) in bill {
        buyer.message(&seller.node, message).await.unwrap();
    }
    let decision = settle(&buyer, &seller, task).await;
    assert_eq!(
        decision.decision,
        OpenAiAdmissionDecision::Allow,
        "{:?}",
        decision.reason
    );
    let paid = row(&buyer, &id);
    assert_eq!((paid.state, paid.error), (State::Closed, None));
    let open = channel(&buyer);
    assert_eq!((open.state, open.running_total), (ChannelState::Open, 5));
    assert_eq!(Chain::count(&chain.recovers), 0);
}

#[tokio::test]
async fn a_node_can_buy_from_and_sell_to_the_same_peer_at_once() {
    let (_bd, _sd, _chain, buyer, seller) = pair();
    // The buyer also sells the model, and the seller also buys it.
    buyer
        .ledger
        .set_pricing("model", Some(&price(1_000_000)))
        .unwrap();
    seller.ledger.set_policy(&automatic(10000)).unwrap();
    let reverse = uuid::Uuid::new_v4().to_string();
    let intent = request_intent(&event(&reverse), &seller.wallet_info.address).unwrap();
    buyer
        .message(&seller.node, mpp(&reverse, Wire::Hello { intent }))
        .await
        .unwrap();
    buyer.outbox.lock().unwrap().clear();
    assert!(row(&buyer, &reverse).seller);
    // With that seller request open, the node still buys from the peer.
    let id = uuid::Uuid::new_v4().to_string();
    admit(&buyer, &seller, &id).await;
    let roles: Vec<_> = buyer
        .ledger
        .mpp_active()
        .unwrap()
        .iter()
        .map(|r| (r.peer.clone(), r.seller))
        .collect();
    assert_eq!(
        roles,
        [(seller.node.clone(), true), (seller.node.clone(), false)]
    );
    // And the peer, serving that request, buys from the node in turn.
    buyer
        .ledger
        .mpp_update(&reverse, |r| {
            r.state = State::Closed;
            Ok(())
        })
        .unwrap();
    let mut request = event(&uuid::Uuid::new_v4().to_string());
    request["target_peer_id"] = json!(buyer.node);
    let decision = decide(&seller, &buyer, request).await;
    assert_eq!(
        decision.decision,
        OpenAiAdmissionDecision::Allow,
        "{:?}",
        decision.reason
    );
    assert_eq!(row(&seller, &id).state, State::Running);
}
