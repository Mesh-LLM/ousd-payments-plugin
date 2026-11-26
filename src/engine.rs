use crate::contract::WalletInfo;
use crate::{
    config::{Config, Price, now, validate_peer},
    ledger::Ledger,
    protocol::{CHANNEL, MPP_VERSION, Message},
    wallet::Backend,
};
use anyhow::{Context, Result, bail, ensure};
use mesh_llm_plugin::{
    PluginContext,
    openai_exchange::{OpenAiAdmissionDecision, OpenAiExchangeDecision},
};
use serde_json::Value;
use std::{collections::BTreeSet, sync::Arc};
#[path = "engine_admission.rs"]
mod admission;
#[path = "engine_bonds.rs"]
mod bonds;
#[path = "engine_channel.rs"]
mod channel;
#[path = "engine_mpp.rs"]
mod mpp;
#[path = "engine_seller.rs"]
mod seller;
#[path = "engine_serving.rs"]
mod serving;

pub struct Engine {
    pub config: Config,
    pub bonds: crate::bonds::Service,
    pub ledger: Ledger,
    pub wallet: Arc<dyn Backend>,
    pub wallet_info: WalletInfo,
    pub node: String,
    context: Option<PluginContext<'static>>,
    changed: tokio::sync::Notify,
    peers: std::sync::Mutex<std::collections::BTreeMap<String, Arc<tokio::sync::Mutex<()>>>>,
    ingress: std::sync::Mutex<std::collections::BTreeMap<String, serving::RemoteIngress>>,
    /// Serializes settings changes with their host calls, so host calls follow
    /// ledger order; holds models whose channel requirement the host has not
    /// accepted yet.
    unsynced: tokio::sync::Mutex<std::collections::BTreeSet<String>>,
    watch: std::sync::Mutex<channel::Watch>,
    /// Buyers that paid the seller since it started, as in the Lightning
    /// plugin; their new channels open without an upfront.
    standing: std::sync::Mutex<BTreeSet<String>>,
    /// The host has not accepted the current bond reference yet.
    bond_unsynced: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    outbox: std::sync::Mutex<Vec<(String, Message)>>,
    /// Makes test sends fail as if the peer were unreachable.
    #[cfg(test)]
    unreachable: std::sync::atomic::AtomicBool,
    /// Channel requirements sent to the test host: model and whether required.
    #[cfg(test)]
    requirements: std::sync::Mutex<Vec<(String, bool)>>,
    /// Bond references published to the test host.
    #[cfg(test)]
    published: std::sync::Mutex<Vec<Option<crate::bonds::Proof>>>,
}
impl Engine {
    pub fn new(
        config: Config,
        ledger: Ledger,
        wallet: Arc<dyn Backend>,
        wallet_info: WalletInfo,
        node: String,
        context: PluginContext<'static>,
    ) -> Result<Arc<Self>> {
        validate_peer(&node)?;
        if !wallet_info.is_absent() {
            wallet_info.validate_tempo()?;
        }
        ensure!(
            wallet_info.asset.network()? == config.wallet.network,
            "wallet does not match configured Tempo network"
        );
        // Payment state is bound to the node; a node without a wallet has none,
        // so a client with a changing identity can still verify bonds.
        if !wallet_info.is_absent() {
            ledger.pin_node(&node)?;
        }
        ledger.seed(&config)?;
        ledger.mpp_restart()?;
        if wallet_info.is_absent() {
            let paid = ledger.pricing()?.values().any(|p| !p.is_free());
            ensure!(!paid && ledger.policy()?.budget().is_none(), NEEDS_WALLET);
        }
        Ok(Arc::new(Self {
            bonds: crate::bonds::Service::new(&config)?,
            config,
            ledger,
            wallet,
            wallet_info,
            node,
            context: Some(context),
            changed: Default::default(),
            peers: Default::default(),
            ingress: Default::default(),
            unsynced: Default::default(),
            watch: Default::default(),
            standing: Default::default(),
            bond_unsynced: Default::default(),
            #[cfg(test)]
            outbox: Default::default(),
            #[cfg(test)]
            unreachable: Default::default(),
            #[cfg(test)]
            requirements: Default::default(),
            #[cfg(test)]
            published: Default::default(),
        }))
    }
    pub async fn send(&self, peer: &str, message: &Message) -> Result<()> {
        if let Some(context) = &self.context {
            return context
                .clone()
                .send_json_channel(CHANNEL, peer, "payment", message)
                .await;
        }
        #[cfg(test)]
        {
            ensure!(
                !self.unreachable.load(std::sync::atomic::Ordering::SeqCst),
                "peer unreachable"
            );
            self.outbox
                .lock()
                .unwrap()
                .push((peer.into(), message.clone()));
            Ok(())
        }
        #[cfg(not(test))]
        bail!("peer transport is not initialized")
    }
    pub async fn message(&self, peer: &str, message: Message) -> Result<()> {
        validate_peer(peer)?;
        let serial = self.peer_lock(peer)?;
        let _guard = serial.lock().await;
        match message {
            Message::Mpp {
                version,
                id,
                message,
            } => {
                ensure!(version == MPP_VERSION, "unsupported MPP protocol version");
                self.mpp_message(peer, &id, *message).await
            }
            Message::Closed {
                version,
                channel_id,
                closure,
            } => {
                ensure!(version == MPP_VERSION, "unsupported MPP protocol version");
                self.buyer_closed_notice(peer, &channel_id, closure).await
            }
            Message::Error { id, reason } => {
                self.ledger.mpp_update(&id, |row| {
                    ensure!(row.peer == peer, "unexpected error peer");
                    row.error = Some(reason.chars().take(512).collect());
                    Ok(())
                })?;
                self.changed.notify_waiters();
                Ok(())
            }
        }
    }
    pub async fn hook(&self, mut event: Value) -> Result<OpenAiExchangeDecision> {
        if let Err(error) = self.remember_ingress(&event) {
            return Ok(deny(&error.to_string()));
        }
        self.restore_ingress(&mut event);
        if event["phase"] == "backend_selected"
            && event["endpoint"] == "chat_completions"
            && (self.ledger.policy()?.budget().is_some()
                || event["requested_by_node_id"].is_string())
            && (!event["body"].is_object() || serde_json::to_vec(&event["body"])?.len() > 64 * 1024)
        {
            return Ok(deny(
                "bounded request body is required to enforce payment identity on every route",
            ));
        }
        match event["phase"].as_str() {
            Some("exchange_finished") => {
                self.mpp_terminal(&event).await?;
                Ok(Default::default())
            }
            Some("backend_selected") if self.is_remote_paid_selection(&event)? => {
                self.mpp_seller_hook(&event).await
            }
            Some("backend_selected") => self.mpp_buyer_hook(&event).await,
            _ => Ok(Default::default()),
        }
    }
    pub fn record_terminal(&self, event: &Value) -> Result<Option<String>> {
        self.forget_ingress(event);
        self.mpp_record_terminal(event)
    }
    /// Asks the host to route every model the ledger prices only to peers
    /// on the payment channel.
    pub async fn require_paid_models(&self) -> Result<()> {
        for (model, price) in self.ledger.pricing()? {
            if !price.is_free() {
                self.require_model(&model).await?;
            }
        }
        Ok(())
    }
    /// Requires the payment channel for `model` while the ledger prices it,
    /// and withdraws the requirement otherwise.
    async fn require_model(&self, model: &str) -> Result<()> {
        let paid = self.ledger.paid_price(model)?.is_some();
        if let Some(context) = &self.context {
            let required: &[(&str, &[u8])] = if paid { &[(CHANNEL, &[])] } else { &[] };
            return context
                .clone()
                .require_channels_for_model(model, required)
                .await;
        }
        #[cfg(test)]
        {
            ensure!(
                !self.unreachable.load(std::sync::atomic::Ordering::SeqCst),
                "host unreachable"
            );
            self.requirements.lock().unwrap().push((model.into(), paid));
            Ok(())
        }
        #[cfg(not(test))]
        bail!("host context unavailable")
    }
    /// Sends `model`'s requirement, keeping it for the next retry tick when
    /// the host does not accept it.
    async fn sync_model(&self, unsynced: &mut BTreeSet<String>, model: &str) -> Result<()> {
        let result = self.require_model(model).await;
        if result.is_ok() {
            unsynced.remove(model);
        } else {
            unsynced.insert(model.into());
        }
        result
    }
    /// Re-sends the channel requirements the host has not accepted yet.
    pub(crate) async fn resync_requirements(&self) -> Result<()> {
        let mut unsynced = self.unsynced.lock().await;
        for model in unsynced.clone() {
            self.sync_model(&mut unsynced, &model).await?;
        }
        Ok(())
    }
    /// Sets, or with `None` removes, a model's seller price together with
    /// its channel requirement; removal also drops its admission deposit.
    pub async fn set_pricing(&self, model: &str, price: Option<&Price>) -> Result<()> {
        ensure!(
            price.is_none_or(Price::is_free) || !self.wallet_info.is_absent(),
            NEEDS_WALLET
        );
        let mut unsynced = self.unsynced.lock().await;
        self.ledger.set_pricing(model, price)?;
        self.sync_model(&mut unsynced, model).await
    }
    /// Changes only the cached-input discount of an already priced model.
    pub async fn set_cache_discount(&self, model: &str, percent: u8) -> Result<()> {
        let mut unsynced = self.unsynced.lock().await;
        self.ledger.set_cache_discount(model, percent)?;
        self.sync_model(&mut unsynced, model).await
    }
    /// Whether the policy allows paid buying now and `peer` is an allowed
    /// seller: any seller unless `buy_peers` lists some.
    fn buys_from(&self, peer: &str) -> Result<bool> {
        let peers = &self.config.buy_peers;
        Ok(self.ledger.policy()?.budget().is_some() && (peers.is_empty() || peers.contains(peer)))
    }
    fn peer_lock(&self, peer: &str) -> Result<Arc<tokio::sync::Mutex<()>>> {
        let mut peers = self.peers.lock().unwrap();
        if !peers.contains_key(peer) && peers.len() >= 4096 {
            // A held or queued lock has another Arc and must remain shared.
            peers.retain(|_, lock| Arc::strong_count(lock) > 1);
        }
        ensure!(
            peers.contains_key(peer) || peers.len() < 4096,
            "peer coordination capacity exhausted"
        );
        Ok(peers.entry(peer.into()).or_default().clone())
    }
}
/// Why a node without a wallet refuses paid settings.
pub(crate) const NEEDS_WALLET: &str =
    "prices and automatic buying need a wallet; run `tempo wallet init`, fund it and restart";

fn allow() -> OpenAiExchangeDecision {
    OpenAiExchangeDecision {
        decision: OpenAiAdmissionDecision::Allow,
        ..Default::default()
    }
}
fn deny(reason: &str) -> OpenAiExchangeDecision {
    OpenAiExchangeDecision {
        decision: OpenAiAdmissionDecision::Deny,
        status: Some(402),
        reason: Some(reason.into()),
        ..Default::default()
    }
}
