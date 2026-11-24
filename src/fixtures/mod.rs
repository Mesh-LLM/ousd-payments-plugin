//! Explicit in-process fake wallet for the separately built offline entry point.
mod settlement;

use crate::{
    config::Config,
    contract::{Asset, Balance, Network, WalletInfo},
    wallet::{Backend, OpenedWallet, WalletFuture},
};
use anyhow::{Context, Result, ensure};
use rusqlite::Connection;
use serde_json::Value;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

struct Fixture {
    connection: Mutex<Connection>,
    address: String,
}

pub(crate) fn load_wallet(
    config: &Config,
    _db: crate::ledger::Database,
) -> WalletFuture<'_, OpenedWallet> {
    Box::pin(async move {
        ensure!(
            config.wallet.network == Network::Testnet,
            "offline fixture entry point requires testnet configuration"
        );
        let path = PathBuf::from(
            std::env::var("MESH_LLM_OFFLINE_CHAIN_PATH")
                .context("explicit offline chain path required")?,
        );
        ensure!(path.is_absolute(), "offline chain path must be absolute");
        let address = std::env::var("MESH_LLM_OFFLINE_ADDRESS")
            .context("explicit offline address required")?;
        crate::contract::validate_address(&address)?;
        let connection = Connection::open(path)?;
        connection.busy_timeout(std::time::Duration::from_secs(10))?;
        connection.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE IF NOT EXISTS fixture_channels(id TEXT PRIMARY KEY,funding TEXT NOT NULL,deposit INTEGER NOT NULL,topups INTEGER NOT NULL,closure TEXT);")?;
        let info = WalletInfo {
            wallet_id: format!("OFFLINE-{address}"),
            address: address.clone(),
            asset: Asset::default(),
            can_send: true,
        };
        Ok((
            Arc::new(Fixture {
                connection: Mutex::new(connection),
                address,
            }) as Arc<dyn Backend>,
            info,
        ))
    })
}
impl Backend for Fixture {
    fn execute(
        &self,
        request: crate::contract::mpp::Request,
    ) -> WalletFuture<'_, crate::contract::mpp::Response> {
        Box::pin(async move { self.execute_request(request) })
    }
    fn balance(&self) -> WalletFuture<'_, Balance> {
        Box::pin(async {
            Ok(Balance {
                balance: 1_000_000_000,
                reserved: 0,
                spendable: 1_000_000_000,
                block_number: 10,
            })
        })
    }
    fn attempts(&self) -> Result<Vec<Value>> {
        let db = self.connection.lock().unwrap();
        let mut query =
            db.prepare("SELECT funding,deposit,closure FROM fixture_channels ORDER BY rowid")?;
        query.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u64>(1)?, r.get::<_, Option<String>>(2)?)))?
            .map(|row| { let (funding, deposit, closure)=row?; Ok(serde_json::json!({"funding":serde_json::from_str::<Value>(&funding)?, "deposit":deposit.to_string(), "closure":closure.map(|s| serde_json::from_str::<Value>(&s)).transpose()?})) })
            .collect()
    }
}
