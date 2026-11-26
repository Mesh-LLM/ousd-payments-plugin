//! Journaled OUSD transfers out of the wallet's own balance.
use super::{
    Wallet, receipt,
    rpc::{address, quantity},
    transaction::{SignedTransaction, call},
};
use crate::{
    config::now,
    contract::{OUSD, mpp::RESERVE, validate_address},
};
use alloy::{
    primitives::{B256, U256},
    sol,
    sol_types::{SolCall, SolEvent},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

sol! {
    function transfer(address to, uint256 amount) external returns (bool);
    event Transfer(address indexed from, address indexed to, uint256 amount);
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SendAttempt {
    pub id: String,
    pub chain_id: u64,
    pub to: String,
    #[serde(with = "crate::contract::decimal")]
    pub amount: u64,
    #[serde(flatten)]
    pub tx: SignedTransaction,
    /// Whether the finalized receipt shows this transfer.
    pub verified: bool,
}

impl Wallet {
    /// Sends `amount` OUSD to `to` once per `operation_id`. It spends only
    /// the wallet's own balance: channel deposits are held by the reserve.
    pub async fn send(
        &self,
        operation_id: &str,
        to: &str,
        amount: u64,
        max_fee: u64,
    ) -> Result<Value> {
        let operation_id =
            uuid::Uuid::parse_str(operation_id).context("send operation ID must be a UUID")?;
        let id = format!("send/{operation_id}");
        validate_address(to)?;
        ensure!(
            !to.eq_ignore_ascii_case(&self.profile.address),
            "sending to this wallet refused"
        );
        ensure!(
            !to.eq_ignore_ascii_case(OUSD) && !to.eq_ignore_ascii_case(RESERVE),
            "sending to the token or reserve contract refused"
        );
        ensure!(amount > 0, "amount must be greater than zero");
        let chain_id = self.settings.network.chain_id();
        let _guard = self.send_lock.lock().await;
        self.rpc.check_network().await?;
        self.reconcile_pending_locked().await?;
        let attempt = if let Some(old) = self.journal.get::<SendAttempt>(&id)? {
            ensure!(
                old.chain_id == chain_id
                    && address(&old.to)? == address(to)?
                    && old.amount == amount,
                "send operation ID reused with another intent"
            );
            old
        } else {
            let data = transferCall {
                to: address(to)?,
                amount: U256::from(amount),
            }
            .abi_encode();
            let tx = self
                .prepare_transaction(
                    vec![call(address(OUSD)?, data)],
                    now() + 600,
                    amount,
                    max_fee,
                    2_000_000,
                )
                .await?;
            let attempt = SendAttempt {
                id,
                chain_id,
                to: to.into(),
                amount,
                tx,
                verified: false,
            };
            self.journal.save(&attempt, true)?;
            attempt
        };
        let attempt = self.resume_send(attempt).await?;
        super::transaction::public_attempt(&attempt)
    }
    async fn resume_send(&self, mut attempt: SendAttempt) -> Result<SendAttempt> {
        ensure!(
            attempt.chain_id == self.settings.network.chain_id(),
            "send journal network changed"
        );
        if attempt.tx.status == "pending" {
            attempt.tx.status = self
                .resume_transaction(
                    &attempt.tx.hash,
                    &attempt.tx.raw,
                    attempt.tx.expires_at,
                    attempt.tx.nonce,
                )
                .await?;
            self.journal.save(&attempt, false)?;
        }
        if attempt.tx.status == "finalized" && !attempt.verified {
            let receipt = receipt::finalized_receipt(&self.rpc, &attempt.tx.hash)
                .await?
                .context("finalized send disappeared")?;
            verify_transfer(&self.profile.address, &attempt, &receipt)?;
            attempt.verified = true;
            self.journal.save(&attempt, false)?;
        }
        Ok(attempt)
    }
    pub(super) async fn resume_sends(&self) -> Result<()> {
        for attempt in self.journal.unresolved::<SendAttempt>()? {
            self.resume_send(attempt).await?;
        }
        Ok(())
    }
}

/// The receipt must show exactly one OUSD transfer of the amount from this
/// wallet to the recipient; other token events, such as fees, are ignored.
pub(super) fn verify_transfer(from: &str, attempt: &SendAttempt, receipt: &Value) -> Result<()> {
    ensure!(quantity(&receipt["status"])? == 1, "transfer reverted");
    let expected = (
        address(from)?,
        address(&attempt.to)?,
        U256::from(attempt.amount),
    );
    let mut found = 0;
    for log in receipt["logs"].as_array().context("missing receipt logs")? {
        if !log["address"]
            .as_str()
            .is_some_and(|v| v.eq_ignore_ascii_case(OUSD))
            || log["removed"] == true
        {
            continue;
        }
        let topics: Vec<B256> = log["topics"]
            .as_array()
            .context("missing topics")?
            .iter()
            .map(|v| Ok(v.as_str().context("invalid topic")?.parse()?))
            .collect::<Result<_>>()?;
        if topics.first() != Some(&Transfer::SIGNATURE_HASH) {
            continue;
        }
        let data = hex::decode(
            log["data"]
                .as_str()
                .context("missing log data")?
                .trim_start_matches("0x"),
        )?;
        let event = Transfer::decode_raw_log_validate(topics, &data)?;
        if (event.from, event.to, event.amount) == expected {
            found += 1;
        }
    }
    ensure!(
        found == 1,
        "receipt does not show exactly this OUSD transfer"
    );
    Ok(())
}
