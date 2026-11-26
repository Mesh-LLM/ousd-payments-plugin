use super::{
    journal::{Attempt, Journal},
    keys::{self, Profile},
    receipt as chain,
    rpc::{Rpc, address, quantity},
    transaction::{SignedTransaction, call},
};
use crate::config::now;
use crate::contract::{
    Balance, Network, WalletInfo,
    mpp::{ChannelFunding, ChannelTerms, CloseEvents, RESERVE, Request, Response, Voucher},
};
use alloy::{
    primitives::aliases::U96,
    signers::{Signer, local::PrivateKeySigner},
    sol_types::SolCall,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tempo_alloy::contracts::precompiles::ITIP20ChannelReserve as Reserve;

/// Lifetime of each close, request-close or withdraw attempt. Expired
/// attempts count toward `max_settlement_attempts`, so the retries together
/// must outlast the buyer's earliest withdrawal; a stuck attempt holds the
/// wallet's single transaction slot for at most this long.
const SETTLEMENT_ATTEMPT_SECONDS: u64 = 3600;
/// The longest block range one close-event scan reads; a longer gap is
/// covered by inspecting every channel instead.
const MAX_CLOSE_LOG_BLOCKS: u64 = 10_000;
/// Lifetime of each open or top-up attempt, which a paid request waits for.
const FUNDING_ATTEMPT_SECONDS: u64 = 600;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub network: Network,
    pub rpc_url: Option<String>,
    pub password_file: Option<PathBuf>,
    pub max_settlement_attempts: u32,
    /// The largest channel open or top-up deposit.
    #[serde(with = "crate::contract::decimal")]
    pub max_payment: u64,
    /// The largest fee of any one transaction attempt.
    #[serde(with = "crate::contract::decimal")]
    pub max_fee: u64,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            network: Network::default(),
            rpc_url: None,
            password_file: None,
            max_settlement_attempts: 3,
            max_payment: 1_000_000,
            max_fee: 10_000,
        }
    }
}
impl Settings {
    pub fn rpc_url(&self) -> &str {
        self.rpc_url.as_deref().unwrap_or(self.network.rpc_url())
    }
    /// The configured password file, else the one a wallet created on first
    /// use keeps in its state directory `dir`.
    pub fn password(&self, dir: &Path) -> Option<PathBuf> {
        self.password_file
            .clone()
            .or_else(|| Some(keys::generated_password(dir)).filter(|p| p.exists()))
    }
}

pub struct Wallet {
    pub settings: Settings,
    pub profile: Profile,
    pub(crate) signer: Option<PrivateKeySigner>,
    pub rpc: Rpc,
    pub journal: Journal,
    pub(crate) send_lock: tokio::sync::Mutex<()>,
}
impl Wallet {
    /// Loads the wallet whose key lives in the payment state directory `dir`.
    pub fn load(settings: Settings, dir: &Path, db: crate::ledger::Database) -> Result<Arc<Self>> {
        ensure!(
            (1..=10).contains(&settings.max_settlement_attempts),
            "max_settlement_attempts must be between 1 and 10"
        );
        let dir = keys::canonical_state_dir(dir)?;
        let profile = keys::load_profile(&dir, settings.network)?;
        let signer = keys::unlock(&dir, settings.password(&dir).as_deref(), &profile)?;
        let journal = Journal::open(db, &profile.wallet_id, &profile.address, settings.network)?;
        let rpc = Rpc::new(settings.rpc_url().into(), settings.network)?;
        Ok(Arc::new(Self {
            settings,
            profile,
            signer,
            rpc,
            journal,
            send_lock: tokio::sync::Mutex::new(()),
        }))
    }
    /// Creates the wallet in `dir`, with a random password there unless one
    /// is configured, and loads it.
    pub fn create(
        settings: Settings,
        dir: &Path,
        db: crate::ledger::Database,
    ) -> Result<Arc<Self>> {
        let state = keys::canonical_state_dir(dir)?;
        let password = match &settings.password_file {
            Some(path) => path.clone(),
            None => {
                let path = keys::generated_password(&state);
                keys::create_password(&path)?;
                path
            }
        };
        keys::initialize(&state, &password, settings.network)?;
        Self::load(settings, dir, db)
    }
    /// Where to send OUSD to fund this wallet; on testnet it also asks the
    /// faucet for test coins.
    pub async fn fund(&self) -> Result<serde_json::Value> {
        let network = self.settings.network;
        let mut funding = serde_json::json!({
            "address": self.profile.address,
            "network": network,
            "asset": network.asset(),
        });
        if network == Network::Testnet {
            self.rpc.check_network().await?;
            let params = serde_json::json!([self.profile.address]);
            funding["faucet"] = self.rpc.call("tempo_fundAddress", params).await?;
        }
        Ok(funding)
    }
    pub async fn info(&self) -> Result<WalletInfo> {
        self.rpc.check_network().await?;
        Ok(WalletInfo {
            wallet_id: self.profile.wallet_id.clone(),
            address: self.profile.address.clone(),
            asset: self.settings.network.asset(),
            can_send: self.signer.is_some(),
        })
    }
    pub async fn balance(&self) -> Result<Balance> {
        self.rpc.check_network().await?;
        let (balance, block_number) = self.rpc.balance(address(&self.profile.address)?).await?;
        let reserved = self.journal.reserved()?;
        Ok(Balance {
            balance,
            reserved,
            spendable: balance.saturating_sub(reserved),
            block_number,
        })
    }
    pub async fn mpp(&self, request: Request) -> Result<Response> {
        self.check_terms(request.terms())?;
        // Signing and polling take no transaction slot.
        let request = match request {
            Request::SignVoucher { funding, cum } => return self.sign_voucher(&funding, cum).await,
            Request::Inspect { funding } => return self.channel_poll(&funding).await,
            request => request,
        };
        let _guard = self.send_lock.lock().await;
        chain::check_abi(&self.rpc).await?;
        self.reconcile_pending_locked().await?;
        match request {
            Request::Open {
                terms,
                deposit,
                max_fee,
            } => self.channel_open(terms, deposit, max_fee).await,
            Request::TopUp {
                funding,
                amount,
                index,
                max_fee,
            } => self.channel_top_up(funding, amount, index, max_fee).await,
            Request::VerifyVoucher { funding, voucher } => Ok(Response {
                deposit: chain::verify_voucher(&self.rpc, &funding, &voucher).await?,
                ..Default::default()
            }),
            Request::Close {
                funding,
                voucher,
                capture,
            } => self.channel_close(funding, voucher, capture).await,
            Request::Recover { funding, max_fee } => self.channel_recover(funding, max_fee).await,
            Request::SignVoucher { .. } | Request::Inspect { .. } => unreachable!(),
        }
    }
    /// The channels of this payee named in close events from block `from` to
    /// the latest block. Without a starting block, or after a gap longer than
    /// `MAX_CLOSE_LOG_BLOCKS`, it reads no logs and reports the scan skipped.
    pub async fn close_events(&self, from: Option<u64>) -> Result<CloseEvents> {
        let to = quantity(
            &self
                .rpc
                .call("eth_blockNumber", serde_json::json!([]))
                .await?,
        )?;
        let Some(from) = from.filter(|from| to.saturating_sub(*from) <= MAX_CLOSE_LOG_BLOCKS)
        else {
            return Ok(CloseEvents {
                to,
                skipped: true,
                ..Default::default()
            });
        };
        let payee = &self.profile.address;
        Ok(CloseEvents {
            channels: chain::close_event_channels(&self.rpc, payee, from.min(to), to).await?,
            to,
            skipped: false,
        })
    }
    /// One state read while the channel is open; closure evidence is looked
    /// up only once it is closing or gone.
    async fn channel_poll(&self, funding: &ChannelFunding) -> Result<Response> {
        let state = chain::state(&self.rpc, funding, "latest").await?;
        if state.deposit > U96::ZERO && state.closeRequestedAt == 0 {
            return Ok(Response {
                deposit: u64::try_from(state.deposit)?,
                ..Default::default()
            });
        }
        chain::check_abi(&self.rpc).await?;
        self.channel_inspect(funding).await
    }
    fn check_terms(&self, terms: &ChannelTerms) -> Result<()> {
        terms.validate()?;
        ensure!(
            terms.asset.network()? == self.settings.network,
            "payment terms belong to another Tempo network"
        );
        Ok(())
    }
    fn check_payer(&self, terms: &ChannelTerms) -> Result<()> {
        ensure!(
            terms.payer.eq_ignore_ascii_case(&self.profile.address),
            "payer is not this wallet"
        );
        Ok(())
    }
    async fn sign_voucher(&self, funding: &ChannelFunding, cum: u64) -> Result<Response> {
        self.check_payer(&funding.terms)?;
        let signature = self
            .signer
            .as_ref()
            .context("unlock wallet to sign vouchers")?
            .sign_hash(&chain::voucher_digest(
                self.settings.network,
                funding.channel_id.parse()?,
                cum,
            ))
            .await?;
        Ok(Response {
            voucher: Some(Voucher {
                channel_id: funding.channel_id.clone(),
                cum,
                signature: format!("0x{}", hex::encode(signature.as_bytes())),
            }),
            ..Default::default()
        })
    }
    async fn channel_open(
        &self,
        terms: ChannelTerms,
        deposit: u64,
        max_fee: u64,
    ) -> Result<Response> {
        self.check_payer(&terms)?;
        let id = format!("channel/{}/open", terms.salt()?);
        let attempt = if let Some(old) = self.journal.get::<Attempt>(&id)? {
            ensure!(
                old.terms == terms && old.amount == deposit,
                "channel reused with different terms"
            );
            old
        } else {
            ensure!(
                deposit > 0 && deposit <= self.settings.max_payment,
                "deposit exceeds wallet limit"
            );
            let data = chain::open_data(&terms, deposit)?;
            let tx = self
                .reserve_transaction(data, FUNDING_ATTEMPT_SECONDS, deposit, max_fee)
                .await?;
            let attempt = Attempt {
                id,
                terms: terms.clone(),
                kind: "open".into(),
                amount: deposit,
                voucher: None,
                tx,
                funding: None,
                deposit: None,
                closure: None,
            };
            self.journal.save(&attempt, true)?;
            attempt
        };
        let mut attempt = self.mpp_resume(attempt).await?;
        if matches!(attempt.tx.status.as_str(), "expired" | "reverted") {
            return Ok(Response {
                unfunded: true,
                ..Default::default()
            });
        }
        if attempt.tx.status != "finalized" {
            return pending(&attempt);
        }
        if attempt.funding.is_none() {
            let mut funding = ChannelFunding {
                terms,
                open_tx: attempt.tx.hash.clone(),
                channel_id: String::new(),
                expiring_nonce_hash: attempt.tx.expiring_nonce_hash.clone(),
            };
            funding.channel_id =
                chain::channel_id(self.settings.network, &chain::descriptor(&funding)?).to_string();
            let receipt = chain::finalized_receipt(&self.rpc, &attempt.tx.hash)
                .await?
                .context("finalized open disappeared")?;
            ensure!(
                chain::verify_open(&funding, &receipt)? == deposit,
                "opening deposit differs from the request"
            );
            attempt.funding = Some(funding);
            self.journal.save(&attempt, false)?;
        }
        Ok(Response {
            funding: attempt.funding,
            deposit,
            ..Default::default()
        })
    }
    async fn channel_top_up(
        &self,
        funding: ChannelFunding,
        amount: u64,
        index: u32,
        max_fee: u64,
    ) -> Result<Response> {
        self.check_payer(&funding.terms)?;
        let base = format!("channel/{}/topup/{index}", funding.terms.salt()?);
        let (id, previous) = self.attempt_slot(&base, false, |old| {
            old.funding.as_ref() == Some(&funding) && old.amount == amount
        })?;
        let attempt = if let Some(old) = previous {
            old
        } else {
            ensure!(
                amount > 0 && amount <= self.settings.max_payment,
                "top-up exceeds wallet limit"
            );
            let inspected = self.channel_inspect(&funding).await?;
            if inspected.closure.is_some() {
                return Ok(inspected);
            }
            let data = Reserve::topUpCall {
                descriptor: chain::descriptor(&funding)?,
                additionalDeposit: U96::from(amount),
            }
            .abi_encode();
            let tx = self
                .reserve_transaction(data, FUNDING_ATTEMPT_SECONDS, amount, max_fee)
                .await?;
            let attempt = Attempt {
                id,
                terms: funding.terms.clone(),
                kind: "topup".into(),
                amount,
                voucher: None,
                tx,
                funding: Some(funding.clone()),
                deposit: None,
                closure: None,
            };
            self.journal.save(&attempt, true)?;
            attempt
        };
        let mut attempt = self.mpp_resume(attempt).await?;
        if attempt.tx.status != "finalized" {
            return pending(&attempt);
        }
        if attempt.deposit.is_none() {
            let receipt = chain::finalized_receipt(&self.rpc, &attempt.tx.hash)
                .await?
                .context("finalized top-up disappeared")?;
            attempt.deposit = Some(chain::verify_top_up(&funding, &receipt, amount)?);
            self.journal.save(&attempt, false)?;
        }
        Ok(Response {
            deposit: attempt.deposit.unwrap_or_default(),
            ..Default::default()
        })
    }
    async fn channel_inspect(&self, funding: &ChannelFunding) -> Result<Response> {
        let closure = chain::find_close(&self.rpc, funding).await?;
        if closure.is_some() {
            return Ok(Response {
                closure,
                ..Default::default()
            });
        }
        let state = chain::state(&self.rpc, funding, "latest").await?;
        ensure!(
            state.deposit > U96::ZERO,
            "channel absent without finalized close evidence"
        );
        Ok(Response {
            deposit: u64::try_from(state.deposit)?,
            close_requested_at: state.closeRequestedAt.into(),
            ..Default::default()
        })
    }
    async fn channel_close(
        &self,
        funding: ChannelFunding,
        voucher: Option<Voucher>,
        capture: u64,
    ) -> Result<Response> {
        ensure!(
            funding
                .terms
                .payee
                .eq_ignore_ascii_case(&self.profile.address),
            "payee is not this wallet"
        );
        let cum = voucher.as_ref().map_or(0, |v| v.cum);
        ensure!(capture <= cum, "capture exceeds the presented voucher");
        let inspected = self.channel_inspect(&funding).await?;
        if inspected.closure.is_some() {
            return Ok(inspected);
        }
        let base = format!("channel/{}/close", funding.terms.salt()?);
        let (id, previous) = self.attempt_slot(&base, false, |old| {
            old.funding.as_ref() == Some(&funding)
                && old.amount == capture
                && old.voucher == voucher
        })?;
        let attempt = if let Some(old) = previous {
            old
        } else {
            let mut signature = Vec::new();
            if let Some(voucher) = &voucher {
                // A capture of what already settled (nothing) skips the native signature check.
                if capture > 0 {
                    chain::verify_signature(&self.rpc, &funding, voucher).await?;
                }
                signature = hex::decode(voucher.signature.trim_start_matches("0x"))?;
            }
            let data = Reserve::closeCall {
                descriptor: chain::descriptor(&funding)?,
                cumulativeAmount: U96::from(cum),
                captureAmount: U96::from(capture),
                signature: signature.into(),
            }
            .abi_encode();
            let tx = self
                .reserve_transaction(data, SETTLEMENT_ATTEMPT_SECONDS, 0, self.settings.max_fee)
                .await?;
            let attempt = Attempt {
                id,
                terms: funding.terms.clone(),
                kind: "close".into(),
                amount: capture,
                voucher,
                tx,
                funding: Some(funding),
                deposit: None,
                closure: None,
            };
            self.journal.save(&attempt, true)?;
            attempt
        };
        self.channel_finish(attempt).await
    }
    async fn channel_recover(&self, funding: ChannelFunding, max_fee: u64) -> Result<Response> {
        ensure!(
            funding
                .terms
                .payer
                .eq_ignore_ascii_case(&self.profile.address),
            "only payer can recover"
        );
        let inspected = self.channel_inspect(&funding).await?;
        if inspected.closure.is_some() {
            return Ok(inspected);
        }
        let (kind, data) = if inspected.close_requested_at == 0 {
            (
                "request_close",
                Reserve::requestCloseCall {
                    descriptor: chain::descriptor(&funding)?,
                }
                .abi_encode(),
            )
        } else {
            let finalized = self.rpc.finalized().await?;
            if quantity(&finalized["timestamp"])?
                < inspected.close_requested_at + crate::contract::mpp::CLOSE_GRACE_SECONDS
            {
                return Ok(inspected);
            }
            (
                "withdraw",
                Reserve::withdrawCall {
                    descriptor: chain::descriptor(&funding)?,
                }
                .abi_encode(),
            )
        };
        let base = format!("channel/{}/{kind}", funding.terms.salt()?);
        // A finalized close request that the chain no longer shows was
        // cancelled by a top-up, so the next request takes a new slot.
        let (id, previous) = self.attempt_slot(&base, kind == "request_close", |old| {
            old.funding.as_ref() == Some(&funding)
        })?;
        let attempt = if let Some(old) = previous {
            old
        } else {
            let tx = self
                .reserve_transaction(data, SETTLEMENT_ATTEMPT_SECONDS, 0, max_fee)
                .await?;
            let attempt = Attempt {
                id,
                terms: funding.terms.clone(),
                kind: kind.into(),
                amount: 0,
                voucher: None,
                tx,
                funding: Some(funding.clone()),
                deposit: None,
                closure: None,
            };
            self.journal.save(&attempt, true)?;
            attempt
        };
        let response = self.channel_finish(attempt).await?;
        if response.pending || response.closure.is_some() {
            return Ok(response);
        }
        self.channel_inspect(&funding).await
    }
    async fn channel_finish(&self, attempt: Attempt) -> Result<Response> {
        let mut attempt = self.mpp_resume(attempt).await?;
        if attempt.tx.status != "finalized" {
            return pending(&attempt);
        }
        let funding = attempt
            .funding
            .clone()
            .context("missing channel descriptor")?;
        if attempt.kind == "request_close" {
            let receipt = chain::finalized_receipt(&self.rpc, &attempt.tx.hash)
                .await?
                .context("finalized close request disappeared")?;
            chain::verify_close_requested(&funding, &receipt)?;
        } else if attempt.closure.is_none() {
            let receipt = chain::finalized_receipt(&self.rpc, &attempt.tx.hash)
                .await?
                .context("finalized close disappeared")?;
            let closure = chain::verify_close(&funding, &receipt)?;
            ensure!(
                attempt.kind != "close" || closure.captured == attempt.amount,
                "unexpected captured amount"
            );
            attempt.closure = Some(closure);
            self.journal.save(&attempt, false)?;
        }
        Ok(Response {
            closure: attempt.closure,
            ..Default::default()
        })
    }
    /// Signs one reserve call that debits `amount` plus its fee.
    async fn reserve_transaction(
        &self,
        data: Vec<u8>,
        lifetime: u64,
        amount: u64,
        max_fee: u64,
    ) -> Result<SignedTransaction> {
        self.prepare_transaction(
            vec![call(address(RESERVE)?, data)],
            now() + lifetime,
            amount,
            max_fee,
            2_000_000,
        )
        .await
    }
    /// The journal slot for `base`: its unresolved or finalized attempt, or
    /// the next retry after expired or reverted ones, and after finalized
    /// ones too when `skip_finalized`.
    fn attempt_slot(
        &self,
        base: &str,
        skip_finalized: bool,
        same: impl Fn(&Attempt) -> bool,
    ) -> Result<(String, Option<Attempt>)> {
        let maximum = self.settings.max_settlement_attempts;
        ensure!(
            (1..=10).contains(&maximum),
            "max_settlement_attempts must be between 1 and 10"
        );
        // Only failed attempts count toward the limit; skipped finalized ones succeeded.
        let mut failed = 0;
        for number in 1u32.. {
            let id = if number == 1 {
                base.to_owned()
            } else {
                format!("{base}/{number}")
            };
            let Some(old) = self.journal.get::<Attempt>(&id)? else {
                return Ok((id, None));
            };
            ensure!(same(&old), "payment retry intent changed");
            if skip_finalized && old.tx.status == "finalized" {
                continue;
            }
            if matches!(old.tx.status.as_str(), "expired" | "reverted") {
                failed += 1;
                ensure!(
                    failed < maximum,
                    "payment {} retry limit reached; operator attention required",
                    old.kind
                );
                continue;
            }
            return Ok((id, Some(old)));
        }
        unreachable!()
    }
    pub(super) async fn mpp_resume(&self, mut attempt: Attempt) -> Result<Attempt> {
        self.check_terms(&attempt.terms)?;
        if attempt.tx.status != "pending" {
            return Ok(attempt);
        }
        attempt.tx.status = self
            .resume_transaction(
                &attempt.tx.hash,
                &attempt.tx.raw,
                attempt.tx.expires_at,
                attempt.tx.nonce,
            )
            .await?;
        self.journal.save(&attempt, false)?;
        Ok(attempt)
    }
}
fn pending(attempt: &Attempt) -> Result<Response> {
    ensure!(
        attempt.tx.status == "pending",
        "native reserve transaction {}; reconcile the outstanding channel",
        attempt.tx.status
    );
    Ok(Response {
        pending: true,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Payment state in its own directory, so the wallet directory stays empty.
    fn database() -> (tempfile::TempDir, crate::ledger::Database) {
        let state = tempfile::tempdir().unwrap();
        let db = crate::ledger::Ledger::open(state.path())
            .unwrap()
            .db
            .clone();
        (state, db)
    }

    #[test]
    fn settlement_attempt_limits_are_explicit_and_bounded() {
        assert_eq!(Settings::default().max_settlement_attempts, 3);
        for maximum in [0, 11] {
            let dir = tempfile::tempdir().unwrap();
            let (_state, db) = database();
            let result = Wallet::load(
                Settings {
                    max_settlement_attempts: maximum,
                    ..Default::default()
                },
                dir.path(),
                db,
            );
            assert!(
                result
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("max_settlement_attempts")
            );
            assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
        }
    }

    #[test]
    fn startup_never_creates_a_wallet_implicitly() {
        let dir = tempfile::tempdir().unwrap();
        let (_state, db) = database();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        assert!(Wallet::load(Settings::default(), dir.path(), db,).is_err());
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    }
}
