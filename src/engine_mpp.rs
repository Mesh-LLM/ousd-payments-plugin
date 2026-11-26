use super::*;
use crate::contract::mpp::{Bill, RequestTerms, Usage};
use crate::{
    ledger::{Channel, ChannelState, Request as Row, State},
    protocol::{Intent, Step as Wire},
};
use admission::SERVED_FREE;
use sha2::{Digest, Sha256};

impl Engine {
    pub(super) async fn mpp_send(&self, peer: &str, id: &str, message: Wire) -> Result<()> {
        self.send(
            peer,
            &Message::Mpp {
                version: crate::protocol::MPP_VERSION,
                id: id.into(),
                message: Box::new(message),
            },
        )
        .await
    }
    pub(super) async fn mpp_message(&self, peer: &str, id: &str, message: Wire) -> Result<()> {
        ensure!(
            uuid::Uuid::parse_str(id).is_ok(),
            "MPP request ID must be a UUID"
        );
        if let Wire::Hello { intent } = message {
            return self.mpp_hello(peer, id, intent).await;
        }
        let row = self.ledger.mpp_get(id)?.context("unknown MPP request")?;
        ensure!(row.peer == peer, "wrong MPP peer");
        if row.state == State::Closed {
            // A repeated bill means the voucher that paid it was lost.
            if let Wire::Bill(bill) = &message
                && !row.seller
                && row.bill.as_ref() == Some(bill)
            {
                self.buyer_resend_voucher(&row).await?;
            }
            // A repeated notice means the acknowledgement was lost.
            if matches!(message, Wire::Unbilled) && !row.seller {
                self.mpp_send(peer, &row.id, Wire::UnbilledSeen).await?;
            }
            return Ok(());
        }
        match message {
            Wire::QuoteRejected { reason } => {
                ensure!(
                    !row.seller && row.state == State::Negotiating && row.terms.is_none(),
                    "quote refusal after agreement"
                );
                self.mpp_close(&row.id, &reason)?;
            }
            Wire::Unpriced => {
                ensure!(
                    !row.seller && row.state == State::Negotiating && row.terms.is_none(),
                    "unpriced notice after agreement"
                );
                self.mpp_close(&row.id, SERVED_FREE)?;
            }
            Wire::Busy => {
                ensure!(
                    !row.seller && row.state == State::Negotiating && row.terms.is_none(),
                    "busy notice after agreement"
                );
                self.ledger.mpp_update(&row.id, |r| {
                    r.busy = true;
                    Ok(())
                })?;
            }
            Wire::Unbilled => {
                ensure!(
                    !row.seller
                        && matches!(row.state, State::Running | State::UsageUnknown)
                        && row.bill.is_none(),
                    "unexpected unbilled notice"
                );
                // Nothing was billed, so the channel view stays as it is.
                self.mpp_close(
                    &row.id,
                    "the seller has no usage for this request and never bills it; not paid",
                )?;
                self.mpp_send(peer, &row.id, Wire::UnbilledSeen).await?;
            }
            Wire::UnbilledSeen => {
                ensure!(row.seller, "unexpected unbilled acknowledgement");
                self.ledger.mpp_update(&row.id, |r| {
                    r.unbilled_seen = true;
                    Ok(())
                })?;
            }
            Wire::Quote { terms } => {
                ensure!(!row.seller, "unexpected quote");
                self.buyer_quote(&row, terms).await?;
            }
            Wire::Voucher { funding, voucher } => {
                ensure!(row.seller, "unexpected voucher");
                match row.state {
                    State::Negotiating => self.seller_admit_voucher(&row, funding, voucher).await?,
                    State::Ready => self.mpp_send(peer, &row.id, Wire::Ready).await?,
                    State::Billed => self.seller_payment(&row, funding, voucher).await?,
                    _ => bail!("voucher while the request runs"),
                }
            }
            Wire::Ready => {
                ensure!(
                    !row.seller && row.cum_after.is_some(),
                    "ready before this request's voucher"
                );
                self.ledger.mpp_update(&row.id, |r| {
                    if r.state == State::Negotiating {
                        r.state = State::Ready;
                    }
                    Ok(())
                })?;
            }
            Wire::Bill(bill) => {
                ensure!(
                    !row.seller && matches!(row.state, State::Running | State::UsageUnknown),
                    "unexpected bill"
                );
                self.ledger.mpp_update(&row.id, |r| {
                    ensure!(
                        r.bill.as_ref().is_none_or(|old| *old == bill),
                        "bill changed"
                    );
                    r.bill = Some(bill);
                    Ok(())
                })?;
                self.mpp_progress(&row.id).await?;
            }
            Wire::Cancel => {
                ensure!(row.seller, "only buyer can cancel peer work");
                ensure!(
                    matches!(row.state, State::Negotiating | State::Ready),
                    "request already dispatched; await usage or explicitly waive unknown usage"
                );
                self.mpp_close(
                    &row.id,
                    "buyer cancelled the unstarted request; any voucher stays as channel credit",
                )?;
            }
            Wire::Hello { .. } => unreachable!(),
        }
        self.changed.notify_waiters();
        Ok(())
    }
    pub(super) fn mpp_close(&self, id: &str, reason: &str) -> Result<()> {
        self.ledger.mpp_update(id, |r| {
            r.state = State::Closed;
            r.error = Some(reason.chars().take(512).collect());
            Ok(())
        })
    }
    async fn mpp_hello(&self, peer: &str, id: &str, intent: Intent) -> Result<()> {
        if let Some(old) = self.ledger.mpp_get(id)? {
            ensure!(
                old.seller && old.peer == peer && old.intent == intent,
                "request ID reused"
            );
            ensure!(
                self.wallet_info.can_send,
                "MPP seller must unlock wallet for capture and refund"
            );
            let terms = old.terms.context("missing quote")?;
            return self.mpp_send(peer, id, Wire::Quote { terms }).await;
        }
        let Some(price) = self.ledger.paid_price(&intent.model)? else {
            return self.mpp_send(peer, id, Wire::Unpriced).await;
        };
        ensure!(
            self.wallet_info.can_send,
            "MPP seller must unlock wallet for capture and refund"
        );
        let channel = self.ledger.channel_active(peer, true)?;
        // Repeat the last close notice, in case the buyer still uses that channel.
        if channel.is_none()
            && let Some(closure) = self
                .ledger
                .channel_last_closed(peer, true)?
                .and_then(|c| c.closure)
        {
            let channel_id = closure.channel_id.clone();
            let notice = Message::Closed {
                version: MPP_VERSION,
                channel_id,
                closure,
            };
            self.send(peer, &notice).await?;
        }
        // Repeat unacknowledged notices of requests that are never billed.
        for old in self.ledger.mpp_active()? {
            if old.seller
                && old.peer == peer
                && old.state == State::UsageUnknown
                && !old.unbilled_seen
            {
                self.seller_unbilled(&old).await?;
            }
        }
        if let Some(answer) = self.seller_refusal(peer, channel.as_ref(), &intent.body_digest)? {
            return self.mpp_send(peer, id, answer).await;
        }
        let terms = RequestTerms {
            request_id: id.into(),
            buyer_peer: peer.into(),
            seller_peer: self.node.clone(),
            model: intent.model.clone(),
            body_digest: intent.body_digest.clone(),
            payee: self.wallet_info.address.clone(),
            asset: self.wallet_info.asset.clone(),
            input_per_million: price.input_per_million,
            output_per_million: price.output_per_million,
            cache_discount_percent: price.cache_discount_percent,
            output_limit: intent.output_limit,
            input_bytes: intent.input_bytes,
            // Only a buyer that has not paid since the seller started pays
            // upfront, and only to open a channel.
            upfront: if channel.is_some() || self.seller_has_standing(peer) {
                0
            } else {
                self.ledger.model_admission_deposit(&intent.model)?
            },
            cum_before: channel.as_ref().map_or(0, Channel::cum),
            cost_before: channel.as_ref().map_or(0, |c| c.running_total),
            channel_id: channel
                .as_ref()
                .and_then(|c| c.funding.as_ref())
                .map(|f| f.channel_id.clone()),
            expires_at: now() + 600,
        };
        terms.validate()?;
        // Bills stay in order: a request with unknown usage is never billed
        // once a later one is quoted.
        for old in self.ledger.mpp_active()? {
            if old.seller && old.peer == peer && old.state == State::UsageUnknown {
                self.ledger.mpp_update(&old.id, |r| {
                    r.superseded = true;
                    Ok(())
                })?;
            }
        }
        let mut row = Row::new(id.into(), peer.into(), true, intent);
        row.terms = Some(terms.clone());
        self.ledger.mpp_insert(&row)?;
        self.mpp_send(peer, id, Wire::Quote { terms }).await
    }
    fn mpp_check_terms(&self, row: &Row, terms: &RequestTerms) -> Result<()> {
        terms.validate()?;
        ensure!(
            terms.request_id == row.id
                && terms.buyer_peer == self.node
                && terms.seller_peer == row.peer
                && terms.model == row.intent.model
                && terms.body_digest == row.intent.body_digest
                && terms.asset == self.wallet_info.asset
                && terms.output_limit == row.intent.output_limit
                && terms.input_bytes == row.intent.input_bytes,
            "quote does not match explicit request approval"
        );
        // Without a ceiling, the worst-case cost checks bound the rates.
        if let Some(ceiling) = self.config.buy_prices.get(&terms.model) {
            ensure!(
                terms.input_per_million <= ceiling.input_per_million
                    && terms.output_per_million <= ceiling.output_per_million
                    && terms.cache_discount_percent >= ceiling.cache_discount_percent,
                "rates exceed buyer policy"
            );
        }
        ensure!(
            terms.upfront <= self.ledger.admission_deposit()?,
            "upfront exceeds the buyer's admission deposit"
        );
        ensure!(
            self.config.cap == 0 || terms.upfront <= self.config.cap,
            "upfront exceeds the per-request cap"
        );
        ensure!(
            terms.expires_at > now() && terms.expires_at <= now() + 600,
            "invalid quote expiry"
        );
        Ok(())
    }
    /// Whether the quote shows the buyer's channel, no more bills than its own
    /// accepted bills and other requests account for, and no voucher above
    /// the buyer's own. Otherwise the buyer stops using its channel and
    /// recovers the deposit. A quote may count fewer bills than the buyer
    /// accepted since, so only the upper bound is checked, also when the
    /// check repeats while a top-up is pending.
    fn buyer_check_view(&self, row: &Row, terms: &RequestTerms) -> Result<()> {
        let channel = self.ledger.channel_active(&row.peer, false)?;
        let shows = match &channel {
            None => terms.channel_id.is_none() && terms.cost_before == 0 && terms.cum_before == 0,
            // The seller may lack the buyer's latest voucher, which the buyer
            // then resends; a seller that never got the first one quotes no
            // channel, which is right only before any bill. Its bills may
            // include ones still on their way to the buyer.
            Some(c) => {
                let id = c.funding.as_ref().map(|f| &f.channel_id);
                let unseen = terms.channel_id.is_none() && terms.cum_before == 0;
                let billed = c.running_total;
                let pending = self.buyer_pending(&row.id, c)?;
                c.state != ChannelState::Closing
                    && c.terms.payee.eq_ignore_ascii_case(&terms.payee)
                    && terms.cost_before <= billed.saturating_add(pending)
                    && (terms.channel_id.as_ref() == id || unseen && billed == 0)
                    && terms.cum_before <= c.cum()
            }
        };
        if shows {
            return Ok(());
        }
        if let Some(c) = channel.filter(|c| c.state == ChannelState::Open) {
            self.ledger.channel_update(&c.key, |c| {
                c.state = ChannelState::Closing;
                c.error = Some("the seller's view of the channel differs; recovering it".into());
                Ok(())
            })?;
        }
        bail!("the seller's view of the channel differs from the buyer's")
    }
    /// What the buyer's other unsettled requests on `channel` may still be
    /// billed: a bill it holds, else the bill for its own usage, else the
    /// worst case.
    fn buyer_pending(&self, id: &str, channel: &Channel) -> Result<u64> {
        Ok(self
            .ledger
            .mpp_active()?
            .iter()
            .filter(|r| !r.seller && r.id != id && r.channel.as_ref() == Some(&channel.key))
            .filter_map(|r| {
                let terms = r.terms.as_ref()?;
                Some(match (&r.bill, r.usage) {
                    (Some(bill), _) => bill.cost,
                    (None, Some(usage)) => terms.bill(usage),
                    (None, None) => terms.worst_case(),
                })
            })
            .fold(0, u64::saturating_add))
    }
    async fn buyer_quote(&self, row: &Row, terms: RequestTerms) -> Result<()> {
        if let Some(old) = &row.terms {
            ensure!(*old == terms, "MPP quote changed");
            return self.mpp_progress(&row.id).await;
        }
        ensure!(row.state == State::Negotiating, "unexpected quote");
        if let Err(error) = self.mpp_check_terms(row, &terms) {
            return self.buyer_refuse(row, &error.to_string()).await;
        }
        self.ledger.mpp_update(&row.id, |r| {
            ensure!(r.terms.is_none(), "MPP quote changed");
            r.terms = Some(terms);
            Ok(())
        })?;
        self.mpp_progress(&row.id).await
    }
    /// Closes a request the buyer will not pay and tells the seller if it has not started.
    pub(super) async fn buyer_refuse(&self, row: &Row, reason: &str) -> Result<()> {
        self.mpp_close(&row.id, reason)?;
        if matches!(row.state, State::Negotiating | State::Ready) {
            self.mpp_send(&row.peer, &row.id, Wire::Cancel).await?;
        }
        self.changed.notify_waiters();
        Ok(())
    }
    /// The policy's daily budget, or 0 when it allows no paid buying.
    fn daily_budget(&self) -> Result<u64> {
        Ok(self.ledger.policy()?.budget().unwrap_or(0))
    }
    /// `payment` is upfront plus voucher increment for one request, and
    /// `increment` is what it signs now, within the daily `budget`.
    fn buyer_check_spend(&self, payment: u64, increment: u64, budget: u64) -> Result<()> {
        ensure!(
            self.config.cap == 0 || payment <= self.config.cap,
            "payment exceeds the per-request cap"
        );
        ensure!(
            self.ledger.spent_today()?.saturating_add(increment) <= budget,
            "daily budget exceeded"
        );
        Ok(())
    }
    /// The checks before anything is opened or signed for a request that
    /// would raise the voucher from `signed` to `cum`, on a channel with
    /// `billed` accepted and `pending` still expected: the seller's unbilled
    /// credit, this request's worst-case cost, and whether one top-up could
    /// pay its worst-case bill.
    fn buyer_check_admission(
        &self,
        terms: &RequestTerms,
        (signed, cum): (u64, u64),
        (billed, pending): (u64, u64),
        deposit: u64,
    ) -> Result<()> {
        let policy = self.ledger.policy()?;
        let budget = policy
            .budget()
            .context("the payment policy allows free requests only")?;
        // A later upfront may refill credit, but never beyond one admission deposit.
        ensure!(
            cum.saturating_sub(billed) <= self.ledger.admission_deposit()?,
            "the seller's unbilled credit would exceed the admission deposit"
        );
        let before = billed.saturating_add(pending);
        let after = before.saturating_add(terms.worst_case());
        // This request's share of what the voucher may still rise to.
        let increment = cum.max(after) - cum.max(before);
        self.buyer_check_spend(
            terms.upfront.saturating_add(increment),
            (cum - signed).saturating_add(increment),
            budget,
        )
        .map_err(|error| anyhow::anyhow!("worst-case cost: {error}"))?;
        // Each bill tops up at most once when it arrives.
        ensure!(
            cum.max(billed.saturating_add(terms.worst_case()))
                <= deposit.saturating_add(self.config.channel_topup),
            "one channel top-up cannot cover the worst-case bill"
        );
        Ok(())
    }
    /// Ensures the channel, then persists and sends the voucher that covers
    /// the seller's view of the channel and this request's upfront.
    async fn buyer_fund(&self, row: Row, terms: RequestTerms) -> Result<()> {
        if row.cum_after.is_some() {
            return self.buyer_resend_voucher(&row).await;
        }
        if !self.buys_from(&row.peer)? {
            let reason = "paid inference is no longer authorized for this provider";
            return self.buyer_refuse(&row, reason).await;
        }
        if let Err(error) = self.buyer_check_view(&row, &terms) {
            return self.buyer_refuse(&row, &error.to_string()).await;
        }
        // The voucher covers the seller's view plus the upfront; usually
        // that is the buyer's existing voucher.
        let target = terms
            .cum_before
            .checked_add(terms.upfront)
            .context("upfront overflow")?;
        // Every refusal comes before an open, so a refused request costs no channel.
        let current = self.ledger.channel_active(&row.peer, false)?;
        let (signed, deposit, billed, pending) = match &current {
            Some(c) => (
                c.cum(),
                c.deposit,
                c.running_total,
                self.buyer_pending(&row.id, c)?,
            ),
            None => (0, self.config.channel_topup, 0, 0),
        };
        let spend = (signed, signed.max(target));
        let checked = self.buyer_check_admission(&terms, spend, (billed, pending), deposit);
        if let Err(error) = checked {
            return self.buyer_refuse(&row, &error.to_string()).await;
        }
        let Some(channel) = self.buyer_channel(&row, &terms).await? else {
            return Ok(());
        };
        let cum = channel.cum().max(target);
        let Some(channel) = self.buyer_capacity(&row, channel, cum).await? else {
            return Ok(());
        };
        let voucher = self.buyer_voucher(&channel, cum).await?;
        let key = channel.key.clone();
        self.ledger
            .mpp_update_both(&row.id, &key, self.daily_budget()?, |r, c| {
                ensure!(
                    r.state == State::Negotiating
                        && r.cum_after.is_none()
                        && c.state == ChannelState::Open
                        && c.cum() <= cum,
                    "request or channel changed concurrently"
                );
                c.voucher = Some(voucher.clone());
                c.active_at = now();
                r.channel = Some(key.clone());
                r.cum_after = Some(cum);
                Ok(())
            })?;
        let funding = channel.funding.context("channel is not open")?;
        self.mpp_send(&row.peer, &row.id, Wire::Voucher { funding, voucher })
            .await
    }
    pub(super) async fn buyer_resend_voucher(&self, row: &Row) -> Result<()> {
        let Some(key) = &row.channel else {
            return Ok(());
        };
        let Some(channel) = self.ledger.channel_get(key)? else {
            return Ok(());
        };
        if let (ChannelState::Open, Some(funding), Some(voucher)) =
            (channel.state, channel.funding, channel.voucher)
        {
            self.mpp_send(&row.peer, &row.id, Wire::Voucher { funding, voucher })
                .await?;
        }
        Ok(())
    }
    /// The voucher a bill needs, or the reason the buyer refuses to sign it.
    fn buyer_check_bill(
        &self,
        row: &Row,
        terms: &RequestTerms,
        channel: &Channel,
        bill: &Bill,
    ) -> Result<u64> {
        match (bill.usage, row.usage) {
            (None, _) => ensure!(bill.cost == 0, "a bill without usage must waive the cost"),
            (Some(billed), Some(own)) => {
                ensure!(
                    billed == own,
                    "billed usage differs from the buyer's own observation"
                );
                ensure!(
                    bill.cost == terms.bill(billed),
                    "cost differs from the quoted rates"
                );
            }
            (Some(_), None) => bail!("the buyer's own usage is unknown"),
        }
        ensure!(
            channel.state == ChannelState::Open,
            "the channel is closing"
        );
        ensure!(
            bill.seq == channel.bills,
            "the bill reuses an earlier number"
        );
        ensure!(
            channel.running_total.checked_add(bill.cost) == Some(bill.running_total),
            "running total differs from earlier bills plus this cost"
        );
        ensure!(
            bill.cost <= terms.worst_case(),
            "cost exceeds the worst case checked before dispatch"
        );
        let cum = channel.cum().max(bill.running_total);
        let increment = cum - channel.cum();
        // Work already dispatched is paid even when concurrent bills overshoot
        // the daily budget, as in the Lightning plugin; each request's worst
        // case fit the budget at admission, and a `free_only` policy stops
        // only new admissions.
        self.buyer_check_spend(terms.upfront.saturating_add(increment), increment, u64::MAX)?;
        Ok(cum)
    }
    /// Signs a checked bill once the buyer has its own usage, which may come
    /// before or after the bill.
    async fn buyer_settle(&self, row: Row) -> Result<()> {
        let Some(bill) = row.bill.clone() else {
            return Ok(());
        };
        if row.state == State::Running && row.usage.is_none() {
            return Ok(());
        }
        let terms = row.terms.clone().context("missing agreed rates")?;
        let key = row.channel.clone().context("missing channel")?;
        let channel = self.ledger.channel_get(&key)?.context("unknown channel")?;
        // Bills are accepted in their channel's order; a later one waits on
        // its request.
        if bill.seq > channel.bills {
            return Ok(());
        }
        let cum = match self.buyer_check_bill(&row, &terms, &channel, &bill) {
            Ok(cum) => cum,
            Err(error) => {
                return self
                    .buyer_refuse(&row, &format!("bill refused: {error}"))
                    .await;
            }
        };
        // A voucher beyond the deposit waits for one top-up; later bills on
        // the channel wait with it.
        let Some(channel) = self.buyer_capacity(&row, channel, cum).await? else {
            return Ok(());
        };
        let voucher = self.buyer_voucher(&channel, cum).await?;
        let raised = voucher.cum > channel.cum();
        self.ledger
            .mpp_update_both(&row.id, &key, u64::MAX, |r, c| {
                ensure!(
                    r.state == row.state
                        && c.state == ChannelState::Open
                        && c.running_total == channel.running_total
                        && c.bills == bill.seq,
                    "request or channel changed concurrently"
                );
                c.voucher = Some(voucher.clone());
                c.running_total = bill.running_total;
                c.bills += 1;
                c.active_at = now();
                r.cum_after = Some(cum);
                r.state = State::Closed;
                Ok(())
            })?;
        if raised {
            let funding = channel.funding.context("channel is not open")?;
            self.mpp_send(&row.peer, &row.id, Wire::Voucher { funding, voucher })
                .await?;
        }
        Ok(())
    }
    /// Settles the bills held for an earlier one, in their channel's order.
    async fn buyer_settle_held(&self, key: &str) -> Result<()> {
        loop {
            let Some(channel) = self.ledger.channel_get(key)? else {
                return Ok(());
            };
            let next = self.ledger.mpp_active()?.into_iter().find(|r| {
                !r.seller
                    && r.channel.as_deref() == Some(key)
                    && r.bill.as_ref().is_some_and(|b| b.seq == channel.bills)
            });
            let Some(next) = next else {
                return Ok(());
            };
            self.buyer_settle(next).await?;
            let settled = self.ledger.channel_get(key)?;
            if settled.is_none_or(|c| c.bills == channel.bills) {
                return Ok(());
            }
        }
    }
    pub(super) async fn mpp_terminal(&self, event: &Value) -> Result<()> {
        if let Some(id) = self.record_terminal(event)? {
            self.mpp_retry_one(&id).await?;
        }
        Ok(())
    }
    /// Durably records terminal usage: the seller's bill or the buyer's own
    /// observation. Returns the request whose payment can now progress.
    pub(super) fn mpp_record_terminal(&self, event: &Value) -> Result<Option<String>> {
        let Ok(observation) = serving::correlation(event) else {
            return Ok(None);
        };
        let Some(row) = self.ledger.mpp_observed(observation)? else {
            return Ok(None);
        };
        let usage = Usage::from_event(event);
        if !row.seller {
            let changed = self.ledger.mpp_update(&row.id, |r| {
                if r.state != State::Running || r.usage.is_some() {
                    return Ok(false);
                }
                match usage {
                    Some(usage) => r.usage = Some(usage),
                    None => {
                        r.state = State::UsageUnknown;
                        r.error = Some(
                            "the buyer's own usage is unavailable; its bill cannot be verified"
                                .into(),
                        );
                    }
                }
                Ok(true)
            })?;
            return Ok(changed.then_some(row.id));
        }
        let Some(key) = row.channel.clone() else {
            return Ok(None);
        };
        let progress = self
            .ledger
            .mpp_update_both(&row.id, &key, 0, |r, c| seller::bill(r, c, usage))?;
        Ok(progress.then_some(row.id))
    }
    pub async fn retry(&self) -> Result<()> {
        self.wallet.reconcile().await?;
        let mut error = self.resync_requirements().await.err();
        if let Err(e) = self.sync_bond().await {
            error.get_or_insert(e);
        }
        for row in self.ledger.mpp_active()? {
            if let Some(key) = &row.channel
                && self
                    .ledger
                    .channel_get(key)?
                    .is_some_and(|c| c.state == ChannelState::Closed)
            {
                self.mpp_close(&row.id, "the payment channel closed")?;
                continue;
            }
            if let Err(e) = self.mpp_retry_one(&row.id).await {
                self.ledger.mpp_update(&row.id, |r| {
                    r.error = Some(e.to_string());
                    Ok(())
                })?;
                error.get_or_insert(e);
            }
        }
        let channels = self.ledger.channels_active()?;
        let open: Vec<_> = channels
            .iter()
            .filter(|c| c.seller && c.state == ChannelState::Open)
            .cloned()
            .collect();
        let watched = self.seller_watch_list(&open).await?;
        for channel in channels {
            // An open seller channel needs a chain read only when watched or idle.
            let quiet = channel.seller
                && channel.state == ChannelState::Open
                && watched.as_ref().is_some_and(|w| !w.contains(&channel.key))
                && !Self::seller_idle_due(&channel);
            if quiet {
                continue;
            }
            if let Err(e) = self.channel_retry_one(&channel.key).await {
                self.ledger.channel_update(&channel.key, |c| {
                    c.error = Some(e.to_string());
                    Ok(())
                })?;
                error.get_or_insert(e);
            }
        }
        error.map_or(Ok(()), Err)
    }
    pub(crate) async fn mpp_retry_one(&self, id: &str) -> Result<()> {
        let row = self.ledger.mpp_get(id)?.context("unknown MPP request")?;
        let serial = self.peer_lock(&row.peer)?;
        let _guard = serial.lock().await;
        self.mpp_progress(&row.id).await
    }
    async fn mpp_progress(&self, id: &str) -> Result<()> {
        let row = self.ledger.mpp_get(id)?.context("unknown MPP request")?;
        let expired = row.terms.as_ref().is_some_and(|t| t.expires_at <= now());
        match (row.seller, &row.state) {
            (_, State::Closed) => return Ok(()),
            (_, State::Negotiating | State::Ready) if expired => self.mpp_close(
                id,
                "quote expired before dispatch; any voucher stays as channel credit",
            )?,
            // No HTTP request can dispatch it any more, for example after a
            // restart or a host deadline, so the seller is told to drop it.
            (false, State::Negotiating | State::Ready) if row.observation.is_none() => {
                let reason =
                    "the HTTP request ended before dispatch; any voucher stays as channel credit";
                self.buyer_refuse(&row, reason).await?
            }
            (false, State::Negotiating) => match row.terms.clone() {
                None => {
                    let intent = row.intent.clone();
                    self.mpp_send(&row.peer, id, Wire::Hello { intent }).await?
                }
                Some(terms) => self.buyer_fund(row, terms).await?,
            },
            (false, State::Running | State::UsageUnknown)
                if row.bill.is_none()
                    && row.terms.as_ref().is_some_and(|t| {
                        now() >= t.expires_at.saturating_add(channel::UNBILLED_SECONDS)
                    }) =>
            {
                // No bill was acknowledged, so the channel view stays as it is.
                self.mpp_close(
                    id,
                    "no bill arrived; closed without paying for this request",
                )?
            }
            (false, State::Running | State::UsageUnknown) => {
                let key = row.channel.clone();
                self.buyer_settle(row).await?;
                if let Some(key) = key {
                    self.buyer_settle_held(&key).await?;
                }
            }
            (true, State::Ready) if self.wallet_info.can_send => {
                self.mpp_send(&row.peer, id, Wire::Ready).await?
            }
            (true, State::Billed) => self.seller_collect(row).await?,
            (true, State::UsageUnknown) if !row.unbilled_seen => self.seller_unbilled(&row).await?,
            _ => {}
        }
        self.changed.notify_waiters();
        Ok(())
    }
    pub async fn mpp_control(&self, id: &str, command: &str) -> Result<()> {
        let row = self.ledger.mpp_get(id)?.context("unknown MPP request")?;
        let stored_id = row.id.clone();
        let id = stored_id.as_str();
        let serial = self.peer_lock(&row.peer)?;
        let _guard = serial.lock().await;
        let row = self.ledger.mpp_get(id)?.context("unknown MPP request")?;
        match command {
            "waive" => {
                ensure!(
                    row.seller && row.state == State::UsageUnknown,
                    "waive requires seller's unknown usage"
                );
                // After a later quote the request is never billed, so the
                // waiver only records the operator's decision.
                if row.superseded {
                    return self.mpp_close(
                        id,
                        "operator waived unknown usage; a later request was already quoted, so no bill is sent",
                    );
                }
                let key = row.channel.context("request has no channel")?;
                self.ledger.mpp_update_both(id, &key, 0, |r, c| {
                    ensure!(r.bill.is_none(), "request already billed");
                    if c.state != ChannelState::Open {
                        r.state = State::Closed;
                        r.error = Some(
                            "operator waived unknown usage; the channel is closing, so no bill is sent"
                                .into(),
                        );
                        return Ok(());
                    }
                    r.bill = Some(Bill {
                        seq: c.bills,
                        usage: None,
                        cost: 0,
                        running_total: c.running_total,
                    });
                    c.bills += 1;
                    r.state = State::Billed;
                    r.error = Some(
                        "operator waived unknown usage with a zero bill, not a zero usage measurement"
                            .into(),
                    );
                    Ok(())
                })?;
            }
            "cancel" => {
                ensure!(!row.seller, "buyer cancels requests");
                ensure!(
                    matches!(row.state, State::Negotiating | State::Ready),
                    "request already dispatched or closed"
                );
                self.mpp_close(id, "cancelled; any voucher stays as channel credit")?;
                self.mpp_send(&row.peer, id, Wire::Cancel).await?;
            }
            _ => bail!("unknown MPP control"),
        }
        self.mpp_progress(id).await
    }
}
/// The request's terms both peers agree on; the digest of the body as the
/// client sent it binds the forwarded request to the agreement.
pub(super) fn request_intent(event: &Value, payer: &str) -> Result<Intent> {
    ensure!(
        event["endpoint"] == "chat_completions",
        "paid requests support chat_completions only"
    );
    let body = event
        .get("body")
        .context("bounded request body permission is required")?;
    ensure!(
        serde_json::to_vec(body)?.len() <= 64 * 1024,
        "request body exceeds 64 KiB"
    );
    let model = event["model"].as_str().context("missing selected model")?;
    ensure!(
        body["model"] == model,
        "paid requests require an exact model, without automatic selection or aliases"
    );
    let output_limit = body
        .get("max_completion_tokens")
        .or_else(|| body.get("max_tokens"))
        .and_then(Value::as_u64)
        .filter(|limit| (1..=4096).contains(limit))
        .context("paid requests require max_tokens or max_completion_tokens of 1..4096")?;
    let mut canonical = body.clone();
    canonical.sort_all_objects();
    let canonical = serde_json::to_vec(&canonical)?;
    Ok(Intent {
        model: model.into(),
        body_digest: hex::encode(Sha256::digest(&canonical)),
        payer: payer.into(),
        output_limit,
        input_bytes: canonical.len() as u64,
    })
}
#[cfg(test)]
#[path = "engine_mpp_tests.rs"]
mod tests;
