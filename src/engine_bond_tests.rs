//! The published bond reference and a node without a wallet.
use super::*;

/// A seller using the network's escrow.
fn bonded() -> (tempfile::TempDir, Arc<Chain>, Arc<Engine>) {
    let dir = tempfile::tempdir().unwrap();
    let chain = Arc::new(Chain::default());
    let mut seller = engine(dir.path(), true, chain.clone());
    let escrow = crate::contract::Network::Testnet
        .bond_escrow()
        .map(Into::into);
    Arc::get_mut(&mut seller).unwrap().config.bonds.contract = escrow;
    (dir, chain, seller)
}
fn attempt(operation: &str, kind: &str, bond: &str, status: &str) -> Value {
    json!({"id":operation,"intent":{"kind":kind},"bond_id":bond,"status":status})
}
fn published(seller: &Engine) -> Vec<Option<String>> {
    let published = seller.published.lock().unwrap();
    published
        .iter()
        .map(|p| p.as_ref().map(|p| p.bond_id.clone()))
        .collect()
}

#[tokio::test]
async fn a_finalized_lock_is_published_once_and_withdrawn_with_its_bond() {
    let (_dir, chain, seller) = bonded();
    let (first, second) = (
        format!("0x{}", "aa".repeat(32)),
        format!("0x{}", "bb".repeat(32)),
    );
    chain
        .attempts
        .lock()
        .unwrap()
        .push(attempt("one", "lock", &first, "pending"));
    seller.retry().await.unwrap();
    assert!(published(&seller).is_empty());
    assert_eq!(seller.newest_bond().unwrap(), None);
    *chain.attempts.lock().unwrap() = vec![attempt("one", "lock", &first, "finalized")];
    seller.retry().await.unwrap();
    seller.retry().await.unwrap();
    assert_eq!(published(&seller), [Some(first.clone())]);
    // The operator withdraws the reference; the same lock is not published again.
    seller.set_published_bond(None).await.unwrap();
    seller.retry().await.unwrap();
    assert_eq!(published(&seller), [Some(first.clone()), None]);
    // A newer lock replaces it, and its withdrawal withdraws the reference.
    chain
        .attempts
        .lock()
        .unwrap()
        .push(attempt("two", "lock", &second, "finalized"));
    seller.retry().await.unwrap();
    chain
        .attempts
        .lock()
        .unwrap()
        .push(attempt("three", "withdraw", &second, "finalized"));
    seller.retry().await.unwrap();
    assert_eq!(
        published(&seller),
        [Some(first), None, Some(second.clone()), None]
    );
    // Exits default to the newest lock even after its reference is withdrawn.
    assert_eq!(seller.newest_bond().unwrap(), Some(second));
    assert_eq!(seller.ledger.published_bond().unwrap(), None);
}

#[tokio::test]
async fn an_exited_bond_is_withdrawn_once_its_delay_has_passed() {
    let (_dir, chain, seller) = bonded();
    let bond = format!("0x{}", "aa".repeat(32));
    let exit = |after: u64| json!({"id":"exit","intent":{"kind":"request_exit"},"bond_id":bond,"withdraw_after":after,"status":"finalized"});
    chain
        .attempts
        .lock()
        .unwrap()
        .push(exit(crate::config::now() + 60));
    seller.retry().await.unwrap();
    assert!(chain.bonds.lock().unwrap().is_empty());
    *chain.attempts.lock().unwrap() = vec![exit(crate::config::now())];
    seller.retry().await.unwrap();
    let withdrawal = (
        crate::bonds::exit_operation("withdraw", &bond).unwrap(),
        crate::bonds::Intent::Withdraw {
            bond_id: bond.clone(),
        },
    );
    assert_eq!(*chain.bonds.lock().unwrap(), [withdrawal]);
    // Once the withdrawal finalizes, the bond is not withdrawn again.
    chain
        .attempts
        .lock()
        .unwrap()
        .push(attempt("refund", "withdraw", &bond, "finalized"));
    seller.retry().await.unwrap();
    assert_eq!(chain.bonds.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_publication_the_host_refused_is_repeated_on_the_next_tick() {
    let (_dir, _chain, seller) = bonded();
    let bond = format!("0x{}", "aa".repeat(32));
    seller.unreachable.store(true, Ordering::SeqCst);
    assert!(seller.set_published_bond(Some(&bond)).await.is_err());
    assert!(seller.retry().await.is_err());
    seller.unreachable.store(false, Ordering::SeqCst);
    seller.retry().await.unwrap();
    seller.retry().await.unwrap();
    assert_eq!(published(&seller), [Some(bond)]);
    assert!(seller.set_published_bond(Some("not-a-bond")).await.is_err());
}

#[tokio::test]
async fn a_node_without_a_wallet_refuses_prices_and_creates_its_wallet_on_first_use() {
    let dir = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let network = crate::contract::Network::Testnet;
    let absent = crate::contract::WalletInfo::absent(network);
    let db = Ledger::open(state.path()).unwrap().db.clone();
    let unopened = Arc::new(crate::wallet::Unopened::new(
        Default::default(),
        state.path(),
        db,
    ));
    let node = engine_with_wallet(dir.path(), true, unopened.clone(), absent, 0);
    let refused = node.set_pricing("model", Some(&price(1))).await;
    assert!(refused.unwrap_err().to_string().contains("need a wallet"));
    node.set_pricing("model", Some(&price(0))).await.unwrap();
    let balance = node.wallet.balance().await.unwrap_err();
    assert!(balance.to_string().contains("no wallet yet"), "{balance}");
    // The retry tick has nothing to reconcile or publish.
    node.retry().await.unwrap();
    assert!(!state.path().join("wallet.json").exists());
    // The first `fund` or `bond_lock` creates an unlocked wallet with a
    // private random password.
    let wallet = unopened.open().await.unwrap();
    assert!(wallet.signer.is_some());
    let password = crate::tempo::keys::generated_password(state.path());
    crate::tempo::keys::private_file(&password).unwrap();
    assert!(state.path().join("wallet.json").exists());
    assert!(std::ptr::eq(wallet, unopened.open().await.unwrap()));
}
