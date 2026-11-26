//! Buyer budget, cap and channel-view checks against the in-memory engine pair.
use super::*;

#[tokio::test]
async fn daily_budget_counts_voucher_increments_and_bounds_the_worst_case() {
    let (_bd, _sd, chain, buyer, seller) = pair();
    // The first request signs the upfront of 10 and could need a worst-case
    // bill of W; the second has 5 of credit left, so W + 5 fits both.
    let w = worst(&uuid::Uuid::nil().to_string());
    buyer.ledger.set_policy(&automatic(w + 5)).unwrap();
    paid_request(&buyer, &seller, usage(1, 4)).await;
    paid_request(&buyer, &seller, usage(1, 4)).await;
    assert_eq!(buyer.ledger.spent_today().unwrap(), 10);
    // A third worst-case bill would raise the voucher by W, beyond what remains.
    let id = uuid::Uuid::new_v4().to_string();
    let decision = decide(&buyer, &seller, event(&id)).await;
    assert_eq!(decision.decision, OpenAiAdmissionDecision::Deny);
    assert!(
        decision
            .reason
            .unwrap()
            .contains("worst-case cost: daily budget")
    );
    assert_eq!(buyer.ledger.spent_today().unwrap(), 10);
    assert_eq!(channel(&buyer).cum(), 10);
    assert_eq!(Chain::count(&chain.signs), 1);
    assert_eq!(row(&seller, &id).state, State::Closed);
}

#[tokio::test]
async fn the_worst_case_must_fit_the_cap_before_anything_is_opened_or_signed() {
    let (_bd, _sd, chain, mut buyer, seller) = pair();
    let w = worst(&uuid::Uuid::nil().to_string());
    // Upfront 10 plus a worst-case bill's increment of W - 10 is W.
    Arc::get_mut(&mut buyer).unwrap().config.cap = w - 1;
    let decision = decide(&buyer, &seller, event(&uuid::Uuid::new_v4().to_string())).await;
    assert_eq!(decision.decision, OpenAiAdmissionDecision::Deny);
    assert!(
        decision
            .reason
            .unwrap()
            .contains("worst-case cost: payment exceeds the per-request cap")
    );
    assert_eq!(Chain::count(&chain.opens), 0);
    assert_eq!(Chain::count(&chain.signs), 0);
    Arc::get_mut(&mut buyer).unwrap().config.cap = w;
    admit(&buyer, &seller, &uuid::Uuid::new_v4().to_string()).await;
}

#[tokio::test]
async fn a_seller_behind_on_vouchers_gets_the_latest_without_another_upfront() {
    let (_bd, _sd, chain, buyer, seller) = pair();
    let id = paid_request(&buyer, &seller, usage(1, 4)).await;
    // A voucher for 12 that the seller never received.
    let open = channel(&buyer);
    let lost = Voucher {
        channel_id: open.funding.unwrap().channel_id,
        cum: 12,
        signature: "signed-12".into(),
    };
    buyer
        .ledger
        .mpp_update_both(&rid(&id), &open.key, 10000, |_, c| {
            c.voucher = Some(lost);
            Ok(())
        })
        .unwrap();
    let next = uuid::Uuid::new_v4().to_string();
    admit(&buyer, &seller, &next).await;
    let quote = row(&seller, &next).terms.unwrap();
    assert_eq!((quote.cum_before, quote.upfront), (10, 0));
    assert_eq!(channel(&buyer).state, ChannelState::Open);
    assert_eq!(channel(&seller).cum(), 12);
    assert_eq!(buyer.ledger.spent_today().unwrap(), 12);
    assert_eq!(Chain::count(&chain.signs), 1);
    assert_eq!(Chain::count(&chain.opens), 1);
}

#[tokio::test]
async fn a_lost_first_voucher_reuses_the_channel_the_seller_never_saw() {
    let (_bd, _sd, chain, buyer, seller) = pair();
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
    buyer.outbox.lock().unwrap().clear();
    assert_eq!(channel(&buyer).cum(), 10);
    buyer
        .ledger
        .mpp_update(&rid(&id), |r| {
            r.terms.as_mut().unwrap().expires_at = now() - 1;
            Ok(())
        })
        .unwrap();
    buyer.retry().await.unwrap();
    seller.mpp_close(&id, "expired").unwrap();
    let next = uuid::Uuid::new_v4().to_string();
    admit(&buyer, &seller, &next).await;
    let quote = row(&seller, &next).terms.unwrap();
    assert_eq!((quote.channel_id, quote.upfront), (None, 10));
    assert_eq!(channel(&seller).key, channel(&buyer).key);
    assert_eq!(channel(&seller).cum(), 10);
    assert_eq!(buyer.ledger.spent_today().unwrap(), 10);
    assert_eq!(Chain::count(&chain.signs), 1);
    assert_eq!(Chain::count(&chain.opens), 1);
}

#[tokio::test]
async fn a_seller_voucher_above_the_buyers_own_is_refused() {
    let (_bd, _sd, _chain, buyer, seller) = pair();
    paid_request(&buyer, &seller, usage(1, 4)).await;
    let open = channel(&seller);
    let forged = Voucher {
        channel_id: open.funding.clone().unwrap().channel_id,
        cum: 15,
        signature: "signed-15".into(),
    };
    seller
        .ledger
        .channel_update(&open.key, |c| {
            c.voucher = Some(forged);
            Ok(())
        })
        .unwrap();
    let decision = decide(&buyer, &seller, event(&uuid::Uuid::new_v4().to_string())).await;
    assert_eq!(decision.decision, OpenAiAdmissionDecision::Deny);
    assert!(
        decision
            .reason
            .unwrap()
            .contains("view of the channel differs")
    );
    assert_eq!(channel(&buyer).state, ChannelState::Closing);
}

/// Delivers a quote for a new request with its upfront replaced, as a
/// dishonest seller would send it, and returns the request and whether the
/// buyer answered with a voucher.
async fn tampered_quote(buyer: &Engine, seller: &Engine, upfront: u64) -> (String, bool) {
    let id = uuid::Uuid::new_v4().to_string();
    let intent = request_intent(&event(&id), &buyer.wallet_info.address).unwrap();
    buyer
        .ledger
        .mpp_insert(&attached(&id, &seller.node, intent.clone()))
        .unwrap();
    seller.mpp_hello(&buyer.node, &id, intent).await.unwrap();
    let quotes: Vec<_> = seller.outbox.lock().unwrap().drain(..).collect();
    for (_, mut message) in quotes {
        if let Message::Mpp { message, .. } = &mut message
            && let Wire::Quote { terms } = message.as_mut()
        {
            terms.upfront = upfront;
        }
        buyer.message(&seller.node, message).await.unwrap();
    }
    let voucher = buyer.outbox.lock().unwrap().iter().any(|(_, m)| {
        matches!(m, Message::Mpp { message, .. } if matches!(message.as_ref(), Wire::Voucher { .. }))
    });
    pump(buyer, seller).await.unwrap();
    (id, voucher)
}

#[tokio::test]
async fn a_seller_cannot_charge_the_upfront_again_but_may_refill_credit_to_the_limit() {
    let (_bd, _sd, chain, buyer, seller) = pair();
    // Voucher 10 and bills 5 leave 5 of credit; the admission deposit is 10.
    paid_request(&buyer, &seller, usage(1, 4)).await;
    for _ in 0..3 {
        let (id, voucher) = tampered_quote(&buyer, &seller, 10).await;
        assert!(!voucher);
        assert!(row(&buyer, &id).error.unwrap().contains("unbilled credit"));
        assert_eq!(row(&seller, &id).state, State::Closed);
    }
    assert_eq!(channel(&buyer).cum(), 10);
    assert_eq!(buyer.ledger.spent_today().unwrap(), 10);
    // An upfront of 5 brings the credit back to exactly 10.
    let (id, voucher) = tampered_quote(&buyer, &seller, 5).await;
    assert!(voucher);
    assert_eq!(channel(&buyer).cum(), 15);
    assert_eq!(channel(&seller).cum(), 15);
    assert_eq!(row(&seller, &id).state, State::Ready);
    assert_eq!(Chain::count(&chain.signs), 2);
}

#[tokio::test]
async fn the_seller_requires_a_voucher_covering_its_view_and_the_upfront() {
    let (_bd, _sd, _chain, buyer, seller) = pair();
    paid_request(&buyer, &seller, usage(1, 4)).await;
    let open = channel(&seller);
    let id = uuid::Uuid::new_v4().to_string();
    let intent = request_intent(&event(&id), &buyer.wallet_info.address).unwrap();
    seller.mpp_hello(&buyer.node, &id, intent).await.unwrap();
    seller.outbox.lock().unwrap().clear();
    // A voucher for the bills alone (5) is below the seller's view of 10.
    let stale = Voucher {
        channel_id: open.funding.clone().unwrap().channel_id,
        cum: 5,
        signature: "signed-5".into(),
    };
    let funding = open.funding.unwrap();
    let refused = seller
        .message(
            &buyer.node,
            mpp(
                &id,
                Wire::Voucher {
                    funding,
                    voucher: stale,
                },
            ),
        )
        .await
        .unwrap_err();
    assert!(
        refused
            .to_string()
            .contains("does not cover the quoted voucher")
    );
    assert_eq!(row(&seller, &id).state, State::Negotiating);
}

#[tokio::test]
async fn work_dispatched_before_a_switch_to_free_only_is_still_paid() {
    let (_bd, _sd, _chain, buyer, seller) = pair();
    paid_request(&buyer, &seller, usage(1, 4)).await;
    let id = uuid::Uuid::new_v4().to_string();
    admit(&buyer, &seller, &id).await;
    buyer.ledger.set_policy(&Policy::default()).unwrap();
    // 24 is more than the 5 of credit left, so the buyer must sign.
    finish(&buyer, &seller, &id, usage(20, 4), false).await;
    let paid = row(&buyer, &id);
    assert_eq!((paid.state, paid.error), (State::Closed, None));
    assert_eq!(row(&seller, &id).state, State::Closed);
    // New requests now go free.
    assert_eq!(channel(&buyer).cum(), 29);
    let decision = decide(&buyer, &seller, event(&uuid::Uuid::new_v4().to_string())).await;
    assert_eq!(decision.decision, OpenAiAdmissionDecision::Abstain);
}

#[tokio::test]
async fn a_prompt_beyond_the_body_bound_is_billed_at_the_worst_case() {
    let (_bd, _sd, chain, buyer, seller) = pair();
    let id = uuid::Uuid::new_v4().to_string();
    let w = worst(&id);
    admit(&buyer, &seller, &id).await;
    // W input tokens and 4 output tokens would charge W + 4; the seller
    // caps the bill at the worst case W, and the buyer pays it.
    finish(&buyer, &seller, &id, usage(w, 4), false).await;
    let paid = row(&buyer, &id);
    assert_eq!(paid.state, State::Closed);
    assert!(paid.error.is_none());
    assert_eq!(paid.bill.unwrap().cost, w);
    assert_eq!(channel(&buyer).state, ChannelState::Open);
    assert_eq!(channel(&buyer).cum(), w.max(10));
    assert_eq!(channel(&seller).running_total, w);
    assert_eq!(Chain::count(&chain.signs), 2);
}

#[tokio::test]
async fn a_bill_above_the_worst_case_is_refused() {
    let (_bd, _sd, chain, buyer, seller) = pair();
    let id = uuid::Uuid::new_v4().to_string();
    let w = worst(&id);
    admit(&buyer, &seller, &id).await;
    terminal(&buyer, "buyer", &id, &usage(w, 4)).await;
    let own = row(&buyer, &id).usage.unwrap();
    let bill = Bill {
        seq: 0,
        usage: Some(own),
        cost: w + 4,
        running_total: w + 4,
    };
    buyer
        .message(&seller.node, mpp(&rid(&id), Wire::Bill(bill)))
        .await
        .unwrap();
    let refused = row(&buyer, &id);
    assert!(refused.error.unwrap().contains("cost differs"));
    assert_eq!(channel(&buyer).cum(), 10);
    assert_eq!(Chain::count(&chain.signs), 1);
}

#[tokio::test]
async fn only_a_new_or_higher_voucher_is_verified_on_chain_at_admission() {
    let (_bd, _sd, chain, buyer, seller) = pair();
    paid_request(&buyer, &seller, usage(1, 4)).await;
    let verified = Chain::count(&chain.verifies);
    assert_eq!(verified, 1);
    // The next request presents the voucher the seller already stores.
    let id = paid_request(&buyer, &seller, usage(1, 4)).await;
    assert_eq!(row(&seller, &id).cum_after, Some(10));
    assert_eq!(Chain::count(&chain.verifies), verified);
    // A refill of the credit to 15 is a higher voucher.
    let (id, voucher) = tampered_quote(&buyer, &seller, 5).await;
    assert!(voucher);
    assert_eq!(row(&seller, &id).state, State::Ready);
    assert_eq!(Chain::count(&chain.verifies), verified + 1);
}
