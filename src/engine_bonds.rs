//! The node's published bond reference: set when a bond lock finalizes or
//! by the operator, withdrawn with its bond, and sent again until the host
//! accepts it. Bonds this wallet exits are withdrawn once their delay passes.
use super::*;
use std::sync::atomic::Ordering::SeqCst;

impl Engine {
    /// Publishes the current reference, or withdraws it when there is none.
    pub async fn publish_bond(&self) -> Result<()> {
        let bond = self.ledger.published_bond()?;
        let evidence = self.bonds.proof(&self.config.bonds, bond.as_deref())?;
        let result = self.send_evidence(evidence).await;
        self.bond_unsynced.store(result.is_err(), SeqCst);
        result
    }
    async fn send_evidence(&self, evidence: Option<crate::bonds::Proof>) -> Result<()> {
        if let Some(context) = &self.context {
            let _: Value = context
                .clone()
                .request_host(
                    "PublishPeerEvidence",
                    serde_json::json!({ "evidence": evidence }),
                )
                .await?;
            return Ok(());
        }
        #[cfg(test)]
        {
            ensure!(!self.unreachable.load(SeqCst), "host unreachable");
            self.published.lock().unwrap().push(evidence);
            Ok(())
        }
        #[cfg(not(test))]
        bail!("host context unavailable")
    }
    /// Publishes `bond_id`, or withdraws the reference with `None`.
    pub async fn set_published_bond(&self, bond_id: Option<&str>) -> Result<()> {
        self.bonds.proof(&self.config.bonds, bond_id)?;
        self.ledger.set_published_bond(bond_id)?;
        self.publish_bond().await
    }
    /// Withdraws each bond whose exit this wallet requested, once its delay
    /// has passed, into the wallet. The wallet refuses a withdrawal before
    /// the finalized chain reaches the deadline, without signing, so an
    /// early try is repeated on a later tick.
    async fn sweep_exits(&self) -> Result<()> {
        let attempts = self.wallet.attempts()?;
        let finalized = |kind: &'static str| {
            attempts
                .iter()
                .filter(move |a| a["intent"]["kind"] == kind && a["status"] == "finalized")
        };
        for exit in finalized("request_exit") {
            let bond = exit["bond_id"]
                .as_str()
                .context("bond exit without a bond")?;
            let withdrawn = finalized("withdraw").any(|a| {
                a["bond_id"]
                    .as_str()
                    .is_some_and(|w| w.eq_ignore_ascii_case(bond))
            });
            let due = exit["withdraw_after"].as_u64().is_none_or(|at| now() >= at);
            if withdrawn || !due {
                continue;
            }
            let operation = crate::bonds::exit_operation("withdraw", bond)?;
            let intent = crate::bonds::Intent::Withdraw {
                bond_id: bond.into(),
            };
            if let Err(error) = self
                .wallet
                .bond(&self.config.bonds, &operation, intent)
                .await
            {
                tracing::debug!(bond, %error, "bond withdrawal not possible yet");
            }
        }
        Ok(())
    }
    /// The bond of the newest finalized lock.
    pub(crate) fn newest_bond(&self) -> Result<Option<String>> {
        let attempts = self.wallet.attempts()?;
        Ok(attempts
            .iter()
            .rev()
            .find(|a| a["intent"]["kind"] == "lock" && a["status"] == "finalized")
            .and_then(|a| a["bond_id"].as_str())
            .map(Into::into))
    }
    /// Publishes the newest finalized bond lock once, withdraws the
    /// reference once its bond is withdrawn, and repeats a publication the
    /// host refused.
    pub(crate) async fn sync_bond(&self) -> Result<()> {
        self.sweep_exits().await?;
        let attempts = self.wallet.attempts()?;
        let finalized = |kind: &'static str| {
            attempts
                .iter()
                .filter(move |a| a["intent"]["kind"] == kind && a["status"] == "finalized")
        };
        if let Some(lock) = finalized("lock").next_back() {
            let operation = lock["id"].as_str().context("bond lock without an ID")?;
            if self.ledger.published_lock()?.as_deref() != Some(operation) {
                let bond = lock["bond_id"]
                    .as_str()
                    .context("bond lock without a bond")?;
                self.ledger.set_published_lock(operation)?;
                return self.set_published_bond(Some(bond)).await;
            }
        }
        let published = self.ledger.published_bond()?;
        let withdrawn = published.as_ref().is_some_and(|id| {
            finalized("withdraw").any(|a| {
                a["bond_id"]
                    .as_str()
                    .is_some_and(|w| w.eq_ignore_ascii_case(id))
            })
        });
        if withdrawn {
            return self.set_published_bond(None).await;
        }
        if self.bond_unsynced.load(SeqCst) {
            return self.publish_bond().await;
        }
        Ok(())
    }
}
