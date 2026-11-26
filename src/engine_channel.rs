//! The lifecycle of each buyer-seller channel: buyer open, top-up and
//! recovery; seller watch and close.
use super::*;
use crate::contract::mpp::{ChannelTerms, Closure, Request, RequestTerms, Voucher};
use crate::ledger::{Channel, ChannelState, Request as Row, State};

/// Seconds a seller waits after first sending a bill for a voucher covering it.
pub(super) const BILL_PAYMENT_SECONDS: u64 = 120;
/// After the buyer's close request, a running request may still be billed
/// for this long, well inside the 900 s grace before the buyer can withdraw.
const CLOSE_DEFER_SECONDS: u64 = 600;
/// A buyer stops waiting for the bill of a dispatched request this long
/// after its quote expired.
pub(super) const UNBILLED_SECONDS: u64 = 3600;
/// A seller closes a channel that served no request for this long.
const SELLER_IDLE_SECONDS: u64 = 3600;
/// A buyer recovers a channel that served no request for this long.
const BUYER_IDLE_SECONDS: u64 = 7200;
/// Every open seller channel is inspected at least this often, as a backstop
/// against gaps in the close-event logs, well inside the 900 s grace.
const WATCH_BACKSTOP_SECONDS: u64 = 120;
/// Blocks each close-event scan reads again before its cursor.
const CLOSE_LOG_OVERLAP: u64 = 16;

/// Which open seller channels the retry tick inspects on chain.
#[derive(Default)]
pub(super) struct Watch {
    /// Channels named in close events or seen with a close request; they are
    /// inspected every tick until an inspection shows no close request.
    pub(super) pending: std::collections::BTreeSet<String>,
    /// When every channel was last inspected.
    pub(super) polled_at: u64,
}

impl Engine {
    /// The open seller channels this tick inspects, or `None` for all of them:
    /// those named in close events since the last complete scan and those
    /// still pending. Every channel is inspected on a backstop tick and after
    /// a failed or skipped scan, which leaves the cursor where it was.
    pub(super) async fn seller_watch_list(
        &self,
        open: &[Channel],
    ) -> Result<Option<std::collections::BTreeSet<String>>> {
        if open.is_empty() {
            return Ok(Some(Default::default()));
        }
        let from = self.ledger.close_cursor()?;
        let from = from.map(|block| block.saturating_sub(CLOSE_LOG_OVERLAP));
        let named = match self.wallet.close_events(from).await {
            Ok(events) => {
                self.ledger.set_close_cursor(events.to)?;
                (!events.skipped).then_some(events.channels)
            }
            Err(error) => {
                tracing::warn!("close event scan failed; inspecting every channel: {error}");
                None
            }
        };
        let mut watch = self.watch.lock().unwrap();
        let backstop = now() >= watch.polled_at.saturating_add(WATCH_BACKSTOP_SECONDS);
        let Some(named) = named.filter(|_| !backstop) else {
            watch.polled_at = now();
            return Ok(None);
        };
        for c in open {
            let id = c
                .funding
                .as_ref()
                .map(|f| f.channel_id.to_ascii_lowercase());
            if id.is_some_and(|id| named.contains(&id)) {
                watch.pending.insert(c.key.clone());
            }
        }
        Ok(Some(
            open.iter()
                .filter(|c| watch.pending.contains(&c.key))
                .map(|c| c.key.clone())
                .collect(),
        ))
    }
    /// Whether the seller's idle close may be due, which needs no chain read
    /// to decide.
    pub(super) fn seller_idle_due(c: &Channel) -> bool {
        now() >= c.active_at.saturating_add(SELLER_IDLE_SECONDS)
    }
    /// Keeps a channel with a close request on the per-tick list.
    fn watch_close_request(&self, key: &str, requested: bool) {
        let mut watch = self.watch.lock().unwrap();
        if requested {
            watch.pending.insert(key.into());
        } else {
            watch.pending.remove(key);
        }
    }
    pub(crate) async fn channel_retry_one(&self, key: &str) -> Result<()> {
        let channel = self.ledger.channel_get(key)?.context("unknown channel")?;
        let serial = self.peer_lock(&channel.peer)?;
        let _guard = serial.lock().await;
        self.channel_progress(key).await
    }
    /// Drives pending chain work and idle timeouts of one channel.
    pub(super) async fn channel_progress(&self, key: &str) -> Result<()> {
        let channel = self.ledger.channel_get(key)?.context("unknown channel")?;
        match (channel.seller, &channel.state) {
            (_, ChannelState::Closed) | (true, ChannelState::Opening) => {}
            (false, ChannelState::Opening) => {
                self.buyer_open(&channel).await?;
            }
            (false, ChannelState::Open) => {
                if now() >= channel.active_at.saturating_add(BUYER_IDLE_SECONDS) {
                    self.ledger.channel_update(key, |c| {
                        c.state = ChannelState::Closing;
                        c.error = Some("idle; recovering the deposit".into());
                        Ok(())
                    })?;
                    self.buyer_recover(key).await?;
                }
            }
            (false, ChannelState::Closing) => self.buyer_recover(key).await?,
            (true, ChannelState::Open) => self.seller_watch(channel).await?,
            (true, ChannelState::Closing) => self.seller_close(channel).await?,
        }
        self.changed.notify_waiters();
        Ok(())
    }
    /// Closes the buyer's open channel with the seller `peer`, named by its
    /// node ID or a unique prefix of at least eight characters, or without
    /// one the only open channel, and returns the seller. The close request
    /// on chain makes the seller close; otherwise the buyer withdraws after
    /// the grace.
    pub async fn buyer_close(&self, peer: Option<&str>) -> Result<String> {
        let mut open: Vec<Channel> = self
            .ledger
            .channels_active()?
            .into_iter()
            .filter(|c| !c.seller && matches!(c.state, ChannelState::Open | ChannelState::Closing))
            .collect();
        if let Some(peer) = peer {
            ensure!(
                peer.len() >= 8,
                "name the seller by at least eight characters of its node ID"
            );
            let peer = peer.to_ascii_lowercase();
            open.retain(|c| c.peer.starts_with(&peer));
        }
        ensure!(!open.is_empty(), "no open channel to close");
        ensure!(
            open.len() == 1,
            "several open channels; name the seller with peer"
        );
        let channel = open.remove(0);
        let serial = self.peer_lock(&channel.peer)?;
        let _guard = serial.lock().await;
        self.ledger.channel_update(&channel.key, |c| {
            ensure!(
                matches!(c.state, ChannelState::Open | ChannelState::Closing),
                "channel is not open"
            );
            c.state = ChannelState::Closing;
            Ok(())
        })?;
        self.channel_progress(&channel.key).await?;
        Ok(channel.peer)
    }
    /// The buyer's open channel with the quoting seller, opening one when
    /// none exists. `None` means the open is pending or the request was refused.
    pub(super) async fn buyer_channel(
        &self,
        row: &Row,
        terms: &RequestTerms,
    ) -> Result<Option<Channel>> {
        let channel = match self.ledger.channel_active(&row.peer, false)? {
            Some(channel) => channel,
            None => {
                let pair = ChannelTerms {
                    buyer_peer: self.node.clone(),
                    seller_peer: row.peer.clone(),
                    payer: self.wallet_info.address.clone(),
                    payee: terms.payee.clone(),
                    asset: terms.asset.clone(),
                    nonce: hex::encode(rand::random::<[u8; 32]>()),
                };
                let deposit = self.config.channel_topup;
                let channel = Channel::new(pair, row.peer.clone(), false, deposit)?;
                self.ledger.channel_insert(&channel)?;
                channel
            }
        };
        match channel.state {
            ChannelState::Open => Ok(Some(channel)),
            ChannelState::Opening => {
                let opened = self.buyer_open(&channel).await?;
                if opened.is_none()
                    && self
                        .ledger
                        .channel_get(&channel.key)?
                        .is_some_and(|c| c.state == ChannelState::Closed)
                {
                    let reason = "the channel open expired unsent or reverted";
                    self.buyer_refuse(row, reason).await?;
                }
                Ok(opened)
            }
            _ => {
                let reason = "the channel is closing; retry after its closure";
                self.buyer_refuse(row, reason).await?;
                Ok(None)
            }
        }
    }
    /// Drives the open to finality; `None` while it is pending or after it failed.
    async fn buyer_open(&self, channel: &Channel) -> Result<Option<Channel>> {
        let response = self
            .wallet
            .execute(Request::Open {
                terms: channel.terms.clone(),
                deposit: channel.deposit,
                max_fee: self.config.wallet.max_fee,
            })
            .await?;
        if response.unfunded {
            self.ledger.channel_update(&channel.key, |c| {
                c.state = ChannelState::Closed;
                c.error =
                    Some("channel open expired unsent or reverted; nothing was deposited".into());
                Ok(())
            })?;
            return Ok(None);
        }
        let Some(funding) = response.funding else {
            return Ok(None);
        };
        ensure!(
            funding.terms == channel.terms,
            "wallet changed channel terms"
        );
        let opened = self.ledger.channel_update(&channel.key, |c| {
            c.funding = Some(funding);
            c.deposit = response.deposit;
            c.state = ChannelState::Open;
            c.active_at = now();
            Ok(c.clone())
        })?;
        Ok(Some(opened))
    }
    /// The channel once its deposit fits `cum`, topping it up once when
    /// needed. `None` means the top-up is pending or the request was refused.
    pub(super) async fn buyer_capacity(
        &self,
        row: &Row,
        channel: Channel,
        cum: u64,
    ) -> Result<Option<Channel>> {
        if cum <= channel.deposit {
            return Ok(Some(channel));
        }
        let amount = self.config.channel_topup;
        if cum > channel.deposit.saturating_add(amount) {
            let reason = "one channel top-up cannot cover the next voucher";
            self.buyer_refuse(row, reason).await?;
            return Ok(None);
        }
        let index = channel.topups + 1;
        let response = self
            .wallet
            .execute(Request::TopUp {
                funding: channel.funding.clone().context("channel is not open")?,
                amount,
                index,
                max_fee: self.config.wallet.max_fee,
            })
            .await?;
        if let Some(closure) = response.closure {
            self.buyer_record_close(&channel, closure)?;
            self.buyer_refuse(row, "the channel closed before its top-up")
                .await?;
            return Ok(None);
        }
        if response.pending {
            return Ok(None);
        }
        let channel = self.ledger.channel_update(&channel.key, |c| {
            ensure!(c.topups + 1 == index, "top-up recorded concurrently");
            c.topups = index;
            c.deposit = response.deposit;
            Ok(c.clone())
        })?;
        Ok(Some(channel))
    }
    /// The channel's voucher for `cum`, signing a new one only when it grows.
    pub(super) async fn buyer_voucher(&self, channel: &Channel, cum: u64) -> Result<Voucher> {
        if let Some(voucher) = channel.voucher.as_ref().filter(|v| v.cum == cum) {
            return Ok(voucher.clone());
        }
        self.wallet
            .execute(Request::SignVoucher {
                funding: channel.funding.clone().context("channel is not open")?,
                cum,
            })
            .await?
            .voucher
            .context("wallet returned no voucher")
    }
    async fn buyer_recover(&self, key: &str) -> Result<()> {
        let channel = self.ledger.channel_get(key)?.context("unknown channel")?;
        let response = self
            .wallet
            .execute(Request::Recover {
                funding: channel.funding.clone().context("channel never opened")?,
                max_fee: self.config.wallet.max_fee,
            })
            .await?;
        if let Some(closure) = response.closure {
            self.buyer_record_close(&channel, closure)?;
        }
        Ok(())
    }
    pub(super) fn buyer_record_close(&self, channel: &Channel, closure: Closure) -> Result<()> {
        let reason = "the payment channel closed";
        self.ledger.channel_close(&channel.key, reason, |c| {
            ensure!(
                c.funding
                    .as_ref()
                    .is_some_and(|f| f.channel_id == closure.channel_id),
                "closure for another channel"
            );
            ensure!(
                c.closure.as_ref().is_none_or(|old| *old == closure),
                "closure changed"
            );
            if closure.captured > c.running_total {
                c.error = Some(format!(
                    "seller captured {} but the buyer acknowledged bills of {}",
                    closure.captured, c.running_total
                ));
            }
            c.closure = Some(closure);
            c.state = ChannelState::Closed;
            Ok(())
        })
    }
    /// Verifies the seller's close notice on chain before recording it.
    pub(super) async fn buyer_closed_notice(
        &self,
        peer: &str,
        channel_id: &str,
        closure: Closure,
    ) -> Result<()> {
        let Some(channel) = self.ledger.channel_active(peer, false)? else {
            return Ok(());
        };
        let Some(funding) = channel
            .funding
            .clone()
            .filter(|f| f.channel_id == channel_id)
        else {
            return Ok(());
        };
        let verified = self
            .wallet
            .execute(Request::Inspect { funding })
            .await?
            .closure
            .context("close not finalized")?;
        ensure!(
            verified == closure,
            "close notice differs from chain evidence"
        );
        self.buyer_record_close(&channel, verified)?;
        self.changed.notify_waiters();
        Ok(())
    }
    /// Closes the channel after a close request or idle time, or records the
    /// buyer's withdrawal.
    async fn seller_watch(&self, channel: Channel) -> Result<()> {
        let status = self
            .wallet
            .execute(Request::Inspect {
                funding: channel.funding.clone().context("missing funding")?,
            })
            .await?;
        // The 600 s deferral is checked again on every tick.
        self.watch_close_request(
            &channel.key,
            status.closure.is_none() && status.close_requested_at > 0,
        );
        if let Some(closure) = status.closure {
            return self.seller_finish(&channel, closure).await;
        }
        // The capture is fixed when closing starts, so a running request is
        // billed first; its usage is known or unknown once it ends.
        let running = self.ledger.mpp_active()?.iter().any(|r| {
            r.seller && r.state == State::Running && r.channel.as_ref() == Some(&channel.key)
        });
        let reason = if status.close_requested_at > 0 {
            if running
                && now()
                    < status
                        .close_requested_at
                        .saturating_add(CLOSE_DEFER_SECONDS)
            {
                return Ok(());
            }
            "the buyer requested close"
        } else if !running && now() >= channel.active_at.saturating_add(SELLER_IDLE_SECONDS) {
            "idle"
        } else {
            return Ok(());
        };
        self.seller_start_close(&channel.key, reason)?;
        let channel = self
            .ledger
            .channel_get(&channel.key)?
            .context("unknown channel")?;
        self.seller_close(channel).await
    }
    fn seller_start_close(&self, key: &str, reason: &str) -> Result<()> {
        self.ledger.channel_update(key, |c| {
            start_close(c, reason);
            Ok(())
        })
    }
    async fn seller_close(&self, channel: Channel) -> Result<()> {
        let response = self
            .wallet
            .execute(Request::Close {
                funding: channel.funding.clone().context("missing funding")?,
                voucher: channel.voucher.clone().filter(|v| v.cum > 0),
                capture: channel.capture.context("missing capture")?,
            })
            .await?;
        if let Some(closure) = response.closure {
            self.seller_finish(&channel, closure).await?;
        }
        Ok(())
    }
    async fn seller_finish(&self, channel: &Channel, closure: Closure) -> Result<()> {
        let reason = "the payment channel closed before this request was billed and paid";
        // Bills that neither the capture nor the seller's best voucher covers
        // count as strikes; a missed withdrawal is not the buyer's fault.
        let covered = closure.captured.max(channel.cum());
        let lost = self
            .ledger
            .mpp_active()?
            .iter()
            .filter(|r| r.seller && r.channel.as_ref() == Some(&channel.key))
            .filter(|r| r.state == State::Billed)
            .filter(|r| r.bill.as_ref().is_some_and(|b| b.running_total > covered))
            .count() as u64;
        self.ledger.channel_close(&channel.key, reason, |c| {
            ensure!(
                c.funding
                    .as_ref()
                    .is_some_and(|f| f.channel_id == closure.channel_id),
                "closure for another channel"
            );
            ensure!(
                c.closure.as_ref().is_none_or(|old| *old == closure),
                "closure changed"
            );
            // A seller offline for the whole grace never fixed a capture, but
            // still earned what its best voucher covers of the bills.
            let earned = c
                .capture
                .unwrap_or_else(|| c.cum().min(c.running_total));
            if closure.captured < earned {
                c.error = Some(format!(
                    "the buyer withdrew before the seller's capture: {} earned, {} captured; no second capture will be attempted",
                    earned, closure.captured
                ));
            }
            if c.closure.is_none() {
                c.lost_bills += lost;
            }
            c.closure = Some(closure.clone());
            c.state = ChannelState::Closed;
            Ok(())
        })?;
        self.send(
            &channel.peer,
            &Message::Closed {
                version: MPP_VERSION,
                channel_id: closure.channel_id.clone(),
                closure,
            },
        )
        .await
    }
}
/// Moves an open seller channel to closing, fixing the capture at the bills
/// the best voucher covers.
pub(super) fn start_close(c: &mut Channel, reason: &str) {
    if c.state == ChannelState::Open {
        c.state = ChannelState::Closing;
        c.capture = Some(c.cum().min(c.running_total));
        c.error = Some(format!("closing: {reason}"));
    }
}
