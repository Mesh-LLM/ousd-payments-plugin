//! Payment controls against the in-memory engine pair.
use super::*;
use crate::control::Control;

async fn control(engine: &Engine, input: Value) -> Result<Value> {
    serde_json::from_value::<Control>(input)?
        .run(engine, None)
        .await
}

#[tokio::test]
async fn set_pricing_re_registers_the_model_and_drops_its_deposit() {
    let (_bd, _sd, _chain, _buyer, seller) = pair();
    let priced = json!({"command":"set_pricing","model":"other",
        "value":{"input_per_million":"5","output_per_million":"6"}});
    control(&seller, priced).await.unwrap();
    let override_deposit =
        json!({"command":"set_model_admission_deposit","model":"other","value":"3"});
    let deposits = control(&seller, override_deposit).await.unwrap();
    assert_eq!(deposits["models"], json!({"other":"3"}));
    let discount = json!({"command":"set_cache_discount","model":"other","percent":40});
    let pricing = control(&seller, discount).await.unwrap();
    assert_eq!(pricing["prices"]["other"]["cache_discount_percent"], 40);
    assert_eq!(pricing["prices"]["other"]["output_per_million"], "6");
    let removed = control(&seller, json!({"command":"set_pricing","model":"other"}))
        .await
        .unwrap();
    assert!(removed["prices"].get("other").is_none());
    assert_eq!(removed["models"], json!({}));
    assert_eq!(removed["admission_deposit"], "10");
    let requirements = seller.requirements.lock().unwrap().clone();
    let expected = [("other", true), ("other", true), ("other", false)];
    assert_eq!(
        requirements,
        expected.map(|(model, paid)| (model.to_string(), paid))
    );
    // An unpriced model gets no discount and no requirement change.
    let unpriced = json!({"command":"set_cache_discount","model":"other","percent":40});
    assert!(control(&seller, unpriced).await.is_err());
    assert_eq!(seller.requirements.lock().unwrap().len(), 3);
    // A free price withdraws the requirement too.
    seller.set_pricing("model", Some(&price(0))).await.unwrap();
    assert_eq!(
        seller.requirements.lock().unwrap().last().unwrap(),
        &("model".to_string(), false)
    );
}

#[tokio::test]
async fn policy_and_admission_deposits_are_shown_and_set() {
    let (_bd, _sd, _chain, buyer, _seller) = pair();
    let policy = control(&buyer, json!({"command":"policy"})).await.unwrap();
    assert_eq!(policy["mode"], "automatic");
    assert_eq!(policy["daily_budget"], "10000");
    assert_eq!(policy["spent_today"], "0");
    let set = json!({"command":"policy","value":{"mode":"automatic","daily_budget":"30"}});
    let policy = control(&buyer, set).await.unwrap();
    assert_eq!(policy["remaining_daily_budget"], "30");
    let invalid = json!({"command":"policy","value":{"mode":"automatic"}});
    assert!(control(&buyer, invalid).await.is_err());
    assert_eq!(buyer.ledger.policy().unwrap(), automatic(30));
    let free = json!({"command":"policy","value":{"mode":"free_only"}});
    let policy = control(&buyer, free).await.unwrap();
    assert_eq!(policy["remaining_daily_budget"], "0");
    let set = json!({"command":"admission_deposit","value":"25"});
    let deposits = control(&buyer, set).await.unwrap();
    assert_eq!(deposits["admission_deposit"], "25");
    let both = json!({"command":"admission_deposit","value":"25","reset":true});
    assert!(control(&buyer, both).await.is_err());
    let reset = json!({"command":"admission_deposit","reset":true});
    let deposits = control(&buyer, reset).await.unwrap();
    assert_eq!(deposits["admission_deposit"], "0");
}

#[tokio::test]
async fn fund_names_the_wallet_and_pending_and_transactions_list_state() {
    let (_bd, _sd, _chain, buyer, seller) = pair();
    let fund = control(&buyer, json!({"command":"fund"})).await.unwrap();
    assert_eq!(
        fund,
        json!({"address":format!("0x{}", "11".repeat(20)),"network":"testnet",
            "asset":{"chain_id":42431,"token":crate::contract::OUSD,"decimals":6}})
    );
    let id = uuid::Uuid::new_v4().to_string();
    admit(&buyer, &seller, &id).await;
    let pending = control(&buyer, json!({"command":"pending"})).await.unwrap();
    assert_eq!(pending["requests"][0]["id"], rid(&id));
    assert_eq!(pending["channels"].as_array().unwrap().len(), 1);
    // Voucher signatures never appear in control output.
    assert_eq!(
        pending["channels"][0]["voucher"],
        json!({"channel_id": pending["channels"][0]["funding"]["channel_id"], "cum": "10"})
    );
    finish(&buyer, &seller, &id, usage(1, 4), false).await;
    let history = json!({"command":"transactions","limit":1});
    let transactions = control(&buyer, history).await.unwrap();
    assert_eq!(transactions["requests"][0]["state"], "closed");
    assert_eq!(transactions["channels"][0]["running_total"], "5");
    let pending = control(&buyer, json!({"command":"pending"})).await.unwrap();
    assert_eq!(pending["requests"], json!([]));
    let too_many = json!({"command":"transactions","limit":101});
    assert!(control(&buyer, too_many).await.is_err());
    let send = json!({"command":"send","operation_id":id,"to":format!("0x{}", "22".repeat(20)),"amount":"1","max_fee":"1"});
    let refused = control(&buyer, send).await.unwrap_err();
    assert!(refused.to_string().contains("unavailable"));
}

#[tokio::test]
async fn a_requirement_the_host_refused_is_sent_again_on_the_next_tick() {
    let (_bd, _sd, _chain, _buyer, seller) = pair();
    seller.unreachable.store(true, Ordering::SeqCst);
    let priced = json!({"command":"set_pricing","model":"other",
        "value":{"input_per_million":"5","output_per_million":"6"}});
    assert!(control(&seller, priced).await.is_err());
    assert!(seller.ledger.paid_price("other").unwrap().is_some());
    assert!(seller.requirements.lock().unwrap().is_empty());
    assert!(seller.retry().await.is_err());
    seller.unreachable.store(false, Ordering::SeqCst);
    seller.retry().await.unwrap();
    seller.retry().await.unwrap();
    assert_eq!(
        seller.requirements.lock().unwrap().clone(),
        [("other".to_string(), true)]
    );
}
