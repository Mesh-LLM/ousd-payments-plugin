//! Runtime-changeable settings in the payment ledger: the buyer policy,
//! seller prices and admission deposits, as in the Lightning plugin.
use super::Ledger;
use crate::config::{Config, Policy, Price};
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, params};
use std::collections::BTreeMap;

impl Ledger {
    /// Writes the config's policy, seller prices and, when set, admission
    /// deposit into the ledger at startup, as the Lightning plugin does.
    /// Settings for models absent from the config stay.
    pub fn seed(&self, config: &Config) -> Result<()> {
        self.set_policy(&config.policy)?;
        for (model, price) in &config.prices {
            self.set_pricing(model, Some(price))?;
        }
        if let Some(deposit) = config.admission_deposit {
            self.set_admission_deposit(deposit)?;
        }
        Ok(())
    }
    pub fn policy(&self) -> Result<Policy> {
        self.setting("policy")?
            .map_or_else(|| Ok(Policy::default()), |v| Ok(serde_json::from_str(&v)?))
    }
    pub fn set_policy(&self, policy: &Policy) -> Result<()> {
        policy.validate()?;
        self.set_setting("policy", &serde_json::to_string(policy)?)
    }
    /// The policy with today's voucher increments and what remains of the
    /// daily budget.
    pub fn policy_status(&self) -> Result<serde_json::Value> {
        let policy = self.policy()?;
        let spent = self.spent_today()?;
        let remaining = policy.budget().unwrap_or(0).saturating_sub(spent);
        let mut status = serde_json::to_value(&policy)?;
        status["spent_today"] = spent.to_string().into();
        status["remaining_daily_budget"] = remaining.to_string().into();
        Ok(status)
    }
    /// Seller rates per model, including free ones.
    pub fn pricing(&self) -> Result<BTreeMap<String, Price>> {
        self.prefixed("price/")?
            .into_iter()
            .map(|(model, value)| Ok((model, serde_json::from_str(&value)?)))
            .collect()
    }
    /// Per-model admission deposits, as set.
    pub fn model_admission_deposits(&self) -> Result<BTreeMap<String, u64>> {
        self.prefixed("deposit/")?
            .into_iter()
            .map(|(model, value)| Ok((model, value.parse()?)))
            .collect()
    }
    /// Settings whose key starts with `prefix`, by the rest of the key.
    fn prefixed(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        let db = self.db.lock().unwrap();
        let mut query = db.prepare(
            "SELECT substr(key,length(?1)+1),value FROM settings WHERE substr(key,1,length(?1))=?1 ORDER BY key",
        )?;
        Ok(query
            .query_map([prefix], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?)
    }
    /// The seller's rates for `model`, unless it is unpriced or free.
    pub fn paid_price(&self, model: &str) -> Result<Option<Price>> {
        let Some(value) = self.setting(&format!("price/{model}"))? else {
            return Ok(None);
        };
        let price: Price = serde_json::from_str(&value)?;
        Ok((!price.is_free()).then_some(price))
    }
    /// Sets, or with `None` removes, one model's rates; its admission deposit
    /// goes with its price in the same transaction.
    pub fn set_pricing(&self, model: &str, price: Option<&Price>) -> Result<()> {
        ensure!(
            !model.is_empty() && model.len() <= 1024,
            "model must be 1..1024 bytes"
        );
        let Some(price) = price else {
            let mut db = self.db.lock().unwrap();
            let tx = db.transaction()?;
            for key in [format!("price/{model}"), format!("deposit/{model}")] {
                tx.execute("DELETE FROM settings WHERE key=?1", [key])?;
            }
            tx.commit()?;
            return Ok(());
        };
        price.validate()?;
        self.set_setting(&format!("price/{model}"), &serde_json::to_string(price)?)
    }
    /// Changes only the cached-input discount of a priced model, reading and
    /// writing its price in one transaction.
    pub fn set_cache_discount(&self, model: &str, percent: u8) -> Result<()> {
        let key = format!("price/{model}");
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let value: String = tx
            .query_row("SELECT value FROM settings WHERE key=?1", [&key], |r| {
                r.get(0)
            })
            .optional()?
            .context("set the model's input and output rates before a cache discount")?;
        let mut price: Price = serde_json::from_str(&value)?;
        price.cache_discount_percent = percent;
        price.validate()?;
        tx.execute(
            "UPDATE settings SET value=?2 WHERE key=?1",
            params![key, serde_json::to_string(&price)?],
        )?;
        tx.commit()?;
        Ok(())
    }
    /// The seller's upfront for a new channel on a model without its own
    /// deposit, and the largest upfront this node accepts as a buyer.
    pub fn admission_deposit(&self) -> Result<u64> {
        Ok(self
            .setting("admission_deposit")?
            .map(|v| v.parse())
            .transpose()?
            .unwrap_or(0))
    }
    pub fn set_admission_deposit(&self, deposit: u64) -> Result<()> {
        self.set_setting("admission_deposit", &deposit.to_string())
    }
    /// Forgets the set node-wide deposit, so it is 0 until set again.
    pub fn reset_admission_deposit(&self) -> Result<()> {
        self.db
            .lock()
            .unwrap()
            .execute("DELETE FROM settings WHERE key='admission_deposit'", [])?;
        Ok(())
    }
    /// The seller's upfront for a new channel on `model`: its own deposit,
    /// else the node-wide one.
    pub fn model_admission_deposit(&self, model: &str) -> Result<u64> {
        match self.setting(&format!("deposit/{model}"))? {
            Some(value) => Ok(value.parse()?),
            None => self.admission_deposit(),
        }
    }
    /// Sets, or with `None` clears, the admission deposit of a priced model.
    pub fn set_model_admission_deposit(&self, model: &str, deposit: Option<u64>) -> Result<()> {
        let key = format!("deposit/{model}");
        let db = self.db.lock().unwrap();
        let Some(deposit) = deposit else {
            db.execute("DELETE FROM settings WHERE key=?1", [key])?;
            return Ok(());
        };
        // The price check and the write are one statement.
        let written = db.execute(
            "INSERT INTO settings(key,value) SELECT ?1,?2 WHERE EXISTS(SELECT 1 FROM settings WHERE key=?3) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, deposit.to_string(), format!("price/{model}")],
        )?;
        ensure!(
            written == 1,
            "set the model's price before its admission deposit"
        );
        Ok(())
    }
    /// The last block whose close events the seller scanned completely.
    pub fn close_cursor(&self) -> Result<Option<u64>> {
        Ok(self
            .setting("close_cursor")?
            .map(|b| b.parse())
            .transpose()?)
    }
    pub fn set_close_cursor(&self, block: u64) -> Result<()> {
        self.set_setting("close_cursor", &block.to_string())
    }
    /// The bond this node publishes as its peer evidence.
    pub fn published_bond(&self) -> Result<Option<String>> {
        self.setting("published_bond")
    }
    pub fn set_published_bond(&self, bond_id: Option<&str>) -> Result<()> {
        match bond_id {
            Some(id) => self.set_setting("published_bond", id),
            None => self.clear_setting("published_bond"),
        }
    }
    /// The operation ID of the newest finalized bond lock already published.
    pub fn published_lock(&self) -> Result<Option<String>> {
        self.setting("published_lock")
    }
    pub fn set_published_lock(&self, operation_id: &str) -> Result<()> {
        self.set_setting("published_lock", operation_id)
    }
    fn clear_setting(&self, key: &str) -> Result<()> {
        self.db
            .lock()
            .unwrap()
            .execute("DELETE FROM settings WHERE key=?1", [key])?;
        Ok(())
    }
    fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .db
            .lock()
            .unwrap()
            .query_row("SELECT value FROM settings WHERE key=?1", [key], |r| {
                r.get(0)
            })
            .optional()?)
    }
    fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.db.lock().unwrap().execute(
            "INSERT INTO settings VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_are_seeded_from_config_survive_restart_and_deposits_follow_prices() {
        let temp = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(temp.path()).unwrap();
        assert_eq!(ledger.policy().unwrap(), Policy::default());
        assert!(ledger.pricing().unwrap().is_empty());
        assert_eq!(ledger.admission_deposit().unwrap(), 0);
        let mut config: Config = serde_json::from_value(serde_json::json!({
            "data_dir": temp.path(),
            "policy": {"mode":"automatic","daily_budget":"50"},
            "prices": {"paid":{"input_per_million":"1","output_per_million":"2"},
                "free":{"input_per_million":"0","output_per_million":"0"}},
            "admission_deposit": "7"
        }))
        .unwrap();
        ledger.seed(&config).unwrap();
        assert!(
            ledger
                .set_model_admission_deposit("unpriced", Some(3))
                .is_err()
        );
        ledger.set_model_admission_deposit("paid", Some(3)).unwrap();
        drop(ledger);
        let ledger = Ledger::open(temp.path()).unwrap();
        assert_eq!(ledger.policy().unwrap().budget(), Some(50));
        assert_eq!(ledger.pricing().unwrap().len(), 2);
        assert!(ledger.paid_price("free").unwrap().is_none());
        assert_eq!(
            ledger
                .paid_price("paid")
                .unwrap()
                .unwrap()
                .output_per_million,
            2
        );
        assert_eq!(ledger.model_admission_deposit("paid").unwrap(), 3);
        assert_eq!(ledger.model_admission_deposit("free").unwrap(), 7);
        // Startup writes what the config sets and leaves the rest.
        config.admission_deposit = None;
        config.policy = Policy::default();
        ledger.seed(&config).unwrap();
        assert_eq!(ledger.policy().unwrap(), Policy::default());
        assert_eq!(ledger.admission_deposit().unwrap(), 7);
        assert_eq!(ledger.model_admission_deposit("paid").unwrap(), 3);
        // A model's deposit goes with its price.
        ledger.set_pricing("paid", None).unwrap();
        assert!(ledger.paid_price("paid").unwrap().is_none());
        ledger.seed(&config).unwrap();
        assert_eq!(ledger.model_admission_deposit("paid").unwrap(), 7);
        let invalid = Policy {
            mode: crate::config::Mode::Automatic,
            daily_budget: None,
        };
        assert!(ledger.set_policy(&invalid).is_err());
    }
}
