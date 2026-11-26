//! The seller's side of a paid request: whether to quote, voucher admission,
//! dispatch, billing and payment.
use super::*;
use crate::contract::mpp::{Bill, ChannelFunding, Request, Usage, Voucher};
use crate::{
    ledger::{Channel, ChannelState, Request as Row, State},
    protocol::{Intent, Step as Wire},
};
use mpp::request_intent;

/// Unpaid bills at which the seller refuses a buyer's new requests, as in
/// the Lightning plugin.
const STRIKE_LIMIT: u64 = 3;
/// At most this many buyers have standing; the set is cleared when full.
const STANDING_LIMIT: usize = 1024;

impl Engine {
    /// The buyer's request for `body_digest` that holds the pair's slot for
    /// that body; the unique index keeps it to at most one.
    pub(super) fn seller_agreement(&self, peer: &str, body_digest: &str) -> Result<Option<Row>> {
        Ok(self.ledger.mpp_active()?.into_iter().find(|r| {
            r.seller && r.peer == peer && r.holds_peer() && r.intent.body_digest == body_digest
        }))
    }
    /// The answer to a hello the seller cannot quote now: a refusal while the
    /// buyer's channel closes, and `Busy`, which the buyer retries, while the
    /// channel is still being opened, the same body is in flight, or the
    /// buyer has `STRIKE_LIMIT` unpaid bills.
    pub(super) fn seller_refusal(
        &self,
        peer: &str,
        channel: Option<&Channel>,
        body_digest: &str,
    ) -> Result<Option<Wire>> {
        if channel.is_some_and(|c| c.state != ChannelState::Open) {
            let reason = "the buyer's channel is closing; retry after its closure".into();
            return Ok(Some(Wire::QuoteRejected { reason }));
        }
        let held: Vec<_> = self
            .ledger
            .mpp_active()?
            .into_iter()
            .filter(|r| r.seller && r.peer == peer && r.holds_peer())
            .collect();
        let opening = channel.is_none() && !held.is_empty();
        let same = held.iter().any(|r| r.intent.body_digest == body_digest);
        let busy = opening || same || self.seller_strikes(peer, channel)? >= STRIKE_LIMIT;
        Ok(busy.then_some(Wire::Busy))
    }
    /// The buyer's unpaid bills: those on its open channel that no voucher
    /// covers yet, and those closed channels lost until the operator
    /// forgives them.
    pub(super) fn seller_strikes(&self, peer: &str, channel: Option<&Channel>) -> Result<u64> {
        let open = channel.filter(|c| c.state == ChannelState::Open).map_or(
            Ok(0),
            |c| -> Result<u64> {
                Ok(self
                    .ledger
                    .mpp_active()?
                    .iter()
                    .filter(|r| r.seller && r.channel.as_ref() == Some(&c.key))
                    .filter(|r| r.state == State::Billed)
                    .filter(|r| r.bill.as_ref().is_some_and(|b| b.running_total > c.cum()))
                    .count() as u64)
            },
        )?;
        Ok(open + self.ledger.lost_bills(peer)?)
    }
    /// Forgives the buyer's lost bills, so they no longer count as strikes;
    /// returns how many.
    pub async fn unblock(&self, peer: &str) -> Result<u64> {
        validate_peer(peer)?;
        let serial = self.peer_lock(peer)?;
        let _guard = serial.lock().await;
        let forgiven = self.ledger.forgive_lost_bills(peer)?;
        ensure!(
            forgiven > 0,
            "the peer has no lost bills; nothing to unblock"
        );
        Ok(forgiven)
    }
    /// Whether the buyer has paid this seller since it started.
    pub(super) fn seller_has_standing(&self, peer: &str) -> bool {
        self.standing.lock().unwrap().contains(peer)
    }
    fn seller_grant_standing(&self, peer: &str) {
        let mut standing = self.standing.lock().unwrap();
        if standing.len() >= STANDING_LIMIT && !standing.contains(peer) {
            standing.clear();
        }
        standing.insert(peer.into());
    }
    /// Tells the buyer that a request with unknown usage is never billed, so
    /// it closes its own request instead of waiting for a bill; repeated
    /// until the buyer acknowledges it.
    pub(super) async fn seller_unbilled(&self, row: &Row) -> Result<()> {
        if !row.superseded {
            // Once told, the buyer would ignore a bill for it.
            self.ledger.mpp_update(&row.id, |r| {
                r.superseded = true;
                Ok(())
            })?;
        }
        self.mpp_send(&row.peer, &row.id, Wire::Unbilled).await
    }
    pub(super) async fn seller_admit_voucher(
        &self,
        row: &Row,
        funding: ChannelFunding,
        voucher: Voucher,
    ) -> Result<()> {
        let terms = row.terms.clone().context("missing quote")?;
        ensure!(terms.expires_at > now(), "quote expired");
        ensure!(
            self.wallet_info.can_send,
            "MPP seller must unlock wallet for capture and refund"
        );
        let pair = &funding.terms;
        ensure!(
            pair.buyer_peer == row.peer
                && pair.seller_peer == self.node
                && pair.payer.eq_ignore_ascii_case(&row.intent.payer)
                && pair.payee.eq_ignore_ascii_case(&self.wallet_info.address)
                && pair.asset == self.wallet_info.asset,
            "channel terms differ from this buyer and seller"
        );
        let key = pair.salt()?;
        let existing = self.ledger.channel_active(&row.peer, true)?;
        match &existing {
            None => ensure!(
                terms.channel_id.is_none(),
                "quoted channel is no longer open"
            ),
            // Other requests may have been billed since the quote.
            Some(c) => ensure!(
                c.funding.as_ref() == Some(&funding)
                    && terms.channel_id.as_ref() == Some(&funding.channel_id)
                    && c.state == ChannelState::Open,
                "voucher channel differs from the quote"
            ),
        }
        // The voucher the seller already verified and stores needs no second
        // check: its signature and the open receipt cannot change, the
        // deposit only grows, and a close request moves the channel to
        // closing on the next watch tick, within the close deferral.
        let stored = existing
            .as_ref()
            .filter(|c| c.voucher.as_ref() == Some(&voucher));
        let deposit = match stored {
            Some(c) => c.deposit,
            None => {
                self.wallet
                    .execute(Request::VerifyVoucher {
                        funding: funding.clone(),
                        voucher: voucher.clone(),
                    })
                    .await?
                    .deposit
            }
        };
        // A newly verified nonzero voucher gives standing once admitted.
        let standing = stored.is_none() && voucher.cum > 0;
        ensure!(
            voucher.cum >= terms.cum_before.saturating_add(terms.upfront),
            "voucher does not cover the quoted voucher and upfront"
        );
        let admit = |r: &mut Row, c: &mut Channel| {
            ensure!(
                r.state == State::Negotiating,
                "request consumed concurrently"
            );
            if c.voucher.is_none() || voucher.cum > c.cum() {
                c.voucher = Some(voucher);
            }
            c.deposit = deposit;
            c.active_at = now();
            r.channel = Some(key.clone());
            r.cum_after = Some(c.cum());
            r.state = State::Ready;
            Ok(())
        };
        if existing.is_none() {
            let mut channel = Channel::new(pair.clone(), row.peer.clone(), true, deposit)?;
            channel.funding = Some(funding);
            channel.state = ChannelState::Open;
            self.ledger.mpp_insert_channel(&row.id, &channel, admit)?;
        } else {
            self.ledger.mpp_update_both(&row.id, &key, 0, admit)?;
        }
        if standing {
            self.seller_grant_standing(&row.peer);
        }
        self.mpp_send(&row.peer, &row.id, Wire::Ready).await
    }
    pub(super) async fn seller_payment(
        &self,
        row: &Row,
        funding: ChannelFunding,
        voucher: Voucher,
    ) -> Result<()> {
        let key = row.channel.clone().context("missing channel")?;
        let channel = self.ledger.channel_get(&key)?.context("unknown channel")?;
        ensure!(
            channel.funding.as_ref() == Some(&funding),
            "voucher for another channel"
        );
        // A closing channel keeps the voucher its capture was fixed with.
        if channel.state != ChannelState::Open || voucher.cum <= channel.cum() {
            return Ok(());
        }
        let deposit = self
            .wallet
            .execute(Request::VerifyVoucher {
                funding,
                voucher: voucher.clone(),
            })
            .await?
            .deposit;
        self.seller_grant_standing(&row.peer);
        let running_total = row.bill.as_ref().context("missing bill")?.running_total;
        self.ledger.mpp_update_both(&row.id, &key, 0, |r, c| {
            if c.state == ChannelState::Open && voucher.cum > c.cum() {
                c.voucher = Some(voucher);
                c.deposit = deposit;
            }
            if r.state == State::Billed && c.cum() >= running_total {
                r.state = State::Closed;
                r.cum_after = Some(c.cum());
            }
            Ok(())
        })
    }
    /// Sends the bill until a voucher covers it, or closes the channel once
    /// the payment time is over.
    pub(super) async fn seller_collect(&self, row: Row) -> Result<()> {
        let bill = row.bill.clone().context("missing bill")?;
        let sent = self
            .mpp_send(&row.peer, &row.id, Wire::Bill(bill.clone()))
            .await;
        // The payment time starts with the first bill the buyer could receive.
        let billed_at = match (&sent, row.billed_at) {
            (Ok(()), 0) => now(),
            (_, at) => at,
        };
        let unpaid =
            billed_at > 0 && now() >= billed_at.saturating_add(channel::BILL_PAYMENT_SECONDS);
        let key = row.channel.clone().context("missing channel")?;
        self.ledger.mpp_update_both(&row.id, &key, 0, |r, c| {
            ensure!(r.state == State::Billed, "request changed concurrently");
            r.billed_at = billed_at;
            // Only a bill the buyer could receive is settled, even from credit.
            if billed_at > 0 && c.cum() >= bill.running_total {
                r.state = State::Closed;
                r.cum_after = Some(c.cum());
            } else if unpaid {
                r.state = State::Closed;
                r.error = Some("bill unpaid; the channel is closed with the best voucher".into());
                c.lost_bills += 1;
                channel::start_close(c, "a bill is unpaid");
            }
            Ok(())
        })?;
        sent
    }
    pub(super) async fn mpp_seller_hook(&self, event: &Value) -> Result<OpenAiExchangeDecision> {
        let result = self.mpp_admit_seller(event).await;
        Ok(match result {
            Ok(()) => allow(),
            Err(error) => deny(&error.to_string()),
        })
    }
    async fn mpp_admit_seller(&self, event: &Value) -> Result<()> {
        let peer = event["requested_by_node_id"]
            .as_str()
            .context("missing authenticated buyer")?;
        // The body digest binds the forwarded request to the agreed bill cap.
        let intent = request_intent(event, "")?;
        let row = self
            .seller_agreement(peer, &intent.body_digest)?
            .context("negotiate MPP payment before inference")?;
        let id = row.id.as_str();
        let intent = Intent {
            payer: row.intent.payer.clone(),
            ..intent
        };
        ensure!(
            row.intent == intent,
            "inference differs from approved request"
        );
        let observation = serving::correlation(event)?;
        if row.state == State::Running && row.observation.as_deref() == Some(observation) {
            return Ok(());
        }
        ensure!(
            row.state == State::Ready,
            "payment not ready or request already executed"
        );
        ensure!(
            self.wallet_info.can_send,
            "MPP seller must unlock wallet for capture and refund"
        );
        ensure!(
            row.terms.as_ref().is_some_and(|t| t.expires_at > now()),
            "quote expired"
        );
        // The voucher was verified on chain when the request became ready.
        let key = row.channel.clone().context("missing channel")?;
        self.ledger.mpp_update_both(id, &key, 0, |r, c| {
            ensure!(r.state == State::Ready, "request consumed concurrently");
            ensure!(c.state == ChannelState::Open, "channel is closing");
            r.state = State::Running;
            r.observation = Some(observation.into());
            c.active_at = now();
            Ok(())
        })
    }
}
/// Bills known terminal usage on the seller's open channel, numbering the
/// bill; unknown usage is never billed as zero. Returns whether the request
/// can progress.
pub(super) fn bill(r: &mut Row, c: &mut Channel, usage: Option<Usage>) -> Result<bool> {
    if let Some(usage) = usage {
        ensure!(
            r.usage.is_none_or(|old| old == usage),
            "terminal usage changed"
        );
        r.usage = Some(usage);
        if r.state == State::Closed || r.bill.is_some() {
            return Ok(false);
        }
        if r.superseded {
            r.state = State::Closed;
            r.error = Some("usage arrived after the channel moved on; not billed".into());
            return Ok(false);
        }
        // The capture was fixed when closing started.
        if c.state != ChannelState::Open {
            r.state = State::Closed;
            r.error = Some("usage arrived after the channel began closing; not billed".into());
            return Ok(false);
        }
        ensure!(
            matches!(r.state, State::Running | State::UsageUnknown),
            "request was not dispatched"
        );
        let cost = r
            .terms
            .as_ref()
            .context("missing agreed rates")?
            .bill(usage);
        let running_total = c.running_total.saturating_add(cost);
        r.bill = Some(Bill {
            seq: c.bills,
            usage: Some(usage),
            cost,
            running_total,
        });
        r.state = State::Billed;
        c.running_total = running_total;
        c.bills += 1;
        c.active_at = now();
    } else if r.state == State::Running {
        r.state = State::UsageUnknown;
        r.error =
            Some("terminal usage unavailable; no zero bill invented; operator may waive".into());
    }
    Ok(r.state != State::Closed)
}
