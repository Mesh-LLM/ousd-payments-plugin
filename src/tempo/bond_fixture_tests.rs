//! Bond RPC fixtures use the real signer, native multicall encoding and durable journal.
use super::*;
use crate::bonds::{self, Escrow, Intent, Requirement, Settings as BondSettings};
use crate::tempo::bonds::BondAttempt;
use tempo_alloy::primitives::transaction::Call;
pub(super) const ESCROW: &str = "0x11111111111111111111111111111111111111ab";

pub(super) fn view(fixture: &Fixture, data: &[u8]) -> Result<Value> {
    if data[..4] == Escrow::tokenCall::SELECTOR {
        return Ok(abi(OUSD.parse::<Address>()?));
    }
    let call = Escrow::getBondCall::abi_decode_validate(data)?;
    let bond = fixture.bonds.get(&call.id).context("bond absent")?;
    Ok(json!(format!("0x{}", hex::encode(bond.abi_encode()))))
}

pub(super) fn execute(fixture: &mut Fixture, payer: Address, calls: &[Call]) -> Result<()> {
    if calls.len() == 2 {
        ensure!(
            calls[0].to == TxKind::Call(OUSD.parse()?)
                && calls[1].to == TxKind::Call(ESCROW.parse()?),
            "unexpected lock targets"
        );
        let approval = bonds::approveCall::abi_decode_validate(&calls[0].input)?;
        let lock = Escrow::lockCall::abi_decode_validate(&calls[1].input)?;
        ensure!(
            approval.spender == ESCROW.parse::<Address>()?
                && approval.amount == U256::from(lock.amount),
            "approval must match exact bond"
        );
        ensure!(
            lock.amount > U96::ZERO && lock.exitDelay > 0,
            "invalid lock"
        );
        let id = keccak256(
            (
                U256::from(fixture.chain_id),
                ESCROW.parse::<Address>()?,
                payer,
                lock.salt,
            )
                .abi_encode(),
        );
        ensure!(!fixture.bonds.contains_key(&id), "duplicate bond");
        fixture.bonds.insert(
            id,
            Escrow::Bond {
                meshId: lock.meshId,
                hostKey: lock.hostKey,
                refundWallet: payer,
                amount: lock.amount,
                exitDelay: lock.exitDelay,
                withdrawAt: U256::ZERO,
                withdrawn: false,
            },
        );
        *fixture.balances.entry(payer).or_insert(1_000_000) -= u64::try_from(lock.amount)?;
    } else {
        ensure!(
            calls.len() == 1 && calls[0].to == TxKind::Call(ESCROW.parse()?),
            "unexpected refund call"
        );
        if calls[0].input[..4] == Escrow::requestExitCall::SELECTOR {
            let exit = Escrow::requestExitCall::abi_decode_validate(&calls[0].input)?;
            let bond = fixture.bonds.get_mut(&exit.id).context("unknown bond")?;
            ensure!(
                bond.refundWallet == payer && !bond.withdrawn,
                "unauthorized exit"
            );
            if bond.withdrawAt == U256::ZERO {
                bond.withdrawAt = U256::from(fixture.now) + U256::from(bond.exitDelay);
            }
            return Ok(());
        }
        let withdraw = Escrow::withdrawCall::abi_decode_validate(&calls[0].input)?;
        let bond = fixture
            .bonds
            .get_mut(&withdraw.id)
            .context("unknown bond")?;
        ensure!(
            bond.refundWallet == payer
                && !bond.withdrawn
                && bond.withdrawAt != U256::ZERO
                && U256::from(fixture.now) >= bond.withdrawAt,
            "unauthorized or early refund"
        );
        bond.withdrawn = true;
        *fixture.balances.entry(payer).or_insert(1_000_000) += u64::try_from(bond.amount)?;
    }
    Ok(())
}
fn bond_attempt(wallet: &Wallet, id: &str) -> BondAttempt {
    wallet.journal.get(id).unwrap().unwrap()
}
fn settings() -> BondSettings {
    BondSettings {
        contract: Some(ESCROW.into()),
        max_lock: 100,
        max_exit_delay_seconds: 7200,
        providers: Requirement {
            minimum_amount: 50,
            minimum_exit_delay_seconds: 600,
        },
        ..Default::default()
    }
}
fn intent() -> Intent {
    Intent::Lock {
        mesh_id: "fixture-mesh".into(),
        host_key: "ab".repeat(32),
        amount: 100,
        exit_delay_seconds: 3600,
        salt: B256::repeat_byte(3).to_string(),
    }
}
fn op(n: u128) -> String {
    uuid::Uuid::from_u128(n).to_string()
}
async fn finalized_lock(wallet: &Wallet, settings: &BondSettings, intent: &Intent) -> Value {
    wallet.bond(settings, &op(1), intent.clone()).await.unwrap();
    wallet.bond(settings, &op(1), intent.clone()).await.unwrap()
}

#[tokio::test]
async fn operation_uuid_aliases_reuse_the_original_transaction() {
    let (state, url, server) = server().await;
    let dir = tempfile::tempdir().unwrap();
    let wallet = wallet(dir.path(), &url, 1);
    let settings = settings();
    let id = uuid::Uuid::from_u128(0xabcdef);
    let intent = intent();
    let original = wallet
        .bond(&settings, &id.to_string().to_uppercase(), intent.clone())
        .await
        .unwrap();
    for alias in [
        id.simple().to_string(),
        id.urn().to_string(),
        id.braced().to_string(),
    ] {
        let replay = wallet
            .bond(&settings, &alias, intent.clone())
            .await
            .unwrap();
        assert_eq!(replay["id"], id.to_string());
        assert_eq!(replay["hash"], original["hash"]);
    }
    assert_eq!(state.lock().unwrap().raws.len(), 1);
    assert_eq!(wallet.journal.all::<BondAttempt>().unwrap().len(), 1);
    server.abort();
}

#[tokio::test]
async fn advancing_finality_does_not_expire_an_included_transaction() {
    let (state, url, server) = server().await;
    let dir = tempfile::tempdir().unwrap();
    let wallet = wallet(dir.path(), &url, 1);
    let settings = settings();
    wallet.bond(&settings, &op(1), intent()).await.unwrap();
    let attempt = wallet
        .journal
        .unresolved::<BondAttempt>()
        .unwrap()
        .pop()
        .unwrap();
    {
        let mut fixture = state.lock().unwrap();
        fixture.finalized_height = 9;
        fixture.now = attempt.tx.expires_at - 1;
        fixture.next_finalized = Some((10, attempt.tx.expires_at));
    }
    wallet.reconcile().await.unwrap();
    assert_eq!(bond_attempt(&wallet, &op(1)).tx.status, "pending");
    wallet.reconcile().await.unwrap();
    assert_eq!(bond_attempt(&wallet, &op(1)).tx.status, "finalized");
    server.abort();
}

#[tokio::test]
async fn missing_receipts_with_a_consumed_finalized_nonce_stay_pending() {
    let (state, url, server) = server().await;
    let dir = tempfile::tempdir().unwrap();
    let wallet = wallet(dir.path(), &url, 1);
    let settings = settings();
    wallet.bond(&settings, &op(1), intent()).await.unwrap();
    let attempt = wallet
        .journal
        .unresolved::<BondAttempt>()
        .unwrap()
        .pop()
        .unwrap();
    {
        let mut fixture = state.lock().unwrap();
        fixture.now = attempt.tx.expires_at;
        fixture.hide_receipt = true;
    }
    wallet.reconcile().await.unwrap();
    assert_eq!(bond_attempt(&wallet, &op(1)).tx.status, "pending");
    assert!(wallet.bond(&settings, &op(2), intent()).await.is_err());
    state.lock().unwrap().hide_receipt = false;
    wallet.reconcile().await.unwrap();
    assert_eq!(bond_attempt(&wallet, &op(1)).tx.status, "finalized");
    assert_eq!(state.lock().unwrap().raws.len(), 1);
    server.abort();
}
fn proof(result: &Value) -> Value {
    json!({"chain_id":42431,"contract":ESCROW,"bond_id":result["bond_id"]})
}

#[tokio::test]
async fn idle_reconciliation_does_not_contact_the_chain() {
    let (state, url, server) = server().await;
    let dir = tempfile::tempdir().unwrap();
    let wallet = wallet(dir.path(), &url, 1);
    for _ in 0..3 {
        wallet.reconcile().await.unwrap();
    }
    assert!(state.lock().unwrap().calls.is_empty());
    server.abort();
}

#[tokio::test]
async fn without_an_escrow_no_bond_qualifies_and_no_chain_is_read() {
    let (state, url, server) = server().await;
    let dir = tempfile::tempdir().unwrap();
    let wallet = wallet(dir.path(), &url, 1);
    let mut policy = settings();
    policy.contract = None;
    let before = state.lock().unwrap().calls.len();
    for evidence in [json!({"bond":"uninspected"}), json!({"chain_id":4217})] {
        let decision = bonds::verify(
            &wallet.rpc,
            &policy,
            "fixture-mesh",
            &"ab".repeat(32),
            evidence,
        )
        .await
        .unwrap();
        assert_eq!(decision["accepted"], false);
        let far = crate::config::now() + 300 * 24 * 60 * 60;
        assert!(decision["valid_until"].as_u64().unwrap() > far);
    }
    assert_eq!(state.lock().unwrap().calls.len(), before);
    server.abort();
}

#[tokio::test]
async fn bond_state_verification_rejects_replays_withdrawals_wrong_code_and_stale_state() {
    let (state, url, server) = server().await;
    let dir = tempfile::tempdir().unwrap();
    let wallet = wallet(dir.path(), &url, 1);
    let settings = settings();
    let result = finalized_lock(&wallet, &settings, &intent()).await;
    assert_eq!(result["status"], "finalized");
    let reference = proof(&result);
    assert!(
        bonds::verify(
            &wallet.rpc,
            &settings,
            "fixture-mesh",
            &"ab".repeat(32),
            reference.clone(),
        )
        .await
        .is_ok()
    );
    for (mesh, host) in [
        ("other-mesh", "ab".repeat(32)),
        ("fixture-mesh", "ac".repeat(32)),
    ] {
        assert!(
            bonds::verify(&wallet.rpc, &settings, mesh, &host, reference.clone(),)
                .await
                .is_err()
        );
    }
    for (field, value) in [
        ("chain_id", json!(4217)),
        ("contract", json!(OUSD)),
        ("bond_id", json!("bad")),
    ] {
        let mut wrong = reference.clone();
        wrong[field] = value;
        assert!(
            bonds::verify(
                &wallet.rpc,
                &settings,
                "fixture-mesh",
                &"ab".repeat(32),
                wrong,
            )
            .await
            .is_err()
        );
    }
    state.lock().unwrap().bond_code_valid = false;
    assert!(
        bonds::verify(
            &wallet.rpc,
            &settings,
            "fixture-mesh",
            &"ab".repeat(32),
            reference.clone(),
        )
        .await
        .is_err()
    );
    state.lock().unwrap().bond_code_valid = true;
    state.lock().unwrap().now -= 121;
    assert!(
        bonds::verify(
            &wallet.rpc,
            &settings,
            "fixture-mesh",
            &"ab".repeat(32),
            reference.clone(),
        )
        .await
        .is_err()
    );
    state.lock().unwrap().now += 121;
    let id: B256 = result["bond_id"].as_str().unwrap().parse().unwrap();
    state.lock().unwrap().bonds.get_mut(&id).unwrap().amount = U96::from(49);
    assert!(
        bonds::verify(
            &wallet.rpc,
            &settings,
            "fixture-mesh",
            &"ab".repeat(32),
            reference.clone(),
        )
        .await
        .is_err()
    );
    state.lock().unwrap().bonds.get_mut(&id).unwrap().amount = U96::from(100);
    state.lock().unwrap().bonds.get_mut(&id).unwrap().exitDelay = 599;
    assert!(
        bonds::verify(
            &wallet.rpc,
            &settings,
            "fixture-mesh",
            &"ab".repeat(32),
            reference.clone(),
        )
        .await
        .is_err()
    );
    state.lock().unwrap().bonds.get_mut(&id).unwrap().exitDelay = 3600;
    state.lock().unwrap().bonds.get_mut(&id).unwrap().withdrawAt =
        U256::from(crate::config::now() + 3600);
    let deadline = crate::config::now() + 600;
    state.lock().unwrap().bonds.get_mut(&id).unwrap().withdrawAt = U256::from(deadline);
    let exiting = bonds::verify(
        &wallet.rpc,
        &settings,
        "fixture-mesh",
        &"ab".repeat(32),
        reference.clone(),
    )
    .await
    .unwrap();
    assert_eq!(exiting["valid_until"], deadline);
    state.lock().unwrap().bonds.get_mut(&id).unwrap().withdrawAt = U256::from(crate::config::now());
    assert!(
        bonds::verify(
            &wallet.rpc,
            &settings,
            "fixture-mesh",
            &"ab".repeat(32),
            reference.clone(),
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("withdrawable")
    );
    state.lock().unwrap().bonds.get_mut(&id).unwrap().withdrawn = true;
    assert!(
        bonds::verify(
            &wallet.rpc,
            &settings,
            "fixture-mesh",
            &"ab".repeat(32),
            reference,
        )
        .await
        .is_err()
    );
    server.abort();
}

#[tokio::test]
async fn bonds_and_payments_share_nonce_owner_and_restart_preserves_signed_bytes() {
    let (state, url, server) = server().await;
    let dir = tempfile::tempdir().unwrap();
    let sd = tempfile::tempdir().unwrap();
    let operator = wallet(dir.path(), &url, 1);
    let seller = wallet(sd.path(), &url, 2);
    let lock = intent();
    let mut settings = settings();
    {
        let mut state = state.lock().unwrap();
        state.lose_reply = true;
        state.hide_receipt = true;
    }
    let pending = operator
        .bond(&settings, &op(1), lock.clone())
        .await
        .unwrap();
    assert_eq!(pending["status"], "pending");
    assert!(pending.get("raw").is_none());
    let original = bond_attempt(&operator, &op(1));
    let agreed = terms(&operator, &seller, 1);
    assert!(operator.mpp(open_request(&agreed)).await.is_err());
    assert!(operator.balance().await.unwrap().reserved >= 100);
    drop(operator);
    let operator = wallet(dir.path(), &url, 1);
    settings.max_lock = 0;
    assert_eq!(
        operator
            .bond(&settings, &op(1), lock.clone())
            .await
            .unwrap()["status"],
        "pending"
    );
    assert_eq!(bond_attempt(&operator, &op(1)).tx.raw, original.tx.raw);
    state.lock().unwrap().hide_receipt = false;
    operator.reconcile().await.unwrap();
    assert_eq!(
        operator.bond(&settings, &op(1), lock).await.unwrap()["status"],
        "finalized"
    );
    assert!(operator.bond(&settings, &op(2), intent()).await.is_err());
    assert_eq!(state.lock().unwrap().raws.len(), 1);
    // A subsequent payment uses nonce 1; the bond's native multicall used nonce 0.
    fund(&operator, &agreed).await;
    assert_eq!(attempt(&operator, &attempt_id(&agreed, "open")).tx.nonce, 1);
    // A pending payment prevents signing a new bond, including with another salt.
    state.lock().unwrap().hide_receipt = true;
    operator
        .mpp(open_request(&terms(&operator, &seller, 2)))
        .await
        .unwrap();
    settings.max_lock = 100;
    let mut next = intent();
    if let Intent::Lock { salt, .. } = &mut next {
        *salt = B256::repeat_byte(4).to_string();
    }
    assert!(operator.bond(&settings, &op(3), next).await.is_err());
    assert!(
        operator
            .journal
            .get::<BondAttempt>(&op(3))
            .unwrap()
            .is_none()
    );
    server.abort();
}

#[tokio::test]
async fn explicit_refund_works_without_authorization_for_new_locks() {
    let (state, url, server) = server().await;
    let dir = tempfile::tempdir().unwrap();
    let wallet = wallet(dir.path(), &url, 1);
    let mut settings = settings();
    let mut initial = intent();
    if let Intent::Lock {
        exit_delay_seconds, ..
    } = &mut initial
    {
        *exit_delay_seconds = 10;
    }
    let result = finalized_lock(&wallet, &settings, &initial).await;
    let id = result["bond_id"].as_str().unwrap().to_string();
    assert!(
        wallet
            .bond(
                &settings,
                &op(2),
                Intent::Withdraw {
                    bond_id: id.clone()
                }
            )
            .await
            .is_err()
    );
    settings.max_lock = 0;
    settings.max_exit_delay_seconds = 0;
    let exit = Intent::RequestExit {
        bond_id: id.clone(),
    };
    let before = crate::config::now();
    let exited = wallet.bond(&settings, &op(3), exit.clone()).await.unwrap();
    wallet.bond(&settings, &op(3), exit).await.unwrap();
    // The exit records when its delay has passed at the earliest.
    let delay = state.lock().unwrap().bonds[&id.parse::<B256>().unwrap()].exitDelay;
    let withdraw_after = exited["withdraw_after"].as_u64().unwrap();
    assert!((before + delay..=crate::config::now() + delay).contains(&withdraw_after));
    let deadline =
        u64::try_from(state.lock().unwrap().bonds[&id.parse::<B256>().unwrap()].withdrawAt)
            .unwrap();
    state.lock().unwrap().now = deadline - 1;
    assert!(
        wallet
            .bond(
                &settings,
                &op(2),
                Intent::Withdraw {
                    bond_id: id.clone()
                }
            )
            .await
            .is_err()
    );
    state.lock().unwrap().now = deadline;
    let refund = Intent::Withdraw { bond_id: id };
    wallet
        .bond(&settings, &op(2), refund.clone())
        .await
        .unwrap();
    assert_eq!(
        wallet.bond(&settings, &op(2), refund).await.unwrap()["status"],
        "finalized"
    );
    assert_eq!(
        state.lock().unwrap().balances[&wallet.profile.address.parse::<Address>().unwrap()],
        1_000_000
    );
    server.abort();
}

#[tokio::test]
async fn pending_exit_survives_restart_without_resetting_the_delay_or_principal() {
    let (state, url, server) = server().await;
    let directory = tempfile::tempdir().unwrap();
    let wallet = wallet(directory.path(), &url, 1);
    let settings = settings();
    let locked = finalized_lock(&wallet, &settings, &intent()).await;
    let bond_id = locked["bond_id"].as_str().unwrap().to_string();
    let exit = Intent::RequestExit {
        bond_id: bond_id.clone(),
    };
    state.lock().unwrap().lose_reply = true;
    wallet.bond(&settings, &op(2), exit.clone()).await.unwrap();
    let original = bond_attempt(&wallet, &op(2));
    state
        .lock()
        .unwrap()
        .hidden_receipts
        .insert(original.tx.hash.clone());
    let deadline = state.lock().unwrap().bonds[&bond_id.parse::<B256>().unwrap()].withdrawAt;
    drop(wallet);
    let wallet = super::wallet(directory.path(), &url, 1);
    assert_eq!(
        wallet.bond(&settings, &op(2), exit.clone()).await.unwrap()["status"],
        "pending"
    );
    assert_eq!(bond_attempt(&wallet, &op(2)).tx.raw, original.tx.raw);
    state.lock().unwrap().hidden_receipts.clear();
    assert_eq!(
        wallet.bond(&settings, &op(2), exit.clone()).await.unwrap()["status"],
        "finalized"
    );
    assert!(wallet.bond(&settings, &op(3), exit).await.is_err());
    assert_eq!(
        state.lock().unwrap().bonds[&bond_id.parse::<B256>().unwrap()].withdrawAt,
        deadline
    );
    assert_eq!(
        state.lock().unwrap().bonds[&bond_id.parse::<B256>().unwrap()].amount,
        U96::from(100)
    );
    assert_eq!(state.lock().unwrap().raws.len(), 2);
    let exiting = bonds::verify(
        &wallet.rpc,
        &settings,
        "fixture-mesh",
        &"ab".repeat(32),
        proof(&locked),
    )
    .await
    .unwrap();
    assert_eq!(exiting["valid_until"], u64::try_from(deadline).unwrap());
    server.abort();
}

#[tokio::test]
async fn verifier_has_no_second_eligibility_cache() {
    let (state, url, server) = server().await;
    let dir = tempfile::tempdir().unwrap();
    let wallet = wallet(dir.path(), &url, 1);
    let settings = settings();
    let locked = finalized_lock(&wallet, &settings, &intent()).await;
    let reference = proof(&locked);
    let config = crate::config::Config {
        data_dir: dir.path().join("engine"),
        wallet: wallet.settings.clone(),
        bonds: settings.clone(),
        prices: Default::default(),
        policy: Default::default(),
        cap: 0,
        buy_peers: Default::default(),
        buy_prices: Default::default(),
        admission_deposit: None,
        channel_topup: 0,
    };
    let service = bonds::Service::new(&config).unwrap();
    let host = "ab".repeat(32);
    let decision = service
        .verify(&settings, "fixture-mesh", &host, reference.clone())
        .await
        .unwrap();
    assert_eq!(decision["valid_until"], state.lock().unwrap().now + 3600);
    let after = state.lock().unwrap().calls.len();
    let id: B256 = locked["bond_id"].as_str().unwrap().parse().unwrap();
    state.lock().unwrap().bonds.get_mut(&id).unwrap().withdrawAt = U256::from(crate::config::now());
    assert!(
        service
            .verify(&settings, "fixture-mesh", &host, reference)
            .await
            .unwrap_err()
            .to_string()
            .contains("withdrawable")
    );
    assert!(
        state.lock().unwrap().calls.len() > after,
        "only Mesh may reuse eligibility decisions"
    );
    server.abort();
}
