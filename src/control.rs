//! The `control` operation: payment controls named as in the Lightning
//! plugin, wallet sends, and the Tempo-only recovery and bond commands.
use crate::{
    config::{Policy, Price},
    contract::{decimal, decimal_option},
    engine::Engine,
};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Map, Value, json};

#[derive(Debug, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Control {
    Balance {},
    /// The most recent requests, channels and wallet attempts, newest first.
    Transactions {
        limit: u32,
    },
    /// Active requests and channels, and unresolved wallet attempts.
    Pending {},
    /// Where to send OUSD to fund this wallet.
    Fund {},
    /// A journaled OUSD transfer out of the wallet, once per operation ID.
    Send {
        operation_id: String,
        to: String,
        #[serde(with = "decimal")]
        amount: u64,
        #[serde(with = "decimal")]
        max_fee: u64,
    },
    Policy {
        value: Option<Policy>,
    },
    Pricing {},
    SetPricing {
        model: String,
        value: Option<Price>,
    },
    /// Patch only the cached-input discount of an already priced model.
    SetCacheDiscount {
        model: String,
        percent: u8,
    },
    AdmissionDeposit {
        #[serde(default, with = "decimal_option")]
        value: Option<u64>,
        /// Forget the set node-wide deposit.
        #[serde(default)]
        reset: bool,
    },
    SetModelAdmissionDeposit {
        model: String,
        #[serde(default, with = "decimal_option")]
        value: Option<u64>,
    },
    Retry {
        request_id: Option<String>,
    },
    /// Close the buyer's channel with a seller, named by its node ID or a
    /// unique prefix of at least eight characters; without one, the
    /// buyer's only open channel.
    Recover {
        peer: Option<String>,
    },
    Waive {
        request_id: String,
    },
    Cancel {
        request_id: String,
    },
    /// Forgive a buyer's bills that closed channels lost, so the seller
    /// accepts its requests again.
    Unblock {
        peer: String,
    },
    BondContext {},
    /// Lock a bond; amount and exit delay default to 1 OUSD and the
    /// network's default delay, a minute on testnet and a day on mainnet.
    /// Every input is optional: the observed mesh, the default bond, and a
    /// salt and operation ID derived from the mesh and host key, so the
    /// same command resumes the same bond.
    BondLock {
        operation_id: Option<String>,
        mesh_id: Option<String>,
        amount: Option<String>,
        exit_delay_seconds: Option<u64>,
        salt: Option<String>,
    },
    /// Start a bond's exit countdown, after which the plugin withdraws it
    /// to this wallet; the bond defaults to the newest locked one, and the
    /// operation ID to one derived from it, so the same command resumes the
    /// same exit.
    BondExit {
        operation_id: Option<String>,
        bond_id: Option<String>,
    },
    /// Publish a bond as this node's evidence, or withdraw the reference
    /// without `bond_id`.
    BondPublish {
        bond_id: Option<String>,
    },
}

const COMMANDS: [&str; 20] = [
    "balance",
    "transactions",
    "pending",
    "fund",
    "send",
    "policy",
    "pricing",
    "set_pricing",
    "set_cache_discount",
    "admission_deposit",
    "set_model_admission_deposit",
    "retry",
    "recover",
    "waive",
    "cancel",
    "unblock",
    "bond_context",
    "bond_lock",
    "bond_exit",
    "bond_publish",
];

/// The operation's input schema.
pub(crate) fn schema() -> Map<String, Value> {
    let string = json!({"type": "string"});
    let properties = json!({
        "command": {"type": "string", "enum": COMMANDS},
        "limit": {"type": "integer", "minimum": 1, "maximum": 100},
        "value": {"type": ["object", "string", "null"]},
        "reset": {"type": "boolean"},
        "model": string, "percent": {"type": "integer", "minimum": 0, "maximum": 100},
        "to": string, "amount": string, "max_fee": string,
        "request_id": string, "peer": string, "operation_id": string, "mesh_id": string,
        "exit_delay_seconds": {"type": "integer"}, "salt": string, "bond_id": string
    });
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": properties,
        "required": ["command"]
    });
    schema.as_object().unwrap().clone()
}

impl Control {
    pub(crate) async fn run(self, engine: &Engine, mesh_id: Option<String>) -> Result<Value> {
        match self {
            Self::Balance {} => Ok(serde_json::to_value(engine.wallet.balance().await?)?),
            Self::Transactions { limit } => transactions(engine, limit),
            Self::Pending {} => pending(engine),
            Self::Fund {} => match engine.wallet.fund().await? {
                Some(funding) => Ok(funding),
                None => Ok(json!({
                    "address": engine.wallet_info.address,
                    "network": engine.config.wallet.network,
                    "asset": engine.wallet_info.asset,
                })),
            },
            Self::Send {
                operation_id,
                to,
                amount,
                max_fee,
            } => {
                engine
                    .wallet
                    .send(&operation_id, &to, amount, max_fee)
                    .await
            }
            Self::Policy { value } => {
                if let Some(value) = value {
                    ensure!(
                        value.budget().is_none() || !engine.wallet_info.is_absent(),
                        crate::engine::NEEDS_WALLET
                    );
                    engine.config.check_buying(&value)?;
                    engine.ledger.set_policy(&value)?;
                }
                engine.ledger.policy_status()
            }
            Self::Pricing {} => pricing(engine),
            Self::SetPricing { model, value } => {
                engine.set_pricing(&model, value.as_ref()).await?;
                pricing(engine)
            }
            Self::SetCacheDiscount { model, percent } => {
                engine.set_cache_discount(&model, percent).await?;
                pricing(engine)
            }
            Self::AdmissionDeposit { value, reset } => {
                ensure!(
                    !(reset && value.is_some()),
                    "set an admission deposit or reset it, not both"
                );
                if reset {
                    engine.ledger.reset_admission_deposit()?;
                }
                if let Some(value) = value {
                    engine.ledger.set_admission_deposit(value)?;
                }
                deposits(engine)
            }
            Self::SetModelAdmissionDeposit { model, value } => {
                engine.ledger.set_model_admission_deposit(&model, value)?;
                deposits(engine)
            }
            other => other.operate(engine, mesh_id).await,
        }
    }
    /// The Tempo-only payment recovery and bond commands.
    async fn operate(self, engine: &Engine, mesh_id: Option<String>) -> Result<Value> {
        // A finalized lock is published, and a withdrawn bond unpublished, at once.
        let bond = |operation_id: String, intent| async move {
            let settings = &engine.config.bonds;
            let result = engine.wallet.bond(settings, &operation_id, intent).await?;
            engine.sync_bond().await?;
            Ok::<_, anyhow::Error>(result)
        };
        match self {
            Self::Retry { request_id } => {
                if let Some(id) = &request_id {
                    engine.mpp_retry_one(id).await?;
                } else {
                    engine.retry().await?;
                }
                Ok(json!({"reconciled": true, "request_id": request_id}))
            }
            Self::Recover { peer } => {
                let peer = engine.buyer_close(peer.as_deref()).await?;
                Ok(json!({"command": "recover", "peer": peer}))
            }
            Self::Waive { request_id } => request_control(engine, request_id, "waive").await,
            Self::Cancel { request_id } => request_control(engine, request_id, "cancel").await,
            Self::Unblock { peer } => {
                let bills = engine.unblock(&peer).await?;
                Ok(json!({"peer": peer, "bills": bills}))
            }
            Self::BondContext {} => Ok(json!({
                "mesh_id": mesh_id,
                "node_id": engine.node,
                "namespace": crate::bonds::NAMESPACE,
                "reference": engine
                    .bonds
                    .proof(&engine.config.bonds, engine.ledger.published_bond()?.as_deref())?,
                "requirement": engine.config.bonds.providers,
            })),
            lock @ Self::BondLock { .. } => {
                let (operation_id, intent) = lock.lock_intent(
                    &engine.node,
                    mesh_id,
                    engine.config.wallet.network.bond_exit_delay(),
                )?;
                bond(operation_id, intent).await
            }
            Self::BondExit {
                operation_id,
                bond_id,
            } => {
                let (operation_id, bond_id) = exit_inputs(engine, operation_id, bond_id)?;
                bond(operation_id, crate::bonds::Intent::RequestExit { bond_id }).await
            }
            Self::BondPublish { bond_id } => {
                engine.set_published_bond(bond_id.as_deref()).await?;
                Ok(json!({"bond_id": bond_id}))
            }
            _ => unreachable!("payment controls run in Control::run"),
        }
    }
}

impl Control {
    /// The operation ID and intent of a `bond_lock`, filling in what it omits.
    fn lock_intent(
        self,
        host: &str,
        observed: Option<String>,
        default_exit_delay: u64,
    ) -> Result<(String, crate::bonds::Intent)> {
        let Self::BondLock {
            operation_id,
            mesh_id,
            amount,
            exit_delay_seconds,
            salt,
        } = self
        else {
            anyhow::bail!("not a bond lock");
        };
        let mesh_id = match mesh_id {
            // An explicit mesh guards against a mesh change racing the command.
            Some(intended) => {
                ensure!(
                    observed.as_deref() == Some(intended.as_str()),
                    "intended mesh differs from observed mesh ID"
                );
                intended
            }
            None => observed.context("the mesh ID is not known yet; join a mesh first")?,
        };
        let salt = match salt {
            Some(salt) => salt,
            None => crate::bonds::default_salt(&mesh_id, host)?,
        };
        let operation_id = match operation_id {
            Some(id) => id,
            None => crate::bonds::lock_operation(&salt)?,
        };
        let intent = crate::bonds::Intent::Lock {
            mesh_id,
            host_key: host.into(),
            amount: amount.map_or(Ok(crate::bonds::DEFAULT_AMOUNT), |a| {
                a.parse().context("invalid bond amount")
            })?,
            exit_delay_seconds: exit_delay_seconds.unwrap_or(default_exit_delay),
            salt,
        };
        Ok((operation_id, intent))
    }
}

/// The operation ID and bond of an exit, filling in what it omits.
fn exit_inputs(
    engine: &Engine,
    operation_id: Option<String>,
    bond_id: Option<String>,
) -> Result<(String, String)> {
    let bond_id = match bond_id {
        Some(id) => id,
        None => engine
            .newest_bond()?
            .context("no finalized bond lock; name the bond with bond_id")?,
    };
    let operation_id = match operation_id {
        Some(id) => id,
        None => crate::bonds::exit_operation("request_exit", &bond_id)?,
    };
    Ok((operation_id, bond_id))
}

async fn request_control(engine: &Engine, request_id: String, command: &str) -> Result<Value> {
    engine.mpp_control(&request_id, command).await?;
    Ok(json!({"command": command, "request_id": request_id}))
}

fn transactions(engine: &Engine, limit: u32) -> Result<Value> {
    ensure!(
        (1..=100).contains(&limit),
        "limit must be between 1 and 100"
    );
    let mut attempts = engine.wallet.attempts()?;
    attempts.reverse();
    attempts.truncate(limit as usize);
    Ok(unsigned(json!({
        "requests": engine.ledger.mpp_history(limit)?,
        "channels": engine.ledger.channel_history(limit)?,
        "attempts": attempts,
    })))
}

fn pending(engine: &Engine) -> Result<Value> {
    let mut attempts = engine.wallet.attempts()?;
    attempts.retain(|attempt| attempt["status"] == "pending");
    Ok(unsigned(json!({
        "requests": engine.ledger.mpp_active()?,
        "channels": engine.ledger.channels_active()?,
        "attempts": attempts,
    })))
}

/// Removes voucher signatures from channel and attempt listings: control
/// output never shows signed bytes.
fn unsigned(mut listing: Value) -> Value {
    for list in ["channels", "attempts"] {
        for item in listing[list].as_array_mut().into_iter().flatten() {
            if let Some(voucher) = item["voucher"].as_object_mut() {
                voucher.remove("signature");
            }
        }
    }
    listing
}

/// The node-wide admission deposit and each model's own.
fn deposits(engine: &Engine) -> Result<Value> {
    let models: Map<String, Value> = engine
        .ledger
        .model_admission_deposits()?
        .into_iter()
        .map(|(model, deposit)| (model, deposit.to_string().into()))
        .collect();
    Ok(json!({
        "admission_deposit": engine.ledger.admission_deposit()?.to_string(),
        "models": models,
    }))
}

/// Seller prices with the admission deposits.
fn pricing(engine: &Engine) -> Result<Value> {
    let mut value = deposits(engine)?;
    value["prices"] = serde_json::to_value(engine.ledger.pricing()?)?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listings_drop_voucher_signatures_from_channels_and_attempts() {
        let listing = unsigned(json!({
            "channels": [{"voucher": {"cum": "10", "signature": "0x01"}}, {"voucher": null}],
            "attempts": [{"kind": "close", "voucher": {"cum": "5", "signature": "0x02"}}, {"kind": "open"}]
        }));
        assert_eq!(listing["channels"][0]["voucher"], json!({"cum": "10"}));
        assert_eq!(listing["attempts"][0]["voucher"], json!({"cum": "5"}));
        assert!(!listing.to_string().contains("signature"));
    }

    #[test]
    fn controls_require_request_ids_and_reject_ignored_arguments() {
        for input in [
            json!({"command": "recover", "request_id": "id"}),
            json!({"command": "waive", "request_id": 123}),
            json!({"command": "cancel", "request_id": "id", "limit": 20}),
            json!({"command": "balance", "request_id": "id"}),
            json!({"command": "resubmit", "request_id": "id"}),
            json!({"command": "status"}),
            json!({"command": "history", "limit": 1}),
            json!({"command": "attempts"}),
            json!({"command": "transactions"}),
            json!({"command": "pending", "limit": 1}),
            json!({"command": "fund", "amount": "1"}),
            json!({"command": "send", "operation_id": "id", "to": "0x1", "amount": 1, "max_fee": "1"}),
            json!({"command": "send", "operation_id": "id", "to": "0x1", "amount": "1"}),
            json!({"command": "policy", "value": {"mode": "always"}}),
            json!({"command": "policy", "value": {"daily_budget_msat": "1"}}),
            json!({"command": "pricing", "model": "m"}),
            json!({"command": "set_pricing", "value": null}),
            json!({"command": "set_cache_discount", "model": "m", "percent": 256}),
            json!({"command": "admission_deposit", "value_msat": "1"}),
            json!({"command": "set_model_admission_deposit", "model": "m", "value": 1}),
        ] {
            assert!(
                serde_json::from_value::<Control>(input.clone()).is_err(),
                "{input}"
            );
        }
        assert!(
            matches!(serde_json::from_value::<Control>(json!({"command":"retry", "request_id":"id"})).unwrap(), Control::Retry { request_id: Some(id) } if id == "id")
        );
        assert!(matches!(
            serde_json::from_value::<Control>(json!({"command":"retry"})).unwrap(),
            Control::Retry { request_id: None }
        ));
    }

    #[test]
    fn lightning_style_controls_parse_amounts_as_decimal_strings() {
        let parse = |value| serde_json::from_value::<Control>(value).unwrap();
        assert!(matches!(
            parse(json!({"command":"transactions","limit":5})),
            Control::Transactions { limit: 5 }
        ));
        assert!(matches!(
            parse(
                json!({"command":"send","operation_id":"op","to":"0xab","amount":"18446744073709551615","max_fee":"7"})
            ),
            Control::Send {
                amount: u64::MAX,
                max_fee: 7,
                ..
            }
        ));
        assert!(matches!(
            parse(json!({"command":"policy"})),
            Control::Policy { value: None }
        ));
        assert!(matches!(
            parse(json!({"command":"policy","value":{"mode":"automatic","daily_budget":"9"}})),
            Control::Policy {
                value: Some(Policy {
                    daily_budget: Some(9),
                    ..
                })
            }
        ));
        assert!(matches!(
            parse(json!({"command":"set_pricing","model":"m"})),
            Control::SetPricing { value: None, .. }
        ));
        assert!(matches!(
            parse(
                json!({"command":"set_pricing","model":"m","value":{"input_per_million":"1","output_per_million":"2"}})
            ),
            Control::SetPricing {
                value: Some(Price {
                    output_per_million: 2,
                    ..
                }),
                ..
            }
        ));
        assert!(matches!(
            parse(json!({"command":"admission_deposit","reset":true})),
            Control::AdmissionDeposit {
                value: None,
                reset: true
            }
        ));
        assert!(matches!(
            parse(json!({"command":"set_model_admission_deposit","model":"m","value":"4"})),
            Control::SetModelAdmissionDeposit { value: Some(4), .. }
        ));
        let schema = schema();
        let listed = &schema["properties"]["command"]["enum"];
        assert_eq!(listed.as_array().unwrap().len(), COMMANDS.len());
        for command in COMMANDS {
            // Every listed command is a variant: only argument errors remain.
            let error = serde_json::from_value::<Control>(json!({"command": command})).err();
            assert!(error.is_none_or(|e| !e.to_string().contains("unknown variant")));
        }
    }

    #[test]
    fn a_bond_lock_without_inputs_resumes_the_default_bond_for_the_observed_mesh() {
        let host = "aa".repeat(32);
        let lock = || serde_json::from_value::<Control>(json!({"command": "bond_lock"})).unwrap();
        let (operation, intent) = lock().lock_intent(&host, Some("mesh".into()), 60).unwrap();
        let (again, same) = lock().lock_intent(&host, Some("mesh".into()), 60).unwrap();
        assert_eq!((&operation, &intent), (&again, &same));
        let crate::bonds::Intent::Lock {
            mesh_id,
            amount,
            exit_delay_seconds,
            salt,
            ..
        } = intent
        else {
            panic!("not a lock")
        };
        assert_eq!(
            (mesh_id.as_str(), amount, exit_delay_seconds),
            ("mesh", 1_000_000, 60)
        );
        assert_eq!(salt, crate::bonds::default_salt("mesh", &host).unwrap());
        assert_eq!(operation, crate::bonds::lock_operation(&salt).unwrap());
        // Another mesh gets another bond; an unknown mesh is refused.
        let (other, _) = lock().lock_intent(&host, Some("other".into()), 60).unwrap();
        assert_ne!(other, operation);
        assert!(lock().lock_intent(&host, None, 60).is_err());
        let explicit = json!({"command": "bond_lock", "mesh_id": "intended"});
        let explicit = serde_json::from_value::<Control>(explicit).unwrap();
        assert!(
            explicit
                .lock_intent(&host, Some("mesh".into()), 60)
                .is_err()
        );
    }
}
