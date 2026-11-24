use super::{
    Wallet, receipt,
    rpc::address,
    transaction::{SignedTransaction, call},
};
use crate::{
    bonds::{self, Escrow, Intent, Settings},
    config::now,
    contract::OUSD,
};
use alloy::{
    primitives::{B256, U256},
    sol_types::SolCall,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BondAttempt {
    pub id: String,
    pub contract: String,
    pub chain_id: u64,
    pub intent: Intent,
    pub bond_id: String,
    /// For an exit, when its delay has passed at the earliest: the signing
    /// time plus the bond's exit delay, since the chain starts the delay
    /// when the exit lands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub withdraw_after: Option<u64>,
    #[serde(flatten)]
    pub tx: SignedTransaction,
}
impl Wallet {
    pub async fn bond(
        &self,
        settings: &Settings,
        operation_id: &str,
        intent: Intent,
    ) -> Result<serde_json::Value> {
        let operation_id = uuid::Uuid::parse_str(operation_id)
            .context("bond operation ID must be a UUID")?
            .to_string();
        let _guard = self.send_lock.lock().await;
        self.rpc.check_network().await?;
        self.reconcile_pending_locked().await?;
        let contract = settings
            .contract
            .as_deref()
            .context("bond escrow missing")?;
        bonds::check_contract(&self.rpc, contract, "finalized").await?;
        let attempt = if let Some(old) = self.journal.get::<BondAttempt>(&operation_id)? {
            ensure!(
                old.intent == intent && address(&old.contract)? == address(contract)?,
                "bond operation ID reused with another intent"
            );
            old
        } else {
            let mut withdraw_after = None;
            let (calls, amount, id) = match &intent {
                Intent::Lock {
                    amount,
                    exit_delay_seconds,
                    salt,
                    ..
                } => {
                    ensure!(
                        *amount > 0 && *amount <= settings.max_lock,
                        "bond exceeds explicit lock authorization"
                    );
                    ensure!(
                        *exit_delay_seconds > 0
                            && *exit_delay_seconds <= settings.max_exit_delay_seconds
                            && *exit_delay_seconds <= bonds::MAX_EXIT_DELAY_SECONDS,
                        "exit delay exceeds authorization"
                    );
                    let id = bonds::bond_id(&self.rpc, contract, &self.profile.address, salt)?;
                    let calls = vec![
                        call(
                            address(OUSD)?,
                            bonds::approveCall {
                                spender: address(contract)?,
                                amount: U256::from(*amount),
                            }
                            .abi_encode(),
                        ),
                        call(address(contract)?, bonds::lock_data(&intent)?),
                    ];
                    (calls, *amount, id)
                }
                Intent::RequestExit { bond_id } => {
                    let id: B256 = bond_id.parse()?;
                    let bond =
                        receipt::call(&self.rpc, contract, Escrow::getBondCall { id }, "finalized")
                            .await?;
                    ensure!(
                        bond.refundWallet == address(&self.profile.address)? && !bond.withdrawn,
                        "bond is not owned and funded by this wallet"
                    );
                    ensure!(bond.withdrawAt == U256::ZERO, "bond exit already requested");
                    withdraw_after = Some(now().saturating_add(bond.exitDelay));
                    (
                        vec![call(
                            address(contract)?,
                            Escrow::requestExitCall { id }.abi_encode(),
                        )],
                        0,
                        id,
                    )
                }
                Intent::Withdraw { bond_id } => {
                    let id: B256 = bond_id.parse()?;
                    let bond =
                        receipt::call(&self.rpc, contract, Escrow::getBondCall { id }, "finalized")
                            .await?;
                    ensure!(
                        bond.refundWallet == address(&self.profile.address)?,
                        "only original funding wallet may withdraw"
                    );
                    ensure!(
                        !bond.withdrawn
                            && bond.withdrawAt != U256::ZERO
                            && U256::from(crate::tempo::rpc::quantity(
                                &self.rpc.finalized().await?["timestamp"]
                            )?) >= bond.withdrawAt,
                        "bond still locked or withdrawn"
                    );
                    (
                        vec![call(
                            address(contract)?,
                            Escrow::withdrawCall { id }.abi_encode(),
                        )],
                        0,
                        id,
                    )
                }
            };
            // Approval and deposit are one atomic transaction with one shared wallet nonce.
            let tx = self
                .prepare_transaction(calls, now() + 600, amount, self.settings.max_fee, 8_000_000)
                .await?;
            let attempt = BondAttempt {
                id: operation_id,
                contract: contract.into(),
                chain_id: self.settings.network.chain_id(),
                intent,
                bond_id: id.to_string(),
                withdraw_after,
                tx,
            };
            self.journal.save(&attempt, true)?;
            attempt
        };
        let attempt = self.resume_bond(attempt).await?;
        super::transaction::public_attempt(&attempt)
    }
    async fn resume_bond(&self, mut attempt: BondAttempt) -> Result<BondAttempt> {
        ensure!(
            attempt.chain_id == self.settings.network.chain_id(),
            "bond journal network changed"
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
        Ok(attempt)
    }
    pub(super) async fn resume_bonds(&self) -> Result<()> {
        for attempt in self.journal.unresolved::<BondAttempt>()? {
            self.resume_bond(attempt).await?;
        }
        Ok(())
    }
}
