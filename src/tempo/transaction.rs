//! Native transactions shared by payments and bonds. Callers journal before sending.
use super::{
    Wallet, receipt,
    rpc::{address, ceil_fee, hex_u128, quantity},
};
use crate::{config::now, contract::OUSD};
use alloy::{
    eips::eip2718::Encodable2718,
    network::{EthereumWallet, NetworkTransactionBuilder, TransactionBuilder},
    primitives::{Address, U256, keccak256},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::num::NonZeroU64;
use tempo_alloy::{primitives::transaction::Call, rpc::TempoTransactionRequest};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SignedTransaction {
    pub reserved: u64,
    pub hash: String,
    pub raw: String,
    pub nonce: u64,
    pub expires_at: u64,
    pub expiring_nonce_hash: String,
    pub status: String,
}
pub fn call(to: Address, input: Vec<u8>) -> Call {
    Call {
        to: to.into(),
        value: U256::ZERO,
        input: input.into(),
    }
}
impl Wallet {
    pub async fn reconcile(&self) -> Result<()> {
        let _guard = self.send_lock.lock().await;
        if !self.journal.pending()? {
            return Ok(());
        }
        self.rpc.check_network().await?;
        self.reconcile_pending_locked().await
    }
    pub(super) async fn reconcile_pending_locked(&self) -> Result<()> {
        self.resume_bonds().await?;
        self.resume_sends().await?;
        for attempt in self.journal.unresolved::<super::journal::Attempt>()? {
            self.mpp_resume(attempt).await?;
        }
        Ok(())
    }
    pub(super) async fn prepare_transaction(
        &self,
        calls: Vec<Call>,
        expires_at: u64,
        amount: u64,
        max_fee: u64,
        max_gas: u64,
    ) -> Result<SignedTransaction> {
        ensure!(
            !self.journal.pending()?,
            "another signed payment or bond is unresolved; reconcile its exact bytes first"
        );
        let signer = self
            .signer
            .as_ref()
            .context("transaction requires an unlocked wallet")?;
        let nonce = quantity(
            &self
                .rpc
                .call(
                    "eth_getTransactionCount",
                    json!([self.profile.address, "pending"]),
                )
                .await?,
        )?;
        let mut tx = TempoTransactionRequest::default()
            .with_chain_id(self.settings.network.chain_id())
            .with_from(address(&self.profile.address)?)
            .with_calls(calls)
            .with_nonce(nonce)
            .with_nonce_key(U256::ZERO)
            .with_fee_token(address(OUSD)?)
            .with_valid_before(NonZeroU64::new(expires_at).context("zero expiry")?);
        let gas = quantity(
            &self
                .rpc
                .call("eth_estimateGas", json!([serde_json::to_value(&tx)?]))
                .await?,
        )?
        .checked_mul(120)
        .context("gas overflow")?
        .div_ceil(100);
        ensure!(
            gas > 0 && gas <= max_gas,
            "unreasonable transaction gas limit: {gas}"
        );
        let price = hex_u128(&self.rpc.call("eth_gasPrice", json!([])).await?)?
            .checked_mul(2)
            .context("fee overflow")?;
        ensure!(price > 0, "zero gas price");
        let fee = ceil_fee(gas, price)?;
        ensure!(
            fee <= max_fee && fee <= self.settings.max_fee,
            "transaction fee {fee} exceeds authorization {}",
            max_fee.min(self.settings.max_fee)
        );
        let reserved = fee.checked_add(amount).context("debit overflow")?;
        ensure!(
            self.rpc.balance(address(&self.profile.address)?).await?.0 >= reserved,
            "insufficient OUSD for transaction and gas"
        );
        tx = tx
            .with_gas_limit(gas)
            .with_max_fee_per_gas(price)
            .with_max_priority_fee_per_gas(0);
        let envelope = tx.build(&EthereumWallet::from(signer.clone())).await?;
        let expiring_nonce_hash = envelope
            .as_aa()
            .context("expected native Tempo transaction")?
            .expiring_nonce_hash(address(&self.profile.address)?)
            .to_string();
        let raw = envelope.encoded_2718();
        Ok(SignedTransaction {
            reserved,
            hash: keccak256(&raw).to_string(),
            raw: format!("0x{}", hex::encode(raw)),
            nonce,
            expires_at,
            expiring_nonce_hash,
            status: "pending".into(),
        })
    }
    pub(super) async fn resume_transaction(
        &self,
        hash: &str,
        raw: &str,
        expires_at: u64,
        nonce: u64,
    ) -> Result<String> {
        // Receipt eligibility and expiry must use the same finalized cutoff.
        let finalized = self.rpc.finalized().await?;
        if let Some(receipt) = receipt::finalized_receipt_at(&self.rpc, hash, &finalized).await? {
            return Ok(if quantity(&receipt["status"])? == 1 {
                "finalized"
            } else {
                "reverted"
            }
            .into());
        }
        if quantity(&finalized["timestamp"])? >= expires_at {
            let consumed = quantity(
                &self
                    .rpc
                    .call(
                        "eth_getTransactionCount",
                        json!([self.profile.address, {"blockHash":finalized["hash"],"requireCanonical":true}]),
                    )
                    .await?,
            )? > nonce;
            // A lagging receipt index must not turn an included transaction into expiry.
            return Ok(if consumed { "pending" } else { "expired" }.into());
        }
        if now() < expires_at {
            // Transport failure retains the reservation and the original signed bytes.
            if let Ok(sent) = self.rpc.call("eth_sendRawTransaction", json!([raw])).await {
                ensure!(
                    sent.as_str()
                        .is_some_and(|sent| sent.eq_ignore_ascii_case(hash)),
                    "RPC returned wrong transaction hash"
                );
            }
        }
        Ok("pending".into())
    }
}

pub(crate) fn public_attempt(attempt: &impl Serialize) -> Result<serde_json::Value> {
    let mut value = serde_json::to_value(attempt)?;
    value
        .as_object_mut()
        .context("invalid attempt metadata")?
        .remove("raw");
    Ok(value)
}
