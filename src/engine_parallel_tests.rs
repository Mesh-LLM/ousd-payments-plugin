//! Requests that run at once on one pair: headroom and top-ups before
//! dispatch, standing, bills accepted in order, and strikes.
use super::sequence::deliver;
use super::*;
use crate::control::Control;

/// A request like [`event`] whose body says `content`, so its digest differs.
fn event_saying(tag: &str, content: &str) -> Value {
    let mut request = event(tag);
    request["body"]["messages"][0]["content"] = json!(content);
    request
}
/// Starts the buyer's admission hook for `request`.
fn start(
    buyer: &Arc<Engine>,
    request: &Value,
) -> tokio::task::JoinHandle<Result<OpenAiExchangeDecision>> {
    let buyer = buyer.clone();
    let request = request.clone();
    tokio::spawn(async move { buyer.hook(request).await })
}
/// Delivers messages, and runs retry ticks when `tick`, until every hook has
/// decided; then dispatches each request at the seller.
async fn dispatch_all(
    buyer: &Arc<Engine>,
    seller: &Arc<Engine>,
    requests: &[(&str, Value)],
    tasks: Vec<tokio::task::JoinHandle<Result<OpenAiExchangeDecision>>>,
    tick: bool,
) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !tasks.iter().all(|t| t.is_finished()) {
            pump(buyer, seller).await.unwrap();
            if tick {
                buyer.retry().await.unwrap();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    pump(buyer, seller).await.unwrap();
    for ((tag, request), task) in requests.iter().zip(tasks) {
        let decision = task.await.unwrap().unwrap();
        assert_eq!(
            decision.decision,
            OpenAiAdmissionDecision::Allow,
            "{:?}",
            decision.reason
        );
        let mut serving = request.clone();
        serving["target_peer_id"] = Value::Null;
        serving["requested_by_node_id"] = json!(buyer.node);
        serving["observation_id"] = json!(format!("seller-{tag}"));
        assert_eq!(
            seller.hook(serving).await.unwrap().decision,
            OpenAiAdmissionDecision::Allow
        );
    }
}
/// Admits `requests` at once and dispatches them at the seller.
async fn admit_all(buyer: &Arc<Engine>, seller: &Arc<Engine>, requests: &[(&str, Value)]) {
    let tasks = requests.iter().map(|(_, r)| start(buyer, r)).collect();
    dispatch_all(buyer, seller, requests, tasks, false).await;
}
fn ids<const N: usize>() -> [String; N] {
    std::array::from_fn(|_| uuid::Uuid::new_v4().to_string())
}
/// The seller's answer to a hello for a new request saying `content`.
async fn answer(buyer: &Engine, seller: &Engine, content: &str) -> Wire {
    let id = uuid::Uuid::new_v4().to_string();
    let request = event_saying(&id, content);
    let intent = request_intent(&request, &buyer.wallet_info.address).unwrap();
    seller
        .message(&buyer.node, mpp(&id, Wire::Hello { intent }))
        .await
        .unwrap();
    let sent: Vec<_> = seller.outbox.lock().unwrap().drain(..).collect();
    match sent.last() {
        Some((_, Message::Mpp { message, .. })) => message.as_ref().clone(),
        other => panic!("no answer: {other:?}"),
    }
}
/// A warm channel with three dispatched requests whose bills of 24 the
/// buyer has not received; returns their tags.
async fn three_unpaid(buyer: &Arc<Engine>, seller: &Arc<Engine>) -> [String; 3] {
    paid_request(buyer, seller, usage(1, 4)).await;
    let tags: [String; 3] = ids();
    let requests: Vec<_> = tags
        .iter()
        .map(|t| (t.as_str(), event_saying(t, t)))
        .collect();
    admit_all(buyer, seller, &requests).await;
    for tag in &tags {
        terminal(seller, "seller", tag, &usage(20, 4)).await;
    }
    seller.outbox.lock().unwrap().clear();
    tags
}

#[tokio::test]
async fn requests_with_different_bodies_run_at_once_and_bills_settle_in_order() {
    let (_bd, _sd, chain, buyer, seller) = pair();
    paid_request(&buyer, &seller, usage(1, 4)).await;
    let [a, b] = ids();
    let requests = [
        (a.as_str(), event_saying(&a, "a")),
        (b.as_str(), event_saying(&b, "b")),
    ];
    admit_all(&buyer, &seller, &requests).await;
    for tag in [&a, &b] {
        let sold = row(&seller, tag);
        assert_eq!(
            (sold.state, sold.terms.unwrap().upfront),
            (State::Running, 0)
        );
    }
    terminal(&buyer, "buyer", &a, &usage(20, 4)).await;
    terminal(&buyer, "buyer", &b, &usage(30, 4)).await;
    // The later bill reaches the buyer first and waits for the earlier one.
    terminal(&seller, "seller", &a, &usage(20, 4)).await;
    let first: Vec<_> = seller.outbox.lock().unwrap().drain(..).collect();
    terminal(&seller, "seller", &b, &usage(30, 4)).await;
    deliver(&seller, &buyer).await;
    let held = row(&buyer, &b);
    assert_eq!((held.state, held.bill.unwrap().seq), (State::Running, 2));
    assert_eq!(channel(&buyer).bills, 1);
    for (_, message) in first {
        buyer.message(&seller.node, message).await.unwrap();
    }
    pump(&buyer, &seller).await.unwrap();
    for tag in [&a, &b] {
        let (bought, sold) = (row(&buyer, tag), row(&seller, tag));
        assert_eq!((bought.state, sold.state), (State::Closed, State::Closed));
        assert_eq!((bought.error, sold.error), (None, None));
    }
    let open = channel(&buyer);
    assert_eq!((open.cum(), open.running_total, open.bills), (63, 63, 3));
    assert_eq!(channel(&seller).cum(), 63);
    assert_eq!(Chain::count(&chain.recovers), 0);
}

#[tokio::test]
async fn a_bill_between_another_requests_quote_and_voucher_does_not_refuse_it() {
    for reordered in [false, true] {
        let (_bd, _sd, chain, buyer, seller) = pair();
        paid_request(&buyer, &seller, usage(1, 4)).await;
        let [a, b] = ids();
        admit_all(&buyer, &seller, &[(a.as_str(), event_saying(&a, "a"))]).await;
        let request = event_saying(&b, "b");
        let task = start(&buyer, &request);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while buyer.outbox.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // The seller quotes b, then bills a before b's voucher arrives.
        deliver(&buyer, &seller).await;
        terminal(&seller, "seller", &a, &usage(20, 4)).await;
        terminal(&buyer, "buyer", &a, &usage(20, 4)).await;
        assert_eq!(row(&seller, &b).terms.unwrap().cost_before, 5);
        assert_eq!(channel(&seller).running_total, 29);
        // The bill may also overtake the quote, which then shows fewer bills
        // than the buyer accepted.
        if reordered {
            let mut sent: Vec<_> = seller.outbox.lock().unwrap().drain(..).collect();
            sent.reverse();
            for (_, message) in sent {
                buyer.message(&seller.node, message).await.unwrap();
            }
        }
        dispatch_all(&buyer, &seller, &[(b.as_str(), request)], vec![task], false).await;
        assert_eq!(row(&buyer, &a).state, State::Closed);
        let open = channel(&buyer);
        assert_eq!((open.state, open.running_total), (ChannelState::Open, 29));
        assert_eq!(Chain::count(&chain.recovers), 0);
    }
}

#[tokio::test]
async fn a_refused_voucher_gives_no_standing() {
    let (_bd, _sd, chain, buyer, seller) = pair();
    let id = uuid::Uuid::new_v4().to_string();
    let intent = request_intent(&event_saying(&id, "a"), &buyer.wallet_info.address).unwrap();
    seller
        .message(&buyer.node, mpp(&id, Wire::Hello { intent }))
        .await
        .unwrap();
    // A real channel and a valid voucher of 1, below the quoted upfront of 10.
    let terms = ChannelTerms {
        buyer_peer: buyer.node.clone(),
        seller_peer: seller.node.clone(),
        payer: buyer.wallet_info.address.clone(),
        payee: seller.wallet_info.address.clone(),
        asset: seller.wallet_info.asset.clone(),
        nonce: "01".repeat(32),
    };
    let open = Request::Open {
        terms,
        deposit: 200,
        max_fee: 0,
    };
    let funding = chain.apply(open).unwrap().funding.unwrap();
    let voucher = Voucher {
        channel_id: funding.channel_id.clone(),
        cum: 1,
        signature: "signed-1".into(),
    };
    let refused = seller
        .message(&buyer.node, mpp(&id, Wire::Voucher { funding, voucher }))
        .await;
    assert!(refused.unwrap_err().to_string().contains("does not cover"));
    seller
        .message(&buyer.node, mpp(&id, Wire::Cancel))
        .await
        .unwrap();
    seller.outbox.lock().unwrap().clear();
    match answer(&buyer, &seller, "b").await {
        Wire::Quote { terms } => assert_eq!(terms.upfront, 10),
        other => panic!("not quoted: {other:?}"),
    }
}

#[tokio::test]
async fn bills_of_concurrent_requests_are_paid_beyond_the_daily_budget() {
    let (_bd, _sd, _chain, buyer, seller) = pair();
    paid_request(&buyer, &seller, usage(1, 4)).await;
    let [a, b] = ids();
    let requests = [
        (a.as_str(), event_saying(&a, "a")),
        (b.as_str(), event_saying(&b, "b")),
    ];
    // Each worst case fits what remains of the budget, but not both.
    let worst = request_intent(&requests[0].1, "").unwrap().input_bytes + 4;
    let budget = 10 + worst;
    buyer.ledger.set_policy(&automatic(budget)).unwrap();
    admit_all(&buyer, &seller, &requests).await;
    for tag in [&a, &b] {
        finish(&buyer, &seller, tag, usage(60, 4), false).await;
        let (bought, sold) = (row(&buyer, tag), row(&seller, tag));
        assert_eq!((bought.state, bought.error), (State::Closed, None));
        assert_eq!(sold.state, State::Closed);
    }
    assert_eq!(buyer.ledger.spent_today().unwrap(), 133);
    assert!(133 > budget);
}

#[tokio::test]
async fn a_concurrent_bill_beyond_the_deposit_waits_for_one_top_up() {
    let (_bd, _sd, chain, buyer, seller) = pair();
    for _ in 0..2 {
        paid_request(&buyer, &seller, usage(60, 4)).await;
    }
    let [a, b] = ids();
    let requests = [
        (a.as_str(), event_saying(&a, "a")),
        (b.as_str(), event_saying(&b, "b")),
    ];
    admit_all(&buyer, &seller, &requests).await;
    assert_eq!(Chain::count(&chain.topups), 0);
    // The first bill of 64 still fits the deposit of 200.
    finish(&buyer, &seller, &a, usage(60, 4), false).await;
    assert_eq!(channel(&buyer).deposit, 200);
    // The second does not, so it waits while the top-up is pending.
    chain.pending.store(true, Ordering::SeqCst);
    finish(&buyer, &seller, &b, usage(60, 4), false).await;
    let waiting = row(&buyer, &b);
    assert_eq!(
        (waiting.state, waiting.bill.is_some()),
        (State::Running, true)
    );
    assert_eq!(channel(&buyer).running_total, 192);
    chain.pending.store(false, Ordering::SeqCst);
    buyer.retry().await.unwrap();
    pump(&buyer, &seller).await.unwrap();
    assert_eq!(row(&seller, &b).state, State::Closed);
    assert_eq!(Chain::count(&chain.topups), 1);
    let topped = channel(&buyer);
    assert_eq!((topped.cum(), topped.deposit), (256, 400));
    assert_eq!(channel(&seller).deposit, 400);
}

#[tokio::test]
async fn first_requests_without_standing_open_one_channel_and_the_rest_need_no_upfront() {
    let (_bd, _sd, chain, buyer, seller) = pair();
    let [a, b] = ids();
    let requests = [
        (a.as_str(), event_saying(&a, "a")),
        (b.as_str(), event_saying(&b, "b")),
    ];
    admit_all(&buyer, &seller, &requests).await;
    let mut upfronts = [&a, &b].map(|t| row(&seller, t).terms.unwrap().upfront);
    upfronts.sort();
    assert_eq!(upfronts, [0, 10]);
    assert_eq!(Chain::count(&chain.opens), 1);
    assert_eq!(channel(&buyer).cum(), 10);
}

#[tokio::test]
async fn a_buyer_with_standing_opens_its_next_channel_with_a_zero_voucher_until_a_restart() {
    let (_bd, sd, chain, buyer, mut seller) = pair();
    for (upfront, restart) in [(10, false), (0, false), (10, true)] {
        if restart {
            drop(seller);
            seller = engine(sd.path(), true, chain.clone());
        }
        let id = uuid::Uuid::new_v4().to_string();
        admit(&buyer, &seller, &id).await;
        // The seller verified a voucher of just the upfront, zero with standing.
        let ready = row(&seller, &id);
        assert_eq!(ready.terms.unwrap().upfront, upfront);
        assert_eq!(ready.cum_after, Some(upfront));
        finish(&buyer, &seller, &id, usage(1, 4), false).await;
        assert_eq!(row(&seller, &id).state, State::Closed);
        // The seller closes the idle channel, so the next request opens one.
        let key = channel(&seller).key;
        seller
            .ledger
            .channel_update(&key, |c| {
                c.active_at = now() - 3600;
                Ok(())
            })
            .unwrap();
        seller.retry().await.unwrap();
        pump(&buyer, &seller).await.unwrap();
        assert!(
            buyer
                .ledger
                .channel_active(&seller.node, false)
                .unwrap()
                .is_none()
        );
    }
    assert_eq!(Chain::count(&chain.opens), 3);
}

#[tokio::test]
async fn an_identical_body_waits_while_the_seller_has_it_in_flight() {
    let (_bd, _sd, _chain, buyer, seller) = pair();
    let id = uuid::Uuid::new_v4().to_string();
    admit(&buyer, &seller, &id).await;
    assert!(matches!(answer(&buyer, &seller, "test").await, Wire::Busy));
    assert!(matches!(
        answer(&buyer, &seller, "other").await,
        Wire::Quote { .. }
    ));
}

#[tokio::test]
async fn three_unpaid_bills_make_the_seller_busy_until_one_is_paid() {
    let (_bd, _sd, _chain, buyer, seller) = pair();
    let tags = three_unpaid(&buyer, &seller).await;
    assert_eq!(
        seller
            .seller_strikes(&buyer.node, Some(&channel(&seller)))
            .unwrap(),
        3
    );
    assert!(matches!(answer(&buyer, &seller, "next").await, Wire::Busy));
    // The buyer knows its usage for the first request only, so it pays that bill.
    terminal(&buyer, "buyer", &tags[0], &usage(20, 4)).await;
    seller.retry().await.unwrap();
    pump(&buyer, &seller).await.unwrap();
    assert_eq!(row(&seller, &tags[0]).state, State::Closed);
    assert_eq!(row(&buyer, &tags[1]).state, State::Running);
    assert!(matches!(
        answer(&buyer, &seller, "next").await,
        Wire::Quote { .. }
    ));
}

#[tokio::test]
async fn lost_bills_count_on_a_new_channel_until_the_operator_unblocks_the_buyer() {
    let (_bd, _sd, chain, buyer, seller) = pair();
    let tags = three_unpaid(&buyer, &seller).await;
    for tag in &tags {
        seller
            .ledger
            .mpp_update(&rid(tag), |r| {
                r.billed_at = now() - channel::BILL_PAYMENT_SECONDS;
                Ok(())
            })
            .unwrap();
    }
    let key = channel(&seller).key;
    seller.retry().await.unwrap();
    let closed = seller.ledger.channel_get(&key).unwrap().unwrap();
    assert_eq!((closed.state, closed.lost_bills), (ChannelState::Closed, 3));
    assert_eq!(Chain::count(&chain.closes), 1);
    pump(&buyer, &seller).await.unwrap();
    assert!(matches!(answer(&buyer, &seller, "next").await, Wire::Busy));
    let unblock = |peer: &str| {
        serde_json::from_value::<Control>(json!({"command": "unblock", "peer": peer})).unwrap()
    };
    let forgiven = unblock(&buyer.node).run(&seller, None).await.unwrap();
    assert_eq!(forgiven, json!({"peer": buyer.node, "bills": 3}));
    assert!(unblock(&buyer.node).run(&seller, None).await.is_err());
    // The forgiven bills stay recorded on the closed channel.
    assert_eq!(
        seller.ledger.channel_get(&key).unwrap().unwrap().lost_bills,
        3
    );
    match answer(&buyer, &seller, "next").await {
        Wire::Quote { terms } => assert_eq!((terms.upfront, terms.channel_id), (0, None)),
        other => panic!("not quoted: {other:?}"),
    }
}

#[tokio::test]
async fn usage_after_the_unpaid_close_started_is_never_billed() {
    let (_bd, _sd, _chain, buyer, seller) = pair();
    paid_request(&buyer, &seller, usage(1, 4)).await;
    let [a, b] = ids();
    let requests = [
        (a.as_str(), event_saying(&a, "a")),
        (b.as_str(), event_saying(&b, "b")),
    ];
    admit_all(&buyer, &seller, &requests).await;
    terminal(&seller, "seller", &a, &usage(20, 4)).await;
    seller.outbox.lock().unwrap().clear();
    seller
        .ledger
        .mpp_update(&rid(&a), |r| {
            r.billed_at = now() - channel::BILL_PAYMENT_SECONDS;
            Ok(())
        })
        .unwrap();
    let key = channel(&seller).key;
    seller.mpp_retry_one(&rid(&a)).await.unwrap();
    assert_eq!(channel(&seller).state, ChannelState::Closing);
    // The capture is fixed, so b's usage is recorded but not billed.
    terminal(&seller, "seller", &b, &usage(20, 4)).await;
    let unbilled = row(&seller, &b);
    assert_eq!((unbilled.state, unbilled.bill), (State::Closed, None));
    assert!(unbilled.error.unwrap().contains("began closing"));
    seller.retry().await.unwrap();
    let closed = seller.ledger.channel_get(&key).unwrap().unwrap();
    assert_eq!((closed.lost_bills, closed.running_total), (1, 29));
}

#[tokio::test]
async fn a_bill_the_voucher_covers_is_not_lost_when_the_buyer_withdraws_first() {
    let (_bd, _sd, chain, buyer, seller) = pair();
    let id = uuid::Uuid::new_v4().to_string();
    admit(&buyer, &seller, &id).await;
    // The upfront credit covers the bill, but the buyer never receives it.
    seller.unreachable.store(true, Ordering::SeqCst);
    let finished = json!({"phase":"exchange_finished","observation_id":format!("seller-{id}"),"usage":usage(1, 4)});
    seller.record_terminal(&finished).unwrap();
    assert_eq!(row(&seller, &id).state, State::Billed);
    let open = channel(&seller);
    chain
        .channels
        .lock()
        .unwrap()
        .get_mut(&open.funding.clone().unwrap().channel_id)
        .unwrap()
        .close("fixture-withdraw", 0);
    // The close notice cannot be sent either; the close is still recorded.
    assert!(seller.retry().await.is_err());
    let closed = seller.ledger.channel_get(&open.key).unwrap().unwrap();
    assert_eq!((closed.state, closed.lost_bills), (ChannelState::Closed, 0));
}
