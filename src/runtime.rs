use crate::{
    config::Config,
    engine::Engine,
    protocol::{CHANNEL, Message},
    wallet::{Backend, OpenedWallet, WalletFuture},
};
use anyhow::{Context, Result, ensure};
use mesh_llm_plugin::peer_evidence::{PeerEvidenceVerificationRequest, peer_evidence_verifier};
use mesh_llm_plugin::{
    OperationRouter, PluginMetadata, PluginRuntime, SimplePlugin, operation_with_schema,
    plugin_manifest, plugin_server_info, structured_tool_result,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{Arc, OnceLock};
use tokio::sync::mpsc::{Sender, error::TrySendError};

pub const NAME: &str = "tempo";
const VERIFIER_HANDLER: &str = "verify_peer_evidence";

type Slot = Arc<OnceLock<Arc<Engine>>>;
type MeshId = Arc<std::sync::Mutex<Option<String>>>;

/// The offline fixture's manifest. Bonds need a chain, so with
/// `TEMPO_FIXTURE_NO_VERIFIER` set the fixture is no verifier, and the
/// offline payment smoke reaches sellers without a bond.
pub fn fixture_manifest() -> mesh_llm_plugin::proto::PluginManifest {
    let mut manifest = manifest();
    if std::env::var_os("TEMPO_FIXTURE_NO_VERIFIER").is_some() {
        manifest.peer_evidence_verifier = None;
    }
    manifest
}

pub fn manifest() -> mesh_llm_plugin::proto::PluginManifest {
    use mesh_llm_plugin::{config_path, config_schema, config_setting, mesh_channel, operation};
    let mut hook = mesh_llm_plugin::openai_exchange::openai_exchange_hook("payment_gate");
    hook.metadata = false;
    hook.required = true;
    hook.admission = true;
    hook.deadline_ms = 30_000;
    hook.request_body = true;
    hook.effective_request_body = true;
    hook.read_identity_bundle = true;
    hook.max_body_bytes = 64 * 1024;
    plugin_manifest()
        .item(
            config_schema(NAME)
                .setting(config_setting("config_path", config_path()).required(false)),
        )
        .item(hook)
        .item(peer_evidence_verifier(
            crate::bonds::NAMESPACE,
            VERIFIER_HANDLER,
        ))
        .item(mesh_channel(CHANNEL))
        .item(mesh_llm_plugin::mesh_event_mesh_id_updated())
        .item(operation::<Value>(
            "control",
            "Manage Tempo payments, bonds and the shared wallet",
        ))
        .item(operation::<PeerEvidenceVerificationRequest>(
            VERIFIER_HANDLER,
            "Verify a peer bond at finalized Tempo state",
        ))
        .build()
}

/// The operator's `[plugin.settings]`; a package install runs without any.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Settings {
    /// The config file; by default `~/.mesh-llm/tempo/config.json`, created
    /// when absent.
    config_path: Option<std::path::PathBuf>,
}

fn get(slot: &Slot) -> Result<Arc<Engine>> {
    slot.get().cloned().context("Tempo engine is not ready")
}

pub async fn run() -> Result<()> {
    run_with_wallet(manifest(), |config, db| {
        Box::pin(async move {
            // Without a wallet the node verifies bonds and serves free
            // inference until `fund` or `bond_lock` creates one.
            if !config.data_dir.join("wallet.json").exists() {
                ensure!(!config.needs_wallet(), crate::engine::NEEDS_WALLET);
                let info = crate::contract::WalletInfo::absent(config.wallet.network);
                let unopened =
                    crate::wallet::Unopened::new(config.wallet.clone(), &config.data_dir, db);
                return Ok((Arc::new(unopened) as Arc<dyn Backend>, info));
            }
            let wallet = crate::tempo::Wallet::load(config.wallet.clone(), &config.data_dir, db)?;
            let info = wallet.info().await?;
            Ok((wallet as Arc<dyn Backend>, info))
        })
    })
    .await
}

/// Run the explicit offline entry point, never selected by production settings.
#[cfg(feature = "offline-fixture")]
pub async fn run_fixture() -> Result<()> {
    run_with_wallet(fixture_manifest(), crate::fixtures::load_wallet).await
}

async fn run_with_wallet(
    manifest: mesh_llm_plugin::proto::PluginManifest,
    load_wallet: fn(&Config, crate::ledger::Database) -> WalletFuture<'_, OpenedWallet>,
) -> Result<()> {
    // Mesh accepts evidence only from its verifier.
    let publishes = manifest.peer_evidence_verifier.is_some();
    let slot: Slot = Default::default();
    let prepared = Arc::new(std::sync::Mutex::new(None::<Config>));
    let metadata = PluginMetadata::new(
        NAME,
        env!("CARGO_PKG_VERSION"),
        plugin_server_info(
            NAME,
            env!("CARGO_PKG_VERSION"),
            "Tempo payments and bonds",
            "OUSD payments and refundable identity bonds",
            None::<String>,
        ),
    )
    .with_manifest(manifest);
    let mesh_id: MeshId = Default::default();
    let mesh_events = mesh_id.clone();
    let initialize = prepared.clone();
    let startup = slot.clone();
    let channels = slot.clone();
    let hooks = slot.clone();
    let queues = PeerQueues::new();
    let bodies = crate::request_stream::RequestStreams::default();
    let plugin = SimplePlugin::new(metadata)
        .with_operation_router(controls(slot, mesh_id))
        .on_mesh_event(move |event, _| {
            let mesh_id = mesh_events.clone();
            Box::pin(async move {
                if event.kind == mesh_llm_plugin::proto::mesh_event::Kind::MeshIdUpdated as i32 {
                    *mesh_id.lock().unwrap() = (!event.mesh_id.is_empty()).then_some(event.mesh_id);
                }
                Ok(())
            })
        })
        .on_open_stream(move |request, _| {
            let bodies = bodies.clone();
            Box::pin(async move { bodies.open(request).await })
        })
        .on_initialize(move |request, _| {
            let prepared = initialize.clone();
            Box::pin(async move {
                let settings: Settings = match request.settings_json.trim() {
                    "" => Settings::default(),
                    json => serde_json::from_str(json).map_err(anyhow::Error::from)?,
                };
                let config = Config::load_from(settings.config_path)?;
                let required = mesh_llm_plugin::peer_evidence::HOST_CAPABILITY;
                if !request
                    .host_capabilities
                    .iter()
                    .any(|capability| capability == required)
                {
                    return Err(anyhow::anyhow!(
                        "Tempo requires a Mesh host supporting {required}"
                    )
                    .into());
                }
                *prepared.lock().unwrap() = Some(config);
                Ok(())
            })
        })
        .on_initialized(move |context| {
            let slot = startup.clone();
            let prepared = prepared.clone();
            let context = context.owned();
            Box::pin(async move {
                let config = prepared
                    .lock()
                    .unwrap()
                    .take()
                    .context("Tempo settings missing")?;
                let identity: Value = context
                    .request_host("ReadIdentityBundle", json!({}))
                    .await?;
                let node = identity["node_endpoint_id"]
                    .as_str()
                    .context("host identity omitted node endpoint")?
                    .into();
                let ledger = crate::ledger::Ledger::open(&config.data_dir)?;
                let (wallet, info) = load_wallet(&config, ledger.db.clone()).await?;
                let engine = Engine::new(config, ledger, wallet, info, node, context.clone())?;
                if publishes {
                    engine.publish_bond().await?;
                }
                engine.require_paid_models().await?;
                ensure!(
                    slot.set(engine.clone()).is_ok(),
                    "Tempo plugin initialized twice"
                );
                let weak = Arc::downgrade(&engine);
                tokio::spawn(async move {
                    let mut interval = tokio::time::interval(std::time::Duration::from_secs(3));
                    loop {
                        interval.tick().await;
                        let Some(engine) = weak.upgrade() else { break };
                        if let Err(error) = engine.retry().await {
                            eprintln!("tempo: reconciliation failed: {error:#}");
                        }
                    }
                });
                Ok(())
            })
        })
        .on_channel_message(move |message, _context| {
            let slot = channels.clone();
            let queues = queues.clone();
            Box::pin(async move {
                if message.channel != CHANNEL || message.body.len() > 32 * 1024 {
                    return Ok(());
                }
                let Ok(body) = serde_json::from_slice::<Message>(&message.body) else {
                    tracing::warn!("dropping malformed stablecoin channel message");
                    return Ok(());
                };
                let Ok(engine) = get(&slot) else {
                    return Ok(());
                };
                let peer = message.source_peer_id;
                let worker = peer.clone();
                let queued = queues.push(&peer, body, move |body| {
                    deliver(engine.clone(), worker.clone(), body)
                });
                if !queued {
                    tracing::warn!("stablecoin channel capacity exhausted");
                }
                Ok(())
            })
        })
        .with_openai_exchange_handler(move |_, event, _| {
            let slot = hooks.clone();
            Box::pin(async move {
                let engine = get(&slot)?;
                if event["phase"] == "exchange_finished" {
                    if let Some(id) = engine.record_terminal(&event)? {
                        tokio::spawn(async move {
                            if let Err(error) = engine.mpp_retry_one(&id).await {
                                eprintln!("tempo: terminal settlement failed: {error:#}");
                            }
                        });
                    }
                    return Ok(Default::default());
                }
                let decision = engine.hook(event).await.inspect_err(|error| {
                    eprintln!("tempo: payment hook failed: {error:#}");
                })?;
                Ok(decision)
            })
        });
    PluginRuntime::run(plugin).await
}

async fn deliver(engine: Arc<Engine>, peer: String, body: Message) {
    // Errors answer request steps only; they are keyed by request ID.
    let id = match &body {
        Message::Mpp { id, .. } => Some(id.clone()),
        Message::Closed { .. } | Message::Error { .. } => None,
    };
    if let Err(error) = engine.message(&peer, body).await {
        eprintln!("tempo: peer operation failed: {error:#}");
        let reason = error.to_string().chars().take(512).collect();
        if let Some(id) = id {
            let _ = engine.send(&peer, &Message::Error { id, reason }).await;
        }
    }
}

/// Handles each peer's messages one at a time, in the order this plugin
/// receives them, with a bounded queue per peer.
struct PeerQueues<T>(Arc<std::sync::Mutex<std::collections::BTreeMap<String, Sender<T>>>>);
impl<T> Clone for PeerQueues<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<T: Send + 'static> PeerQueues<T> {
    fn new() -> Self {
        Self(Default::default())
    }
    /// Queues `item` behind the peer's earlier items, starting a worker that
    /// runs `handle` when the peer has none; `false` when no room is left.
    fn push<F, Fut>(&self, peer: &str, item: T, handle: F) -> bool
    where
        F: Fn(T) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let mut queues = self.0.lock().unwrap();
        let item = match queues.get(peer) {
            Some(queue) => match queue.try_send(item) {
                Ok(()) => return true,
                Err(TrySendError::Full(_)) => return false,
                Err(TrySendError::Closed(item)) => item,
            },
            None if queues.len() >= 4096 => return false,
            None => item,
        };
        let (queue, mut receiver) = tokio::sync::mpsc::channel(32);
        let _ = queue.try_send(item);
        queues.insert(peer.to_owned(), queue);
        let (this, peer) = (self.clone(), peer.to_owned());
        tokio::spawn(async move {
            loop {
                let idle = std::time::Duration::from_secs(60);
                match tokio::time::timeout(idle, receiver.recv()).await {
                    Ok(Some(item)) => handle(item).await,
                    // Pushes hold the same lock, so nothing is queued after this check.
                    _ => {
                        let mut queues = this.0.lock().unwrap();
                        if receiver.is_empty() {
                            queues.remove(&peer);
                            return;
                        }
                    }
                }
            }
        });
        true
    }
}

fn controls(slot: Slot, mesh_id: MeshId) -> OperationRouter {
    let mut router = OperationRouter::new();
    let verification = slot.clone();
    router.add_raw(
        operation_with_schema(
            "control",
            "Manage payments, pricing, wallet sends, bond deposits and refunds",
            crate::control::schema(),
        ),
        move |request, _| {
            let slot = slot.clone();
            let mesh_id = mesh_id.clone();
            Box::pin(async move {
                let engine = get(&slot)?;
                let command: crate::control::Control = request.arguments()?;
                let mesh_id = mesh_id.lock().unwrap().clone();
                structured_tool_result(command.run(&engine, mesh_id).await?)
            })
        },
    );
    router.add_raw(
        mesh_llm_plugin::json_schema_operation::<PeerEvidenceVerificationRequest>(
            VERIFIER_HANDLER,
            "Verify a provider bond",
        ),
        move |request, _| {
            let slot = verification.clone();
            Box::pin(async move {
                let input: PeerEvidenceVerificationRequest = request.arguments()?;
                let engine = get(&slot)?;
                let result = engine
                    .bonds
                    .verify(
                        &engine.config.bonds,
                        &input.mesh_id,
                        &input.node_id,
                        input.evidence,
                    )
                    .await;
                structured_tool_result(
                    result.unwrap_or_else(
                        |error| json!({"accepted":false,"reason":error.to_string(),
                            "valid_until":crate::config::now().saturating_add(engine.config.bonds.verification_retry_seconds)}),
                    ),
                )
            })
        },
    );
    router
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn each_peers_messages_run_one_at_a_time_in_arrival_order() {
        let queues = PeerQueues::new();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        for n in 0..64u64 {
            let peer = format!("peer-{}", n % 2);
            let seen = seen.clone();
            assert!(queues.push(&peer, n, move |n| {
                let seen = seen.clone();
                async move {
                    // Later messages would overtake earlier ones if they ran concurrently.
                    tokio::time::sleep(std::time::Duration::from_millis(64 - n)).await;
                    seen.lock().unwrap().push(n);
                }
            }));
        }
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while seen.lock().unwrap().len() < 64 {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let seen = seen.lock().unwrap();
        for parity in 0..2 {
            let order: Vec<_> = seen.iter().filter(|n| *n % 2 == parity).collect();
            assert!(order.windows(2).all(|w| w[0] < w[1]), "{order:?}");
        }
    }
    #[test]
    fn host_sees_admission_controls_and_the_evidence_verifier() {
        let m = manifest();
        assert_eq!(m.mesh_channels[0].name, CHANNEL);
        let hook = m.openai_exchange_hook.unwrap();
        assert!(hook.required && hook.admission && hook.read_identity_bundle);
        assert!(!hook.delegate_signing_key && !hook.response_body);
        assert_eq!(
            m.operations
                .iter()
                .map(|op| op.name.as_str())
                .collect::<Vec<_>>(),
            vec!["control", "verify_peer_evidence"]
        );
        assert!(m.capabilities.is_empty());
        assert!(
            m.grant_requests
                .is_none_or(|g| g.operation_bindings.is_empty())
        );
    }

    #[test]
    fn documented_grant_fully_authorizes_the_admission_hook() {
        let hook = manifest().openai_exchange_hook.unwrap();
        let grant = serde_json::from_value(json!({
            "endpoints":["chat_completions","completions","responses"],
            "phases":["request_received","backend_selected","exchange_finished"],
            "request_body":true,"effective_request_body":true,
            "admission":true,"read_identity_bundle":true,
            "deadline_ms":30000,"max_body_bytes":65536,
            "max_queue_bytes":2097152,"max_in_flight":32,
            "failure_policy":"required"
        }))
        .unwrap();
        assert!(
            mesh_llm_plugin::openai_exchange::negotiate_openai_exchange(Some(&hook), Some(&grant))
                .unwrap()
                .is_some()
        );
    }
}
