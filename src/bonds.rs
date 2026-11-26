//! Refundable Tempo bonds and finalized-state verification.
use crate::{
    config::now,
    contract::decimal,
    tempo::{
        receipt,
        rpc::{Rpc, address, quantity},
    },
};
use alloy::{
    primitives::{Address, B256, U256, keccak256},
    sol,
    sol_types::{SolCall, SolValue},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;

pub const NAMESPACE: &str = "tempo.bond.v1";
pub const MAX_STATE_AGE: u64 = 120;
pub const MAX_EXIT_DELAY_SECONDS: u64 = u32::MAX as u64;
/// The default bond, 1 OUSD with the network's exit delay: what a seller
/// locks and a buyer with a known escrow requires unless configured otherwise.
pub const DEFAULT_AMOUNT: u64 = 1_000_000;
/// The default bond's exit delay on mainnet, and the longest exit delay a
/// seller authorizes unless configured otherwise.
pub const DEFAULT_EXIT_DELAY_SECONDS: u64 = 86_400;
/// The default bond's exit delay on testnet, short so a trial bond comes
/// back within minutes.
pub const TESTNET_EXIT_DELAY_SECONDS: u64 = 60;

sol! {
    interface Escrow {
        struct Bond {
            bytes32 meshId;
            bytes32 hostKey;
            address refundWallet;
            uint96 amount;
            uint64 exitDelay;
            uint256 withdrawAt;
            bool withdrawn;
        }
        function getBond(bytes32 id) external view returns (Bond memory);
        function token() external view returns (address);
        function lock(bytes32 meshId, bytes32 hostKey, uint96 amount, uint64 exitDelay, bytes32 salt) external returns (bytes32);
        function withdraw(bytes32 id) external;
        function requestExit(bytes32 id) external;
    }
    function approve(address spender, uint256 amount) external returns (bool);
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Requirement {
    #[serde(with = "decimal")]
    pub minimum_amount: u64,
    pub minimum_exit_delay_seconds: u64,
}

impl Default for Requirement {
    fn default() -> Self {
        Self {
            minimum_amount: 1,
            minimum_exit_delay_seconds: 0,
        }
    }
}
impl Requirement {
    /// The requirement a buyer on `network` applies when its config names none.
    pub fn standard(network: crate::contract::Network) -> Self {
        Self {
            minimum_amount: DEFAULT_AMOUNT,
            minimum_exit_delay_seconds: network.bond_exit_delay(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// The escrow; defaults to the network's deployed one.
    pub contract: Option<String>,
    /// The bond a provider needs; defaults to the network's standard bond.
    pub providers: Requirement,
    #[serde(with = "decimal")]
    pub max_lock: u64,
    pub max_exit_delay_seconds: u64,
    pub verification_retry_seconds: u64,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            contract: None,
            providers: Requirement::default(),
            max_lock: DEFAULT_AMOUNT,
            max_exit_delay_seconds: DEFAULT_EXIT_DELAY_SECONDS,
            verification_retry_seconds: 60,
        }
    }
}
impl Settings {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.verification_retry_seconds > 0,
            "bond verification retry must be positive"
        );
        ensure!(
            self.max_exit_delay_seconds <= MAX_EXIT_DELAY_SECONDS,
            "maximum exit delay exceeds the contract limit"
        );
        if let Some(contract) = &self.contract {
            ensure!(address(contract)? != Address::ZERO, "zero escrow address");
        }
        ensure!(
            self.providers.minimum_amount > 0,
            "bond requirements must have a nonzero amount"
        );
        Ok(())
    }
    /// The published reference to `bond_id` in this escrow.
    pub fn proof(&self, rpc: &Rpc, bond_id: Option<&str>) -> Result<Option<Proof>> {
        bond_id
            .map(|id| {
                let _: B256 = id.parse()?;
                Ok(Proof {
                    chain_id: rpc.network.chain_id(),
                    contract: self.contract.clone().context("missing escrow")?,
                    bond_id: id.into(),
                })
            })
            .transpose()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Proof {
    pub chain_id: u64,
    pub contract: String,
    pub bond_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Intent {
    Lock {
        mesh_id: String,
        host_key: String,
        #[serde(with = "decimal")]
        amount: u64,
        exit_delay_seconds: u64,
        salt: String,
    },
    Withdraw {
        bond_id: String,
    },
    RequestExit {
        bond_id: String,
    },
}

pub fn mesh_binding(mesh_id: &str) -> Result<B256> {
    ensure!(
        !mesh_id.is_empty() && mesh_id.len() <= 256,
        "invalid mesh ID"
    );
    Ok(keccak256(
        [b"mesh.tempo.bond.v1/".as_slice(), mesh_id.as_bytes()].concat(),
    ))
}
pub fn host_binding(host: &str) -> Result<B256> {
    crate::config::validate_peer(host)?;
    let key = B256::from_slice(&hex::decode(host)?);
    ensure!(key != B256::ZERO, "empty host key");
    Ok(key)
}
/// The salt of the default bond: one per mesh and host key, so a repeated
/// `bond_lock` without a salt resumes that bond instead of locking another.
pub fn default_salt(mesh_id: &str, host: &str) -> Result<String> {
    let (mesh, host) = (mesh_binding(mesh_id)?, host_binding(host)?);
    let seed = [
        b"tempo.bond.v1/default-salt/".as_slice(),
        mesh.as_slice(),
        host.as_slice(),
    ];
    Ok(keccak256(seed.concat()).to_string())
}
/// The operation ID of a lock that names none, derived from its salt.
pub fn lock_operation(salt: &str) -> Result<String> {
    let salt: B256 = salt.parse()?;
    let digest = keccak256([b"tempo.bond.v1/operation/".as_slice(), salt.as_slice()].concat());
    Ok(uuid::Uuid::from_slice(&digest[..16])?.to_string())
}
/// The operation ID of an exit or withdrawal that names none, derived from
/// its kind and bond, so it is resumed rather than repeated.
pub fn exit_operation(kind: &str, bond_id: &str) -> Result<String> {
    let bond: B256 = bond_id.parse()?;
    let seed = [
        b"tempo.bond.v1/operation/".as_slice(),
        kind.as_bytes(),
        b"/",
        bond.as_slice(),
    ];
    let digest = keccak256(seed.concat());
    Ok(uuid::Uuid::from_slice(&digest[..16])?.to_string())
}

pub fn bond_id(rpc: &Rpc, contract: &str, wallet: &str, salt: &str) -> Result<B256> {
    Ok(keccak256(
        (
            U256::from(rpc.network.chain_id()),
            address(contract)?,
            address(wallet)?,
            salt.parse::<B256>()?,
        )
            .abi_encode(),
    ))
}
pub fn runtime_hash() -> B256 {
    keccak256(
        hex::decode(
            include_str!("../contracts/runtime.hex")
                .trim()
                .trim_start_matches("0x"),
        )
        .expect("checked-in escrow bytecode"),
    )
}
pub async fn check_contract(rpc: &Rpc, contract: &str, tag: &str) -> Result<()> {
    address(contract)?;
    let code = rpc.call("eth_getCode", json!([contract, tag])).await?;
    let bytes = hex::decode(
        code.as_str()
            .context("missing escrow code")?
            .trim_start_matches("0x"),
    )?;
    ensure!(
        keccak256(bytes) == runtime_hash(),
        "escrow runtime bytecode does not match TempoBondEscrow"
    );
    ensure!(
        receipt::call(rpc, contract, Escrow::tokenCall {}, tag).await?
            == address(crate::contract::OUSD)?,
        "escrow uses another token"
    );
    Ok(())
}

/// How long a decision that depends only on configuration stays valid;
/// Mesh discards it sooner when the verifier restarts or the evidence
/// changes.
const POLICY_VALIDITY_SECONDS: u64 = 365 * 24 * 60 * 60;

/// Apply provider policy, checking required funds at one finalized block.
pub async fn verify(
    rpc: &Rpc,
    settings: &Settings,
    mesh_id: &str,
    host: &str,
    evidence: Value,
) -> Result<Value> {
    let requirement = &settings.providers;
    let Some(contract) = settings.contract.as_deref() else {
        // Without an escrow no bond can be checked, so none qualifies.
        let valid_until = now().saturating_add(POLICY_VALIDITY_SECONDS);
        let reason = "no bond escrow is configured on this network";
        return Ok(json!({"accepted":false,"reason":reason,"valid_until":valid_until}));
    };
    ensure!(
        serde_json::to_vec(&evidence)?.len() <= 1024,
        "oversized bond reference"
    );
    let proof: Proof = serde_json::from_value(evidence)?;
    ensure!(
        proof.chain_id == rpc.network.chain_id(),
        "bond is on another network"
    );
    ensure!(
        address(&proof.contract)? == address(contract)?,
        "untrusted escrow address"
    );
    let id: B256 = proof.bond_id.parse()?;
    let expected_mesh = mesh_binding(mesh_id)?;
    let expected_host = host_binding(host)?;
    rpc.check_network().await?;
    let block = rpc.finalized().await?;
    let timestamp = quantity(&block["timestamp"])?;
    ensure!(
        timestamp <= now().saturating_add(5) && now().saturating_sub(timestamp) <= MAX_STATE_AGE,
        "finalized RPC state is stale or in the future"
    );
    let tag = format!("0x{:x}", quantity(&block["number"])?);
    check_contract(rpc, &proof.contract, &tag).await?;
    let bond = receipt::call(rpc, &proof.contract, Escrow::getBondCall { id }, &tag).await?;
    ensure!(
        bond.meshId == expected_mesh && bond.hostKey == expected_host,
        "bond belongs to another mesh or host key"
    );
    ensure!(
        !bond.withdrawn
            && bond.refundWallet != Address::ZERO
            && bond.amount >= requirement.minimum_amount,
        "bond absent, withdrawn or below minimum amount"
    );
    ensure!(
        bond.exitDelay >= requirement.minimum_exit_delay_seconds,
        "bond exit delay is below minimum"
    );
    // Even an exit requested in the next block cannot release funds before this.
    let valid_until = if bond.withdrawAt == U256::ZERO {
        timestamp.saturating_add(bond.exitDelay)
    } else {
        u64::try_from(bond.withdrawAt).context("bond withdrawal deadline exceeds clock range")?
    };
    ensure!(
        valid_until > timestamp && valid_until > now(),
        "bond is withdrawable or finalized evidence lease already expired"
    );
    Ok(
        json!({"accepted":true,"valid_until":valid_until,"amount":bond.amount.to_string(),"exit_delay_seconds":bond.exitDelay,"withdraw_at":bond.withdrawAt.to_string(),"block":tag}),
    )
}

pub fn lock_data(intent: &Intent) -> Result<Vec<u8>> {
    let Intent::Lock {
        mesh_id,
        host_key,
        amount,
        exit_delay_seconds,
        salt,
    } = intent
    else {
        anyhow::bail!("expected lock intent")
    };
    Ok(Escrow::lockCall {
        meshId: mesh_binding(mesh_id)?,
        hostKey: host_binding(host_key)?,
        amount: alloy::primitives::aliases::U96::from(*amount),
        exitDelay: *exit_delay_seconds,
        salt: salt.parse()?,
    }
    .abi_encode())
}

pub struct Service {
    rpc: Rpc,
    chain_verification: tokio::sync::Mutex<()>,
}
impl Service {
    pub fn new(config: &crate::config::Config) -> Result<Self> {
        Ok(Self {
            rpc: Rpc::new(config.wallet.rpc_url().into(), config.wallet.network)?,
            chain_verification: tokio::sync::Mutex::new(()),
        })
    }
    pub fn proof(&self, settings: &Settings, bond_id: Option<&str>) -> Result<Option<Proof>> {
        settings.proof(&self.rpc, bond_id)
    }
    /// Apply provider policy on a Mesh cache miss; only Mesh caches eligibility.
    pub async fn verify(
        &self,
        settings: &Settings,
        mesh: &str,
        host: &str,
        evidence: Value,
    ) -> Result<Value> {
        settings.validate()?;
        // Direct operation calls also need a bound on concurrent chain reads.
        let _guard = self
            .chain_verification
            .try_lock()
            .context("bond verifier is busy")?;
        tokio::time::timeout(
            Duration::from_secs(10),
            verify(&self.rpc, settings, mesh, host, evidence),
        )
        .await
        .context("bond verification deadline exceeded")?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_operations_are_derived_from_their_kind_and_bond() {
        let bond = format!("0x{}", "ab".repeat(32));
        let operation = exit_operation("withdraw", &bond).unwrap();
        assert_eq!(
            exit_operation("withdraw", &bond.to_uppercase().replace("0X", "0x")).unwrap(),
            operation
        );
        assert_ne!(exit_operation("request_exit", &bond).unwrap(), operation);
        let other = format!("0x{}", "cd".repeat(32));
        assert_ne!(exit_operation("withdraw", &other).unwrap(), operation);
        assert!(exit_operation("withdraw", "not a bond").is_err());
    }

    #[tokio::test]
    async fn chain_verification_is_bounded_and_needs_an_escrow() {
        let config: crate::config::Config = serde_json::from_value(json!({
            "data_dir":"/absolute/payments", "wallet":{}
        }))
        .unwrap();
        let service = Service::new(&config).unwrap();
        let required: Settings = serde_json::from_value(json!({
            "contract":"0x1111111111111111111111111111111111111111",
            "providers":{}
        }))
        .unwrap();
        let guard = service.chain_verification.lock().await;
        assert!(
            service
                .verify(&required, "mesh", "host", json!({}))
                .await
                .unwrap_err()
                .to_string()
                .contains("busy")
        );
        drop(guard);
        // Without an escrow no bond qualifies, and no chain is read.
        let verdict = service
            .verify(&config.bonds, "mesh", "host", json!({}))
            .await
            .unwrap();
        assert_eq!(verdict["accepted"], false);
        assert!(
            verdict["reason"]
                .as_str()
                .unwrap()
                .contains("no bond escrow")
        );
        assert!(verdict["valid_until"].as_u64().unwrap() > now() + 300 * 24 * 60 * 60);
    }

    #[test]
    fn provider_requirements_always_apply_and_need_funded_minimums() {
        let settings: Settings = serde_json::from_value(json!({})).unwrap();
        settings.validate().unwrap();
        // A requirement can be lowered but not turned off.
        assert!(serde_json::from_value::<Settings>(json!({"providers":null})).is_err());
        let explicit: Settings = serde_json::from_value(json!({"providers":{}})).unwrap();
        assert_eq!(explicit.providers.minimum_amount, 1);
        assert_eq!(explicit.providers.minimum_exit_delay_seconds, 0);
        explicit.validate().unwrap();
        let zero: Settings =
            serde_json::from_value(json!({"contract":"0x1111111111111111111111111111111111111111","providers":{"minimum_amount":"0"}})).unwrap();
        assert!(zero.validate().is_err());
    }

    #[test]
    fn obsolete_renewal_and_maturity_settings_are_rejected() {
        assert!(serde_json::from_value::<Settings>(json!({"enabled":true})).is_err());
        assert!(serde_json::from_value::<Settings>(json!({"renewal": {"enabled": true}})).is_err());
        assert!(
            serde_json::from_value::<Settings>(json!({"buyers": {"minimum_amount":"1"}})).is_err()
        );
        assert!(serde_json::from_value::<Settings>(json!({"max_lock_seconds": 60})).is_err());
        assert!(
            serde_json::from_value::<Requirement>(json!({"minimum_lock_seconds": 60})).is_err()
        );
        assert!(
            Settings {
                max_exit_delay_seconds: MAX_EXIT_DELAY_SECONDS + 1,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn mesh_and_host_bindings_are_unambiguous() {
        assert_ne!(mesh_binding("a").unwrap(), mesh_binding("b").unwrap());
        assert!(mesh_binding("").is_err());
        assert!(host_binding(&"00".repeat(32)).is_err());
        assert_eq!(
            host_binding(&"ab".repeat(32)).unwrap().as_slice(),
            &[0xab; 32]
        );
        assert_ne!(runtime_hash(), keccak256([]));
    }
}
