//! The buyer's admission hook: whether a request is paid, the wait for the
//! pair's slot and the seller's answer, and cancelling what is not dispatched.
use super::*;
use crate::{
    ledger::{ChannelState, Request as Row, State},
    protocol::{Intent, Step as Wire},
};
use mpp::request_intent;

/// How long the buyer's admission hook waits for a channel open or top-up
/// to finalize, inside the host's 30-second hook deadline.
const ADMISSION_WAIT_SECONDS: u64 = 25;
/// How often the buyer says hello again while the seller answers busy, for
/// example until its channel is open or a voucher paying a bill arrived.
const BUSY_RETRY_MILLIS: u64 = 250;
/// Why a buyer request closed when the seller serves its model free.
pub(super) const SERVED_FREE: &str =
    "the seller has no paid price for this model; sent as a free request";
/// The request IDs the buyer minted, by host observation, for tests.
#[cfg(test)]
pub(super) static MINTED: std::sync::Mutex<std::collections::BTreeMap<String, String>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

impl Engine {
    /// Pays for a request when buyer policy says so: automatic buying and an
    /// allowed seller. Every other request takes the free path, as does one
    /// whose model the seller does not price.
    pub(super) async fn mpp_buyer_hook(&self, event: &Value) -> Result<OpenAiExchangeDecision> {
        if event["requested_by_node_id"].is_string() {
            return Ok(Default::default());
        }
        let Some(peer) = event["target_peer_id"].as_str() else {
            return Ok(Default::default());
        };
        if !self.buys_from(peer)? {
            return Ok(Default::default());
        }
        Ok(match self.mpp_admit_buyer(peer, event).await {
            Ok(true) => allow(),
            Ok(false) => Default::default(),
            Err(error) => deny(&error.to_string()),
        })
    }
    /// Negotiates the request under a newly minted ID until it can be
    /// dispatched; `false` means the seller serves the model free.
    async fn mpp_admit_buyer(&self, peer: &str, event: &Value) -> Result<bool> {
        let intent = request_intent(event, &self.wallet_info.address)?;
        let observation = serving::correlation(event)?;
        ensure!(self.wallet_info.can_send, "buyer wallet locked");
        // Only an identical request, the channel open and a top-up wait;
        // everything else is quick.
        let deadline =
            tokio::time::Instant::now() + std::time::Duration::from_secs(ADMISSION_WAIT_SECONDS);
        self.buyer_await_slot(peer, &intent.body_digest, deadline)
            .await?;
        let id = uuid::Uuid::new_v4().to_string();
        let mut row = Row::new(id.clone(), peer.into(), false, intent.clone());
        row.observation = Some(observation.into());
        self.ledger.mpp_insert(&row)?;
        #[cfg(test)]
        MINTED
            .lock()
            .unwrap()
            .insert(observation.into(), id.clone());
        let attachment = BuyerAttachment {
            engine: self,
            id: id.clone(),
            observation: observation.into(),
        };
        let result = self
            .buyer_await_dispatch(peer, &id, intent, observation, deadline)
            .await;
        if let Err(error) = &result
            && let Err(cancel) = self.buyer_abandon(peer, &id, &error.to_string()).await
        {
            tracing::warn!("cannot cancel the undispatched MPP request: {cancel}");
        }
        drop(attachment);
        result
    }
    /// Waits while a request to this seller with the same body holds its
    /// slot, whose own usage usually arrives just after the response, or
    /// while the buyer's first request to it opens the channel.
    async fn buyer_await_slot(
        &self,
        peer: &str,
        body_digest: &str,
        deadline: tokio::time::Instant,
    ) -> Result<()> {
        loop {
            let open = self
                .ledger
                .channel_active(peer, false)?
                .is_some_and(|c| c.state == ChannelState::Open);
            let waits = self.ledger.mpp_active()?.iter().any(|r| {
                !r.seller
                    && r.peer == peer
                    && r.holds_peer()
                    && (!open || r.intent.body_digest == body_digest)
            });
            if !waits {
                return Ok(());
            }
            ensure!(
                tokio::time::Instant::now() < deadline,
                "a paid request to this provider with the same body, or one opening its channel, is still in flight"
            );
            tokio::select! {_=self.changed.notified()=>{},_=tokio::time::sleep(std::time::Duration::from_millis(100))=>{}}
        }
    }
    /// Says hello again after a busy answer, at most every `BUSY_RETRY_MILLIS`.
    async fn buyer_retry_busy(
        &self,
        peer: &str,
        row: &Row,
        hello_at: &mut tokio::time::Instant,
    ) -> Result<()> {
        let pace = std::time::Duration::from_millis(BUSY_RETRY_MILLIS);
        if hello_at.elapsed() < pace {
            return Ok(());
        }
        self.ledger.mpp_update(&row.id, |r| {
            r.busy = false;
            Ok(())
        })?;
        *hello_at = tokio::time::Instant::now();
        let intent = row.intent.clone();
        self.mpp_send(peer, &row.id, Wire::Hello { intent }).await
    }
    async fn buyer_await_dispatch(
        &self,
        peer: &str,
        id: &str,
        intent: Intent,
        observation: &str,
        deadline: tokio::time::Instant,
    ) -> Result<bool> {
        self.mpp_send(peer, id, Wire::Hello { intent }).await?;
        let mut hello_at = tokio::time::Instant::now();
        loop {
            let row = self.ledger.mpp_get(id)?.context("request disappeared")?;
            if row.state == State::Closed {
                if row.error.as_deref() == Some(SERVED_FREE) {
                    return Ok(false);
                }
                bail!("{}", row.error.as_deref().unwrap_or("request is closed"));
            }
            if row.state == State::Ready {
                ensure!(
                    row.terms.as_ref().is_some_and(|t| t.expires_at > now()),
                    "quote expired before dispatch; any voucher stays as channel credit"
                );
                self.ledger.mpp_update(id, |r| {
                    ensure!(
                        r.state == State::Ready && r.observation.as_deref() == Some(observation),
                        "request consumed concurrently"
                    );
                    r.state = State::Running;
                    Ok(())
                })?;
                return Ok(true);
            }
            ensure!(
                row.state == State::Negotiating,
                "request is no longer awaiting its channel"
            );
            if row.busy {
                self.buyer_retry_busy(peer, &row, &mut hello_at).await?;
            }
            if tokio::time::Instant::now() >= deadline {
                bail!(
                    "{}; the request was cancelled and may be sent again",
                    if row.busy {
                        "the provider is still busy"
                    } else {
                        "the payment channel is not ready yet"
                    }
                );
            }
            tokio::select! {_=self.changed.notified()=>{},_=tokio::time::sleep(std::time::Duration::from_millis(100))=>{}}
        }
    }
    /// Cancels a request that will not be dispatched, so it does not hold
    /// the seller's slot for its body or delay its channel.
    async fn buyer_abandon(&self, peer: &str, id: &str, reason: &str) -> Result<()> {
        let serial = self.peer_lock(peer)?;
        let _guard = serial.lock().await;
        let row = self.ledger.mpp_get(id)?.context("unknown MPP request")?;
        if matches!(row.state, State::Negotiating | State::Ready) {
            self.buyer_refuse(&row, reason).await?;
        }
        Ok(())
    }
}
/// Detaches an undispatched request from its HTTP request when the hook
/// ends, also when the host drops it; the retry tick then cancels it.
struct BuyerAttachment<'a> {
    engine: &'a Engine,
    id: String,
    observation: String,
}
impl Drop for BuyerAttachment<'_> {
    fn drop(&mut self) {
        let result = self.engine.ledger.mpp_update(&self.id, |r| {
            if matches!(r.state, State::Negotiating | State::Ready)
                && r.observation.as_deref() == Some(&self.observation)
            {
                r.observation = None;
            }
            Ok(())
        });
        if let Err(error) = result {
            tracing::error!("cannot detach MPP request: {error}");
        }
    }
}
