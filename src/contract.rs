//! Stablecoin operations are opaque to the Mesh host.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

pub mod mpp;

pub const OUSD: &str = "0x20c0000000000000000000006a37da5c996874be";
pub const SCALE: u64 = 1_000_000;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Network {
    #[default]
    Testnet,
    Mainnet,
}
impl Network {
    pub fn chain_id(self) -> u64 {
        match self {
            Self::Testnet => 42431,
            Self::Mainnet => 4217,
        }
    }
    /// The deployed bond escrow every mesh on the network shares, if any.
    pub fn bond_escrow(self) -> Option<&'static str> {
        match self {
            Self::Testnet => Some("0xd210B0356b3e45B37F29Ec763106dc23BF1cC3aa"),
            Self::Mainnet => None,
        }
    }
    /// The default bond's exit delay.
    pub fn bond_exit_delay(self) -> u64 {
        match self {
            Self::Testnet => crate::bonds::TESTNET_EXIT_DELAY_SECONDS,
            Self::Mainnet => crate::bonds::DEFAULT_EXIT_DELAY_SECONDS,
        }
    }
    pub fn rpc_url(self) -> &'static str {
        match self {
            Self::Testnet => "https://rpc.moderato.tempo.xyz",
            Self::Mainnet => "https://rpc.tempo.xyz",
        }
    }
    pub fn from_chain_id(chain_id: u64) -> Result<Self> {
        match chain_id {
            42431 => Ok(Self::Testnet),
            4217 => Ok(Self::Mainnet),
            _ => anyhow::bail!("unsupported Tempo chain {chain_id}"),
        }
    }
    pub fn asset(self) -> Asset {
        Asset {
            chain_id: self.chain_id(),
            token: OUSD.into(),
            decimals: 6,
        }
    }
}

/// Never serialize native amounts as JSON floating-point numbers.
pub mod decimal {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};
    pub fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(D::Error::custom)
    }
}

/// An optional amount as an exact decimal string, or `null`.
pub mod decimal_option {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};
    pub fn serialize<S: Serializer>(value: &Option<u64>, serializer: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(value) => serializer.serialize_str(&value.to_string()),
            None => serializer.serialize_none(),
        }
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<u64>, D::Error> {
        Option::<String>::deserialize(deserializer)?
            .map(|value| value.parse().map_err(D::Error::custom))
            .transpose()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Asset {
    pub chain_id: u64,
    pub token: String,
    pub decimals: u8,
}
impl Default for Asset {
    fn default() -> Self {
        Network::default().asset()
    }
}
impl Asset {
    /// Validate OUSD identity and return its Tempo network.
    pub fn network(&self) -> Result<Network> {
        let network = Network::from_chain_id(self.chain_id)?;
        ensure!(self.token.eq_ignore_ascii_case(OUSD), "unsupported token");
        ensure!(self.decimals == 6, "unexpected token precision");
        Ok(network)
    }
}

pub fn validate_address(value: &str) -> Result<()> {
    let bytes = hex::decode(value.strip_prefix("0x").unwrap_or(value))?;
    ensure!(
        bytes.len() == 20 && bytes.iter().any(|b| *b != 0),
        "invalid account address"
    );
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WalletInfo {
    pub wallet_id: String,
    pub address: String,
    pub asset: Asset,
    pub can_send: bool,
}
impl WalletInfo {
    /// A node without a wallet: it cannot sign, so it verifies bonds and
    /// serves free inference only.
    pub fn absent(network: Network) -> Self {
        Self {
            wallet_id: String::new(),
            address: format!("0x{}", "00".repeat(20)),
            asset: network.asset(),
            can_send: false,
        }
    }
    pub fn is_absent(&self) -> bool {
        self.wallet_id.is_empty()
    }
    /// Validate the local wallet identity and the supported Tempo OUSD asset.
    pub fn validate_tempo(&self) -> Result<()> {
        self.asset.network()?;
        validate_address(&self.address)?;
        ensure!(
            !self.wallet_id.is_empty()
                && self.wallet_id.len() <= 256
                && !self.wallet_id.chars().any(char::is_control),
            "invalid wallet ID"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Balance {
    #[serde(with = "decimal")]
    pub balance: u64,
    #[serde(with = "decimal")]
    pub reserved: u64,
    #[serde(with = "decimal")]
    pub spendable: u64,
    pub block_number: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_amounts_are_exact_decimal_strings() {
        let value = Balance {
            balance: u64::MAX,
            reserved: 1,
            spendable: 2,
            block_number: 3,
        };
        let encoded = serde_json::to_string(&value).unwrap();
        assert!(encoded.contains("\"18446744073709551615\""));
        assert_eq!(
            serde_json::from_str::<Balance>(&encoded).unwrap().balance,
            u64::MAX
        );
        assert!(serde_json::from_str::<Balance>("{\"balance\":1.5}").is_err());
    }
    #[test]
    fn asset_identity_accepts_both_networks_and_rejects_lookalikes() {
        let mut asset = Asset::default();
        assert_eq!(asset.network().unwrap(), Network::Testnet);
        asset = Network::Mainnet.asset();
        assert_eq!(asset.network().unwrap(), Network::Mainnet);
        asset.chain_id = 1;
        assert!(asset.network().is_err());
        asset = Asset::default();
        asset.token = "0x20c0000000000000000000000000000000000000".into();
        assert!(asset.network().is_err());
        asset = Asset::default();
        asset.decimals = 18;
        assert!(asset.network().is_err());
        assert!(validate_address("0x0000000000000000000000000000000000000000").is_err());
    }
}
