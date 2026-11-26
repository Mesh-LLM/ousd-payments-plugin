use crate::contract::{
    Balance, WalletInfo,
    mpp::{CloseEvents, Request, Response},
};
use anyhow::Result;
use std::{future::Future, pin::Pin};

pub(crate) type OpenedWallet = (std::sync::Arc<dyn Backend>, WalletInfo);

pub(crate) type WalletFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

pub(crate) trait Backend: Send + Sync {
    fn execute(&self, request: Request) -> WalletFuture<'_, Response>;
    fn balance(&self) -> WalletFuture<'_, Balance>;
    fn attempts(&self) -> Result<Vec<serde_json::Value>>;
    fn bond<'a>(
        &'a self,
        _settings: &'a crate::bonds::Settings,
        _id: &'a str,
        _intent: crate::bonds::Intent,
    ) -> WalletFuture<'a, serde_json::Value> {
        Box::pin(async { anyhow::bail!("bond transactions unavailable on this backend") })
    }
    /// A journaled OUSD transfer of `amount` to `to`, once per operation ID.
    fn send<'a>(
        &'a self,
        _operation_id: &'a str,
        _to: &'a str,
        _amount: u64,
        _max_fee: u64,
    ) -> WalletFuture<'a, serde_json::Value> {
        Box::pin(async { anyhow::bail!("wallet sends unavailable on this backend") })
    }
    fn reconcile(&self) -> WalletFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
    /// The funding address and network, after creating the wallet when it
    /// has none; `None` when the engine's wallet info already answers.
    fn fund(&self) -> WalletFuture<'_, Option<serde_json::Value>> {
        Box::pin(async { Ok(None) })
    }
    /// The seller's channels named in close events since block `from`; a
    /// backend without event logs reports every scan skipped, so the caller
    /// inspects every channel.
    fn close_events(&self, _from: Option<u64>) -> WalletFuture<'_, CloseEvents> {
        Box::pin(async {
            Ok(CloseEvents {
                skipped: true,
                ..Default::default()
            })
        })
    }
}

const NO_WALLET: &str = "this node has no wallet yet; `fund` creates one";

/// The backend of a node started without a wallet. It verifies bonds and
/// serves free inference, and creates the wallet when the operator first
/// runs `fund` or `bond_lock`; payments need a restart after that.
pub(crate) struct Unopened {
    settings: crate::tempo::Settings,
    dir: std::path::PathBuf,
    db: crate::ledger::Database,
    wallet: tokio::sync::OnceCell<std::sync::Arc<crate::tempo::Wallet>>,
}
impl Unopened {
    pub(crate) fn new(
        settings: crate::tempo::Settings,
        dir: &std::path::Path,
        db: crate::ledger::Database,
    ) -> Self {
        Self {
            settings,
            dir: dir.into(),
            db,
            wallet: Default::default(),
        }
    }
    /// The wallet, created on the first call.
    pub(crate) async fn open(&self) -> Result<&std::sync::Arc<crate::tempo::Wallet>> {
        self.wallet
            .get_or_try_init(|| async {
                crate::tempo::Wallet::create(self.settings.clone(), &self.dir, self.db.clone())
            })
            .await
    }
}
impl Backend for Unopened {
    fn execute(&self, request: Request) -> WalletFuture<'_, Response> {
        Box::pin(async move {
            match self.wallet.get() {
                Some(wallet) => wallet.mpp(request).await,
                None => anyhow::bail!(NO_WALLET),
            }
        })
    }
    fn balance(&self) -> WalletFuture<'_, Balance> {
        Box::pin(async move {
            match self.wallet.get() {
                Some(wallet) => wallet.balance().await,
                None => anyhow::bail!(NO_WALLET),
            }
        })
    }
    fn attempts(&self) -> Result<Vec<serde_json::Value>> {
        self.wallet
            .get()
            .map_or(Ok(Vec::new()), |wallet| wallet.journal.public())
    }
    fn bond<'a>(
        &'a self,
        settings: &'a crate::bonds::Settings,
        id: &'a str,
        intent: crate::bonds::Intent,
    ) -> WalletFuture<'a, serde_json::Value> {
        Box::pin(async move { self.open().await?.bond(settings, id, intent).await })
    }
    fn send<'a>(
        &'a self,
        operation_id: &'a str,
        to: &'a str,
        amount: u64,
        max_fee: u64,
    ) -> WalletFuture<'a, serde_json::Value> {
        Box::pin(async move {
            match self.wallet.get() {
                Some(wallet) => wallet.send(operation_id, to, amount, max_fee).await,
                None => anyhow::bail!(NO_WALLET),
            }
        })
    }
    fn reconcile(&self) -> WalletFuture<'_, ()> {
        Box::pin(async move {
            match self.wallet.get() {
                Some(wallet) => wallet.reconcile().await,
                None => Ok(()),
            }
        })
    }
    fn fund(&self) -> WalletFuture<'_, Option<serde_json::Value>> {
        Box::pin(async move { Ok(Some(self.open().await?.fund().await?)) })
    }
}

impl Backend for crate::tempo::Wallet {
    fn send<'a>(
        &'a self,
        operation_id: &'a str,
        to: &'a str,
        amount: u64,
        max_fee: u64,
    ) -> WalletFuture<'a, serde_json::Value> {
        Box::pin(self.send(operation_id, to, amount, max_fee))
    }
    fn bond<'a>(
        &'a self,
        settings: &'a crate::bonds::Settings,
        id: &'a str,
        intent: crate::bonds::Intent,
    ) -> WalletFuture<'a, serde_json::Value> {
        Box::pin(self.bond(settings, id, intent))
    }
    fn reconcile(&self) -> WalletFuture<'_, ()> {
        Box::pin(self.reconcile())
    }
    fn close_events(&self, from: Option<u64>) -> WalletFuture<'_, CloseEvents> {
        Box::pin(self.close_events(from))
    }
    fn fund(&self) -> WalletFuture<'_, Option<serde_json::Value>> {
        Box::pin(async move { Ok(Some(self.fund().await?)) })
    }

    fn execute(&self, request: Request) -> WalletFuture<'_, Response> {
        Box::pin(self.mpp(request))
    }
    fn balance(&self) -> WalletFuture<'_, Balance> {
        Box::pin(self.balance())
    }
    fn attempts(&self) -> Result<Vec<serde_json::Value>> {
        self.journal.public()
    }
}
