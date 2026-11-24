use crate::contract::{decimal, decimal_option};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Price {
    #[serde(with = "decimal")]
    pub input_per_million: u64,
    #[serde(with = "decimal")]
    pub output_per_million: u64,
    /// Percentage taken off reported cached input only.
    #[serde(default)]
    pub cache_discount_percent: u8,
}
impl Price {
    pub fn is_free(&self) -> bool {
        self.input_per_million == 0 && self.output_per_million == 0
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.cache_discount_percent <= 100,
            "cache discount must be 0..=100 percent"
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    FreeOnly,
    Automatic,
}

/// The buyer's approval policy, as in the Lightning plugin.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    #[serde(default)]
    pub mode: Mode,
    /// The voucher increments the buyer may sign per day.
    #[serde(default, with = "decimal_option")]
    pub daily_budget: Option<u64>,
}
impl Policy {
    pub fn validate(&self) -> Result<()> {
        if self.mode == Mode::Automatic {
            ensure!(
                self.daily_budget
                    .is_some_and(|n| n > 0 && n <= i64::MAX as u64),
                "automatic payments require a positive daily budget"
            );
        }
        Ok(())
    }
    /// The daily budget when paid buying is allowed.
    pub fn budget(&self) -> Option<u64> {
        (self.mode == Mode::Automatic)
            .then_some(self.daily_budget)
            .flatten()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub data_dir: PathBuf,
    #[serde(default)]
    pub wallet: crate::tempo::Settings,
    #[serde(default)]
    pub bonds: crate::bonds::Settings,
    /// Seller rates, written into the ledger at startup.
    #[serde(default)]
    pub prices: BTreeMap<String, Price>,
    /// Written into the ledger at startup.
    #[serde(default)]
    pub policy: Policy,
    /// Upfront plus the bill's voucher increment for one request; 0 means none.
    #[serde(default, with = "decimal")]
    pub cap: u64,
    /// The sellers paid inference is bought from; empty means any seller,
    /// as in the Lightning plugin.
    #[serde(default)]
    pub buy_peers: BTreeSet<String>,
    /// Per-model rate ceilings; a model without one is bought at the
    /// seller's rates, within the per-request cap and daily budget.
    #[serde(default)]
    pub buy_prices: BTreeMap<String, Price>,
    /// The seller's node-wide upfront for a new channel and the largest
    /// upfront the buyer accepts, written into the ledger at startup when set.
    #[serde(default, with = "decimal_option")]
    pub admission_deposit: Option<u64>,
    /// The deposit of a new channel and the amount of each top-up; 1 OUSD
    /// unless set.
    #[serde(default = "default_channel_topup", with = "decimal")]
    pub channel_topup: u64,
}
fn default_channel_topup() -> u64 {
    1_000_000
}
impl Config {
    pub fn load(path: &std::path::Path) -> Result<Self> {
        ensure!(path.is_absolute(), "config_path must be absolute");
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).context("read Tempo config")?)?;
        let unstated = value["bonds"].get("providers").is_none();
        let mut config: Self = serde_json::from_value(value)?;
        if config.bonds.contract.is_none() {
            config.bonds.contract = config.wallet.network.bond_escrow().map(Into::into);
        }
        // Providers need the network's standard bond unless the config
        // states another requirement.
        if unstated {
            config.bonds.providers = crate::bonds::Requirement::standard(config.wallet.network);
        }
        config.validate()?;
        Ok(config)
    }
    /// Loads `path`, or without one (or an empty one) the default config,
    /// which is created on first use with its state beside it.
    pub fn load_from(path: Option<PathBuf>) -> Result<Self> {
        match path.filter(|p| !p.as_os_str().is_empty()) {
            Some(path) => Self::load(&path),
            None => Self::load_or_create(&default_dir()?),
        }
    }
    /// Loads `dir/config.json`, first writing one that keeps the state in
    /// `dir` and leaves every other setting at its default.
    pub fn load_or_create(dir: &std::path::Path) -> Result<Self> {
        let path = dir.join("config.json");
        if !path.exists() {
            crate::tempo::keys::secure_dir(dir)?;
            let config = serde_json::json!({ "data_dir": dir });
            let mut options = std::fs::OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&path)?;
            std::io::Write::write_all(&mut file, &serde_json::to_vec_pretty(&config)?)?;
            file.sync_all()?;
        }
        Self::load(&path)
    }
    /// Whether the settings charge or pay for inference, which needs a wallet.
    pub fn needs_wallet(&self) -> bool {
        self.policy.mode == Mode::Automatic || self.prices.values().any(|p| !p.is_free())
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(self.data_dir.is_absolute(), "data_dir must be absolute");
        if let Some(password) = &self.wallet.password_file {
            ensure!(
                password.is_absolute(),
                "wallet.password_file must be absolute"
            );
        }
        self.bonds.validate()?;
        self.policy.validate()?;
        for price in self.prices.values().chain(self.buy_prices.values()) {
            price.validate()?;
        }
        for peer in &self.buy_peers {
            validate_peer(peer)?;
        }
        self.check_buying(&self.policy)
    }
    /// Whether this config can pay under `policy`, from the config or the
    /// `policy` control.
    pub fn check_buying(&self, policy: &Policy) -> Result<()> {
        if policy.mode == Mode::Automatic {
            ensure!(
                self.channel_topup > 0,
                "spending requires a nonzero channel top-up"
            );
            ensure!(
                self.channel_topup <= self.wallet.max_payment,
                "channel_topup exceeds wallet.max_payment"
            );
            // One top-up must fit any bill within the per-request limit, or
            // the buyer could not pay it and the seller would close.
            let limit = match self.cap {
                0 => policy.daily_budget.unwrap_or(0),
                cap => cap,
            };
            ensure!(
                self.channel_topup >= limit,
                "channel_topup must be at least cap, or the daily budget without a cap"
            );
        }
        Ok(())
    }
}
/// `~/.mesh-llm/tempo`, beside the Lightning plugin's `~/.mesh-llm/payments`.
pub fn default_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|home| !home.is_empty()))
        .context("cannot determine the home directory for Tempo's state")?;
    Ok(PathBuf::from(home).join(".mesh-llm").join("tempo"))
}

pub fn validate_peer(peer: &str) -> Result<()> {
    ensure!(
        peer.len() == 64 && hex::decode(peer)?.len() == 32,
        "invalid Mesh peer ID"
    );
    Ok(())
}
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn documented_examples_load_and_enable_buying_with_one_change() {
        for example in [
            include_str!("../demo/buyer-payments.json.example"),
            include_str!("../demo/seller-payments.json.example"),
            include_str!("../demo/mainnet-payments.json.example"),
        ] {
            let filled = example.replace(
                "REPLACE_WITH_SELLER_64_CHARACTER_HEX_PEER_ID",
                &"bb".repeat(32),
            );
            let mut config: Config = serde_json::from_str(&filled).unwrap();
            config.validate().unwrap();
            assert_eq!(config.policy.mode, Mode::FreeOnly);
            // Switching the mode is the one change, given a daily budget.
            config.policy.mode = Mode::Automatic;
            assert_eq!(
                config.validate().is_ok(),
                config.policy.daily_budget.is_some()
            );
        }
    }
    #[test]
    fn default_config_never_authorizes_spending_and_rejects_unknown_settings() {
        let mut value = serde_json::json!({"data_dir":"/absolute/payments", "wallet":{}});
        let config: Config = serde_json::from_value(value.clone()).unwrap();
        config.validate().unwrap();
        assert!(config.prices.is_empty());
        assert_eq!(config.bonds.max_exit_delay_seconds, 86_400);
        assert_eq!(config.policy, Policy::default());
        assert_eq!(config.policy.budget(), None);
        assert!(config.buy_peers.is_empty());
        assert_eq!((config.cap, config.admission_deposit), (0, None));
        assert_eq!(config.wallet.network, crate::contract::Network::Testnet);
        assert_eq!(config.wallet.rpc_url(), "https://rpc.moderato.tempo.xyz");
        for removed in [
            serde_json::json!({"payments":{"enabled":true}}),
            serde_json::json!({"bonds":{"enabled":true}}),
            serde_json::json!({"wallet":{"data_dir":"/absolute/wallet"}}),
            serde_json::json!({"buying":{"enabled":true}}),
        ] {
            let mut obsolete = value.clone();
            obsolete
                .as_object_mut()
                .unwrap()
                .extend(removed.as_object().unwrap().clone());
            assert!(serde_json::from_value::<Config>(obsolete).is_err());
        }
        value["unsupported_setting"] = serde_json::json!("0");
        assert!(serde_json::from_value::<Config>(value).is_err());
    }

    #[test]
    fn automatic_buying_needs_a_positive_budget_and_a_top_up() {
        let peer = "bb".repeat(32);
        let mut value = serde_json::json!({"data_dir":"/absolute/payments",
            "policy":{"mode":"automatic","daily_budget":"100"},
            "buy_peers":[peer],"channel_topup":"20","cap":"10","admission_deposit":"5",
            "buy_prices":{"model":{"input_per_million":"1","output_per_million":"2"}}});
        let config: Config = serde_json::from_value(value.clone()).unwrap();
        config.validate().unwrap();
        assert_eq!(config.policy.budget(), Some(100));
        assert_eq!((config.cap, config.admission_deposit), (10, Some(5)));
        for (field, invalid) in [
            ("policy", serde_json::json!({"mode":"automatic"})),
            (
                "policy",
                serde_json::json!({"mode":"automatic","daily_budget":"0"}),
            ),
            ("channel_topup", serde_json::json!("0")),
            // One top-up must cover the cap.
            ("channel_topup", serde_json::json!("9")),
            // Without a cap, it must cover the daily budget.
            ("cap", serde_json::json!("0")),
            // The wallet must be allowed to sign each deposit.
            ("wallet", serde_json::json!({"max_payment": "19"})),
        ] {
            let mut broken = value.clone();
            broken[field] = invalid;
            let config: Config = serde_json::from_value(broken).unwrap();
            assert!(config.validate().is_err(), "{field}");
        }
        let mut uncapped = value.clone();
        uncapped["cap"] = serde_json::json!("0");
        uncapped["channel_topup"] = serde_json::json!("100");
        let config: Config = serde_json::from_value(uncapped).unwrap();
        config.validate().unwrap();
        value["policy"] = serde_json::json!({"mode":"free_only"});
        let config: Config = serde_json::from_value(value).unwrap();
        config.validate().unwrap();
        assert_eq!(config.policy.budget(), None);
        // As in the Lightning plugin, a policy alone enables buying: from
        // any seller, at its rates, with a 1 OUSD channel top-up.
        let policy = serde_json::json!({"mode":"automatic","daily_budget":"100000"});
        let minimal = serde_json::json!({"data_dir":"/absolute/payments","policy":policy});
        let config: Config = serde_json::from_value(minimal).unwrap();
        config.validate().unwrap();
        assert!(config.buy_peers.is_empty() && config.buy_prices.is_empty());
        assert_eq!((config.cap, config.channel_topup), (0, 1_000_000));
    }

    #[test]
    fn network_selection_sets_rpc_default_without_inferring_network_from_url() {
        let mut value =
            serde_json::json!({"data_dir":"/absolute/payments", "wallet":{"network":"mainnet"}});
        let config: Config = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(config.wallet.network, crate::contract::Network::Mainnet);
        assert_eq!(config.wallet.rpc_url(), "https://rpc.tempo.xyz");
        assert_eq!(config.policy.mode, Mode::FreeOnly);
        value["wallet"]["rpc_url"] = serde_json::json!("https://custom.example/rpc");
        let config: Config = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(config.wallet.rpc_url(), "https://custom.example/rpc");
        assert_eq!(config.wallet.network, crate::contract::Network::Mainnet);
        value["wallet"]["network"] = serde_json::json!("unknown");
        assert!(serde_json::from_value::<Config>(value.clone()).is_err());
        value["wallet"].as_object_mut().unwrap().remove("network");
        let config: Config = serde_json::from_value(value).unwrap();
        assert_eq!(config.wallet.network, crate::contract::Network::Testnet);
    }

    #[test]
    fn testnet_uses_the_deployed_escrow_and_providers_always_need_a_bond() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("payments.json");
        let load = |value: serde_json::Value| {
            std::fs::write(&path, value.to_string()).unwrap();
            Config::load(&path)
        };
        let required = serde_json::json!({"providers":{"minimum_amount":"1"}});
        let testnet = load(serde_json::json!({"data_dir":"/absolute/payments","bonds":required}));
        assert_eq!(
            testnet.unwrap().bonds.contract.as_deref(),
            Some("0xd210B0356b3e45B37F29Ec763106dc23BF1cC3aa")
        );
        // Without a stated requirement, testnet buyers require 1 OUSD for a minute.
        let standard = load(serde_json::json!({"data_dir":"/absolute/payments"})).unwrap();
        let requirement = standard.bonds.providers;
        let minimums = (
            requirement.minimum_amount,
            requirement.minimum_exit_delay_seconds,
        );
        assert_eq!(minimums, (1_000_000, 60));
        let none = serde_json::json!({"data_dir":"/absolute/payments","bonds":{"providers":null}});
        assert!(load(none).is_err());
        // Mainnet has no deployed escrow, so it loads but no bond qualifies
        // until one is configured.
        let unknown =
            serde_json::json!({"data_dir":"/absolute/payments","wallet":{"network":"mainnet"}});
        let unknown = load(unknown).unwrap().bonds;
        assert_eq!(unknown.contract, None);
        assert_eq!(unknown.providers.minimum_exit_delay_seconds, 86_400);
        let mut configured = serde_json::json!({"data_dir":"/absolute/payments",
            "wallet":{"network":"mainnet"},"bonds":required});
        configured["bonds"]["contract"] = serde_json::json!(format!("0x{}", "12".repeat(20)));
        assert!(!load(configured).unwrap().needs_wallet());
    }

    #[test]
    fn the_default_config_is_written_once_with_its_state_beside_it() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("tempo");
        let created = Config::load_or_create(&dir).unwrap();
        assert_eq!(created.data_dir, dir);
        assert!(!created.needs_wallet());
        assert_eq!(created.bonds.providers.minimum_amount, 1_000_000);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode();
            assert_eq!(mode(&dir) & 0o777, 0o700);
            assert_eq!(mode(&dir.join("config.json")) & 0o777, 0o600);
        }
        // An edited file is kept.
        let edited =
            serde_json::json!({"data_dir": dir, "bonds": {"providers": {"minimum_amount": "5"}}});
        std::fs::write(dir.join("config.json"), edited.to_string()).unwrap();
        let kept = Config::load_or_create(&dir).unwrap().bonds.providers;
        assert_eq!(kept.minimum_amount, 5);
        assert!(Config::load_from(Some(PathBuf::from("relative.json"))).is_err());
    }
}
