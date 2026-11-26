//! A write-ahead journal of payments, bonds and wallet sends in the payment
//! ledger's database. Signed bytes commit before any broadcast.
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use super::{bonds::BondAttempt, send::SendAttempt, transaction::SignedTransaction};
use crate::contract::{
    Network,
    mpp::{ChannelFunding, ChannelTerms, Closure, Voucher},
};
use crate::ledger::Database;

pub struct Journal {
    db: Database,
}

/// One channel transaction: open, top-up, close, request close or withdraw.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Attempt {
    pub id: String,
    pub terms: ChannelTerms,
    pub kind: String,
    /// The deposit of an open or top-up, or the capture of a close.
    pub amount: u64,
    /// The voucher a close presents.
    pub voucher: Option<Voucher>,
    #[serde(flatten)]
    pub tx: SignedTransaction,
    pub funding: Option<ChannelFunding>,
    /// The verified deposit after a finalized top-up.
    pub deposit: Option<u64>,
    pub closure: Option<Closure>,
}

/// A journaled signed transaction: a payment step or a bond operation.
pub trait Record: Serialize + DeserializeOwned {
    const KIND: &'static str;
    fn id(&self) -> &str;
    fn tx(&self) -> &SignedTransaction;
}
impl Record for Attempt {
    const KIND: &'static str = "payment";
    fn id(&self) -> &str {
        &self.id
    }
    fn tx(&self) -> &SignedTransaction {
        &self.tx
    }
}
impl Record for BondAttempt {
    const KIND: &'static str = "bond";
    fn id(&self) -> &str {
        &self.id
    }
    fn tx(&self) -> &SignedTransaction {
        &self.tx
    }
}
impl Record for SendAttempt {
    const KIND: &'static str = "send";
    fn id(&self) -> &str {
        &self.id
    }
    fn tx(&self) -> &SignedTransaction {
        &self.tx
    }
}

impl Journal {
    /// Adds the journal to the ledger's database, pinned to one wallet and
    /// network.
    pub fn open(db: Database, wallet_id: &str, address: &str, network: Network) -> Result<Self> {
        {
            let mut conn = db.lock().unwrap();
            let tx = conn.transaction()?;
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS attempts(id TEXT PRIMARY KEY, kind TEXT NOT NULL, active INTEGER NOT NULL, body TEXT NOT NULL);",
            )?;
            let pinned: (Option<String>, Option<String>, Option<u64>) =
                tx.query_row("SELECT wallet_id,address,chain_id FROM identity", [], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })?;
            if let (Some(pinned_id), Some(pinned_address), Some(chain_id)) = pinned {
                ensure!(
                    pinned_id == wallet_id && pinned_address.eq_ignore_ascii_case(address),
                    "payment state belongs to another wallet"
                );
                ensure!(
                    chain_id == network.chain_id(),
                    "payment state belongs to another Tempo network; use a separate data_dir"
                );
            } else {
                tx.execute(
                    "UPDATE identity SET wallet_id=?1,address=?2,chain_id=?3",
                    params![wallet_id, address, network.chain_id()],
                )?;
            }
            tx.commit()?;
        }
        Ok(Self { db })
    }
    pub fn get<T: Record>(&self, id: &str) -> Result<Option<T>> {
        let body: Option<String> = self
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT body FROM attempts WHERE id=?1 AND kind=?2",
                [id, T::KIND],
                |r| r.get(0),
            )
            .optional()?;
        Ok(body.map(|body| serde_json::from_str(&body)).transpose()?)
    }
    /// Whether any signed transaction is still unresolved.
    pub fn pending(&self) -> Result<bool> {
        Ok(self.db.lock().unwrap().query_row(
            "SELECT EXISTS(SELECT 1 FROM attempts WHERE active=1)",
            [],
            |r| r.get(0),
        )?)
    }
    pub fn unresolved<T: Record>(&self) -> Result<Vec<T>> {
        self.query(
            "SELECT body FROM attempts WHERE kind=?1 AND active=1",
            T::KIND,
        )
    }
    #[cfg(test)]
    pub fn all<T: Record>(&self) -> Result<Vec<T>> {
        self.query(
            "SELECT body FROM attempts WHERE kind=?1 ORDER BY rowid",
            T::KIND,
        )
    }
    /// Every attempt's public metadata, oldest first, without signed bytes.
    pub fn public(&self) -> Result<Vec<serde_json::Value>> {
        let records: Vec<serde_json::Value> = {
            let db = self.db.lock().unwrap();
            let mut query = db.prepare("SELECT body FROM attempts ORDER BY rowid")?;
            query
                .query_map([], |r| r.get::<_, String>(0))?
                .map(|body| Ok(serde_json::from_str(&body?)?))
                .collect::<Result<_>>()?
        };
        records
            .iter()
            .map(super::transaction::public_attempt)
            .collect()
    }
    /// Funds held by unresolved transactions of any kind.
    pub fn reserved(&self) -> Result<u64> {
        let pending: Vec<SignedTransaction> = {
            let db = self.db.lock().unwrap();
            let mut query = db.prepare("SELECT body FROM attempts WHERE active=1")?;
            query
                .query_map([], |r| r.get::<_, String>(0))?
                .map(|body| Ok(serde_json::from_str(&body?)?))
                .collect::<Result<_>>()?
        };
        pending.iter().try_fold(0u64, |sum, tx| {
            sum.checked_add(tx.reserved).context("reservation overflow")
        })
    }
    pub fn save<T: Record>(&self, record: &T, insert: bool) -> Result<()> {
        let db = self.db.lock().unwrap();
        let active = record.tx().status == "pending";
        let body = serde_json::to_string(record)?;
        if insert {
            ensure!(
                !db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM attempts WHERE active=1)",
                    [],
                    |r| r.get::<_, bool>(0)
                )?,
                "another signed transaction is unresolved"
            );
            db.execute(
                "INSERT INTO attempts VALUES(?1,?2,?3,?4)",
                params![record.id(), T::KIND, active, body],
            )?;
        } else {
            ensure!(
                db.execute(
                    "UPDATE attempts SET body=?3,active=?4 WHERE id=?1 AND kind=?2",
                    params![record.id(), T::KIND, body, active]
                )? == 1,
                "unknown {} attempt",
                T::KIND
            );
        }
        Ok(())
    }
    fn query<T: DeserializeOwned>(&self, sql: &str, kind: &str) -> Result<Vec<T>> {
        let db = self.db.lock().unwrap();
        let mut query = db.prepare(sql)?;
        query
            .query_map([kind], |r| r.get::<_, String>(0))?
            .map(|body| Ok(serde_json::from_str(&body?)?))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::Ledger;

    #[test]
    fn payment_state_stays_pinned_to_its_wallet_and_network() {
        let address = "0x1111111111111111111111111111111111111111";
        for network in [Network::Testnet, Network::Mainnet] {
            let dir = tempfile::tempdir().unwrap();
            let open = |wallet_id: &str, network| {
                let db = Ledger::open(dir.path()).unwrap().db.clone();
                Journal::open(db, wallet_id, address, network)
            };
            open("fixture", network).unwrap();
            let other = if network == Network::Testnet {
                Network::Mainnet
            } else {
                Network::Testnet
            };
            assert!(open("fixture", other).is_err());
            assert!(open("another-wallet", network).is_err());
            assert!(open("fixture", network).is_ok());
        }
    }
}
