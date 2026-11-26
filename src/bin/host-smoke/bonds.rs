//! Explicit Moderato trial using real wallets, escrow, gossip, and inference hooks.
use super::*;
use alloy::primitives::{B256, U256, keccak256};

pub(super) struct Trial<'a> {
    pub mesh: &'a Path,
    pub root: &'a Path,
    pub client: &'a reqwest::Client,
    pub seller_ports: Ports,
    pub buyer_ports: Ports,
    pub seller: Option<Running>,
    pub buyer: Option<Running>,
}
impl Trial<'_> {
    fn directory(&self, seller: bool) -> PathBuf {
        self.root.join(if seller { "seller" } else { "buyer" })
    }
    fn settings(&self, seller: bool) -> Result<Value> {
        Ok(serde_json::from_slice(&std::fs::read(
            self.directory(seller).join("payments.json"),
        )?)?)
    }
    fn save(&self, seller: bool, value: &Value) -> Result<()> {
        save_settings(&self.directory(seller), value)
    }
    async fn restart(&mut self) -> Result<()> {
        drop(self.buyer.take());
        drop(self.seller.take());
        let (seller, _) =
            start_seller(self.mesh, self.root, self.seller_ports, self.client, true).await?;
        self.seller = Some(seller);
        self.buyer =
            Some(start_buyer(self.mesh, self.root, self.buyer_ports, self.client, true).await?);
        Ok(())
    }
    fn configure(&self, seller: bool, contract: &str) -> Result<()> {
        let mut config = self.settings(seller)?;
        config["bonds"] = json!({"contract":contract,"max_lock":if seller {"1000000"} else {"0"},"max_exit_delay_seconds":if seller {600} else {0},
            "verification_retry_seconds":1,"providers":{"minimum_amount":"1000000","minimum_exit_delay_seconds":30}});
        self.save(seller, &config)
    }
    async fn operation(&self, port: u16, input: Value) -> Result<Value> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
        loop {
            let result = control(self.client, port, input.clone()).await?;
            match result["status"].as_str() {
                Some("finalized") => return Ok(result),
                Some("pending") => {}
                _ => anyhow::bail!("bond operation did not succeed: {result}"),
            }
            ensure!(
                tokio::time::Instant::now() < deadline,
                "bond operation is still pending; journal preserved"
            );
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
    async fn state(&self, contract: &str, bond: &str) -> Result<Value> {
        let selector = keccak256("getBond(bytes32)");
        let data = format!(
            "0x{}{}",
            hex::encode(&selector[..4]),
            bond.trim_start_matches("0x")
        );
        let response = tempo_rpc(
            self.client,
            "eth_call",
            json!([{"to":contract,"data":data},"finalized"]),
        )
        .await?;
        let bytes = hex::decode(
            response
                .as_str()
                .context("invalid bond state")?
                .trim_start_matches("0x"),
        )?;
        ensure!(bytes.len() == 224, "unexpected bond ABI");
        let word = |n: usize| U256::from_be_slice(&bytes[n * 32..(n + 1) * 32]);
        Ok(
            json!({"mesh":format!("0x{}",hex::encode(&bytes[..32])),"host":hex::encode(&bytes[32..64]),
            "refund_wallet":format!("0x{}",hex::encode(&bytes[76..96])),"amount":word(3).to_string(),"exit_delay_seconds":u64::try_from(word(4))?,"withdraw_at":word(5).to_string(),"withdrawn":word(6)!=U256::ZERO}),
        )
    }
    /// Sends one request; the buyer pays when the seller prices the model.
    async fn inference(&mut self) -> Result<Value> {
        buyer_ready(self.client, self.buyer_ports, self.buyer.as_mut().unwrap()).await?;
        let request = json!({"model":"mesh-payment-fixture","messages":[{"role":"user","content":"bond trial"}],"max_tokens":4});
        let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
        loop {
            let response = self
                .client
                .post(format!(
                    "http://127.0.0.1:{}/v1/chat/completions",
                    self.buyer_ports.api
                ))
                .json(&request)
                .send()
                .await?;
            let status = response.status();
            let body: Value = response.json().await?;
            if status.is_success() {
                ensure!(
                    body["usage"]["completion_tokens"] == 4,
                    "unexpected inference usage"
                );
                return Ok(json!({"status":status.as_u16(),"usage":body["usage"]}));
            }
            ensure!(
                tokio::time::Instant::now() < deadline,
                "unexpected inference admission status {status}: {body}"
            );
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
    async fn verification(&mut self, evidence: Value) -> Result<Value> {
        let port = self.seller_ports.console;
        let context = wait(
            "provider mesh identity",
            self.seller.as_mut().unwrap(),
            || async {
                let context = control(self.client, port, json!({"command":"bond_context"})).await?;
                ensure!(
                    context["mesh_id"]
                        .as_str()
                        .is_some_and(|mesh| !mesh.is_empty()),
                    "mesh identity not established"
                );
                Ok(context)
            },
        )
        .await?;
        let client = self.client;
        let buyer_port = self.buyer_ports.console;
        wait("bond verification", self.buyer.as_mut().unwrap(), || async {
            let verdict = plugin_operation(
                client,
                buyer_port,
                "verify_peer_evidence",
                json!({"mesh_id":context["mesh_id"],"node_id":context["node_id"],"evidence":evidence}),
            ).await?;
            ensure!(verdict["reason"] != "bond verifier is busy", "bond verifier is busy");
            Ok(verdict)
        }).await
    }
    async fn provider_unavailable(&mut self) -> Result<Value> {
        let client = self.client;
        let seller_ports = self.seller_ports;
        let buyer_ports = self.buyer_ports;
        let result = wait(
            "connected provider unavailability",
            self.buyer.as_mut().unwrap(),
            || async {
                let seller = status(client, seller_ports.console).await?;
                let buyer = status(client, buyer_ports.console).await?;
                for (port, expected) in [
                    (seller_ports.console, &buyer["node_id"]),
                    (buyer_ports.console, &seller["node_id"]),
                ] {
                    let host = get(client, port, "/api/status").await?;
                    let peer = host["peers"]
                        .as_array()
                        .context("peer list missing")?
                        .iter()
                        .find(|peer| peer["node_id"] == *expected && peer["connected"] == true)
                        .context("trial peer is not connected")?;
                    if port == buyer_ports.console {
                        ensure!(
                            ["models", "serving_models", "hosted_models"]
                                .iter()
                                .any(|field| {
                                    peer[*field].as_array().is_some_and(|models| {
                                        models.iter().any(|id| id == "mesh-payment-fixture")
                                    })
                                }),
                            "buyer has not received the provider advertisement"
                        );
                    }
                }
                let seller_models = get(client, seller_ports.api, "/v1/models").await?;
                ensure!(
                    seller_models["data"]
                        .as_array()
                        .context("seller models missing")?
                        .iter()
                        .any(|model| model["id"] == "mesh-payment-fixture"),
                    "seller fixture is not ready"
                );
                let models = get(client, buyer_ports.api, "/v1/models").await?;
                ensure!(
                    !models["data"]
                        .as_array()
                        .context("buyer models missing")?
                        .iter()
                        .any(|model| model["id"] == "mesh-payment-fixture"),
                    "provider is still selectable"
                );
                // Catalog absence does not distinguish pending from rejected verdicts.
                Ok(json!({"connected":true,"provider_advertised":true,"buyer_models":models,"mesh_verdict":"pending_or_rejected"}))
            },
        )
        .await?;
        ensure!(
            self.seller.as_mut().unwrap().0.try_wait()?.is_none(),
            "seller exited during catalog check"
        );
        Ok(result)
    }
    fn policies(&self, priced: bool) -> Result<()> {
        for seller in [true, false] {
            let mut config = self.settings(seller)?;
            // Startup writes config prices into the ledger without removing
            // others, so an unpriced trial stores a free price.
            let rate = if seller && priced { "1000000" } else { "0" };
            config["prices"] = json!({"mesh-payment-fixture":{"input_per_million":rate,"output_per_million":rate}});
            config["bonds"]["providers"] =
                json!({"minimum_amount":"1000000","minimum_exit_delay_seconds":30});
            self.save(seller, &config)?;
        }
        Ok(())
    }
    pub async fn execute(mut self) -> Result<()> {
        if self.root.join("bond-result.json").exists() {
            println!("Bond trial already completed; existing evidence preserved");
            return Ok(());
        }
        let checkpoint_path = self.root.join("bond-release-checkpoint.json");
        if checkpoint_path.exists() {
            let checkpoint: Value = serde_json::from_slice(&std::fs::read(&checkpoint_path)?)?;
            return self.release(checkpoint).await;
        }
        let contract = self.escrow()?;
        let bond = self.lock_bond(&contract).await?;
        let checkpoint = self.check_admission(&contract, bond).await?;
        saved_operation(&checkpoint_path, || checkpoint.clone())?;
        self.release(checkpoint).await
    }
    /// The network's shared escrow is deployed once, outside the plugin;
    /// `TEMPO_BOND_ESCROW` overrides the plugin's Moderato default.
    fn escrow(&self) -> Result<String> {
        let path = self.root.join("bond-deployment.json");
        if !path.exists() {
            let contract = std::env::var("TEMPO_BOND_ESCROW")
                .ok()
                .filter(|contract| !contract.is_empty())
                .unwrap_or_else(|| "0xd210B0356b3e45B37F29Ec763106dc23BF1cC3aa".into());
            saved_operation(&path, || json!({ "contract": contract }))?;
        }
        let deployment: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
        let contract = deployment["contract"]
            .as_str()
            .context("escrow record omitted its address")?;
        println!("Using the bond escrow at {contract}");
        Ok(contract.to_string())
    }
    async fn lock_bond(&mut self, contract: &str) -> Result<Value> {
        for seller in [true, false] {
            self.configure(seller, contract)?;
        }
        self.policies(true)?;
        // Both nodes run while the provider bonds, so the buyer's view is
        // observed before and after publication without a restart.
        self.restart().await?;
        let port = self.seller_ports.console;
        let context = wait(
            "provider mesh identity",
            self.seller.as_mut().unwrap(),
            || async {
                let context = control(self.client, port, json!({"command":"bond_context"})).await?;
                ensure!(
                    context["mesh_id"].is_string(),
                    "mesh identity not established"
                );
                Ok(context)
            },
        )
        .await?;
        let existing = context["reference"]["bond_id"].clone();
        let unbonded = if existing.is_string() {
            Value::Null
        } else {
            println!("Checking the buyer cannot select the provider before its bond");
            self.provider_unavailable().await?
        };
        let result = if existing.is_string() {
            json!({"bond_id":existing,"status":"existing"})
        } else {
            let path = self.directory(true).join("bond-lock-input.json");
            let attempts = control(
                self.client,
                port,
                json!({"command":"transactions","limit":100}),
            )
            .await?;
            if !path.exists() {
                ensure!(
                    !attempts["attempts"]
                        .as_array()
                        .context("attempt list missing")?
                        .iter()
                        .any(|a| a["intent"]["kind"] == "lock"),
                    "existing bond lock has no saved input; inspect attempts and publish it with bond_publish before resuming"
                );
            }
            let input = saved_operation(&path, || {
                json!({"command":"bond_lock","operation_id":uuid::Uuid::new_v4().to_string(),
                "mesh_id":context["mesh_id"],
                "amount":"1000000","exit_delay_seconds":30,"salt":B256::from(rand::random::<[u8;32]>()).to_string()})
            })?;
            self.operation(port, input).await?
        };
        let id = result["bond_id"]
            .as_str()
            .context("bond ID missing")?
            .to_string();
        let initial = self.state(contract, &id).await?;
        ensure!(
            initial["withdrawn"] == false,
            "existing bond was already refunded; use a fresh trial directory"
        );
        // The plugin publishes the finalized lock; the running buyer then
        // selects the provider and pays it, with neither node restarted.
        println!("Checking the buyer selects the provider once its bond is published");
        buyer_ready(self.client, self.buyer_ports, self.buyer.as_mut().unwrap()).await?;
        Ok(json!({"id":id,"initial":initial,"operation":result,"unbonded":unbonded}))
    }
    async fn check_admission(&mut self, contract: &str, mut bond: Value) -> Result<Value> {
        println!("Testing paid inference with the provider bonded and the buyer unbonded");
        let paid = self.inference().await?;
        cleared(
            self.client,
            self.buyer_ports.console,
            self.buyer.as_mut().unwrap(),
        )
        .await?;
        cleared(
            self.client,
            self.seller_ports.console,
            self.seller.as_mut().unwrap(),
        )
        .await?;
        println!("Checking the provider bond remains active across restart without renewal");
        self.restart().await?;
        bond["active"] = self.state(contract, bond["id"].as_str().unwrap()).await?;
        ensure!(
            bond["active"]["withdraw_at"] == "0"
                && bond["active"]["amount"] == bond["initial"]["amount"],
            "active bond changed without an exit request"
        );
        self.policies(false)?;
        self.restart().await?;
        println!("Testing free inference with a bonded provider");
        let free_bonded = self.inference().await?;
        let id = bond["id"].clone();
        let port = self.seller_ports.console;
        control(self.client, port, json!({"command":"bond_publish"})).await?;
        self.restart().await?;
        println!("Checking rejection of a provider with no published bond");
        let rejected = self.verification(Value::Null).await?;
        ensure!(
            rejected["accepted"] == false
                && rejected["reason"]
                    .as_str()
                    .is_some_and(|reason| reason.contains("no bond")),
            "missing proof was not rejected: {rejected}"
        );
        let unbonded_catalog = self.provider_unavailable().await?;
        let republish = json!({"command":"bond_publish","bond_id":id});
        control(self.client, self.seller_ports.console, republish).await?;
        self.restart().await?;
        self.inference().await?;
        println!("Checking the provider stays funded beyond its exit delay without renewal");
        let finalized = tempo_rpc(
            self.client,
            "eth_getBlockByNumber",
            json!(["finalized", false]),
        )
        .await?;
        let timestamp = |block: &Value| -> Result<U256> {
            Ok(U256::from_str_radix(
                block["timestamp"]
                    .as_str()
                    .context("finalized timestamp missing")?
                    .trim_start_matches("0x"),
                16,
            )?)
        };
        let target = timestamp(&finalized)? + U256::from(31);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
        loop {
            let block = tempo_rpc(
                self.client,
                "eth_getBlockByNumber",
                json!(["finalized", false]),
            )
            .await?;
            if timestamp(&block)? >= target {
                break;
            }
            ensure!(
                tokio::time::Instant::now() < deadline,
                "finalized chain time did not advance"
            );
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        bond["active"] = self.state(contract, bond["id"].as_str().unwrap()).await?;
        ensure!(
            bond["active"]["withdraw_at"] == "0" && bond["active"]["withdrawn"] == false,
            "provider lock expired without an exit request"
        );
        Ok(
            json!({"escrow":contract,"bond":bond,"paid_bonded_provider":paid,"free_bonded_provider":free_bonded,"unbonded_provider":{"verification":rejected,"catalog":unbonded_catalog}}),
        )
    }
    async fn release(mut self, checkpoint: Value) -> Result<()> {
        let contract = checkpoint["escrow"]
            .as_str()
            .context("checkpoint escrow missing")?
            .to_string();
        let mut bond = checkpoint["bond"].clone();
        ensure!(
            bond.is_object(),
            "checkpoint must contain the provider bond"
        );
        self.policies(false)?;
        self.restart().await?;
        println!("Requesting bond exits, then refunding full principals after the delay");
        self.request_exit(&contract, &mut bond).await?;
        self.refund_bond(&contract, &mut bond).await?;
        self.restart().await?;
        let verification = self
            .verification(json!({"chain_id":42431,"contract":contract,"bond_id":bond["id"]}))
            .await?;
        ensure!(
            verification["accepted"] == false
                && verification["reason"]
                    .as_str()
                    .is_some_and(|reason| reason.contains("withdrawn")),
            "withdrawn seller was not rejected by bond verification: {verification}"
        );
        let withdrawn =
            json!({"verification":verification,"catalog":self.provider_unavailable().await?});
        let result = json!({"network":"Moderato","chain_id":42431,"escrow":contract,"bond":bond,
            "paid_bonded_provider":checkpoint["paid_bonded_provider"],"free_bonded_provider":checkpoint["free_bonded_provider"],"unbonded_provider":checkpoint["unbonded_provider"],"withdrawn_provider":withdrawn});
        private_write(
            &self.root.join("bond-result.json"),
            &serde_json::to_string_pretty(&result)?,
        )?;
        println!("Moderato bond trial passed; evidence saved in bond-result.json");
        Ok(())
    }
    /// The seller's withdrawal of `id`, which its plugin signs once the exit
    /// delay has passed.
    async fn swept(&self, port: u16, id: &str) -> Result<Value> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        loop {
            let listing = control(
                self.client,
                port,
                json!({"command":"transactions","limit":20}),
            )
            .await?;
            let refund = listing["attempts"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|a| {
                    a["intent"]["kind"] == "withdraw"
                        && a["status"] == "finalized"
                        && a["bond_id"]
                            .as_str()
                            .is_some_and(|b| b.eq_ignore_ascii_case(id))
                });
            if let Some(refund) = refund {
                return Ok(refund.clone());
            }
            ensure!(
                tokio::time::Instant::now() < deadline,
                "the plugin did not withdraw the exited bond"
            );
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
    async fn request_exit(&mut self, contract: &str, bond: &mut Value) -> Result<()> {
        let port = self.seller_ports.console;
        let input = saved_operation(
            &self.directory(true).join("bond-exit-input.json"),
            || json!({"command":"bond_exit","operation_id":uuid::Uuid::new_v4().to_string(),"bond_id":bond["id"]}),
        )?;
        bond["exit"] = self.operation(port, input).await?;
        let id = bond["id"].as_str().context("bond ID missing")?;
        let exiting = self.state(contract, id).await?;
        let finalized = tempo_rpc(
            self.client,
            "eth_getBlockByNumber",
            json!(["finalized", false]),
        )
        .await?;
        let timestamp = U256::from_str_radix(
            finalized["timestamp"]
                .as_str()
                .context("finalized timestamp missing")?
                .trim_start_matches("0x"),
            16,
        )?;
        let deadline = U256::from_str_radix(
            exiting["withdraw_at"]
                .as_str()
                .context("exit deadline missing")?,
            10,
        )?;
        let check_path = self.directory(true).join("bond-exit-check.json");
        if check_path.exists() {
            let checks: Value = serde_json::from_slice(&std::fs::read(check_path)?)?;
            for key in ["exiting", "exit_verification", "early_withdraw_rejected"] {
                bond[key] = checks[key].clone();
            }
            return Ok(());
        }
        if deadline <= timestamp {
            // A resumed trial still refunds funds if this observation window passed.
            bond["exit_check_skipped"] = json!("resumed after exit delay elapsed");
            return Ok(());
        }
        ensure!(
            exiting["withdrawn"] == false
                && exiting["amount"] == bond["initial"]["amount"]
                && deadline > timestamp,
            "exit did not retain the principal throughout its delay"
        );
        let verification = self
            .verification(json!({"chain_id":42431,"contract":contract,"bond_id":id}))
            .await?;
        ensure!(
            verification["accepted"] == true
                && verification["valid_until"]
                    .as_u64()
                    .is_some_and(|until| U256::from(until) <= deadline),
            "exiting bond must qualify only while its principal is locked: {verification}"
        );
        let selector = keccak256("withdraw(bytes32)");
        let response: Value = self.client.post(TEMPO_RPC).json(&json!({"jsonrpc":"2.0","id":1,"method":"eth_call","params":[{
            "from":exiting["refund_wallet"],"to":contract,"data":format!("0x{}{}",hex::encode(&selector[..4]),id.trim_start_matches("0x"))
        },finalized["number"]]})).send().await?.error_for_status()?.json().await?;
        ensure!(
            response["error"]["message"]
                .as_str()
                .is_some_and(|reason| reason.contains("bond still locked or withdrawn")),
            "early withdrawal did not revert for the exit delay: {response}"
        );
        bond["exiting"] = exiting;
        bond["exit_verification"] = verification;
        bond["early_withdraw_rejected"] = json!(true);
        saved_operation(&check_path, || {
            json!({
                "exiting":bond["exiting"],"exit_verification":bond["exit_verification"],
                "early_withdraw_rejected":true
            })
        })?;
        Ok(())
    }
    async fn refund_bond(&mut self, contract: &str, bond: &mut Value) -> Result<()> {
        let id = bond["id"].as_str().unwrap().to_string();
        loop {
            let state = self.state(contract, &id).await?;
            let deadline = U256::from_str_radix(
                state["withdraw_at"]
                    .as_str()
                    .context("exit deadline missing")?,
                10,
            )?;
            let finalized = tempo_rpc(
                self.client,
                "eth_getBlockByNumber",
                json!(["finalized", false]),
            )
            .await?;
            let timestamp = U256::from_str_radix(
                finalized["timestamp"]
                    .as_str()
                    .context("finalized timestamp missing")?
                    .trim_start_matches("0x"),
                16,
            )?;
            if deadline != U256::ZERO && timestamp >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        let port = self.seller_ports.console;
        let check_path = self.directory(true).join("bond-withdrawable-check.json");
        let verification = if check_path.exists() {
            serde_json::from_slice(&std::fs::read(&check_path)?)?
        } else {
            let verification = self
                .verification(json!({"chain_id":42431,"contract":contract,"bond_id":id}))
                .await?;
            ensure!(
                verification["accepted"] == false
                    && verification["reason"]
                        .as_str()
                        .is_some_and(|reason| reason.contains("withdrawable")
                            || reason.contains("withdrawn")),
                "withdrawable bond remained eligible: {verification}"
            );
            saved_operation(&check_path, || verification)?
        };
        bond["withdrawable_verification"] = verification;
        bond["refund"] = self.swept(port, &id).await?;
        ensure!(
            self.state(contract, &id).await?["withdrawn"] == true,
            "refund did not consume bond"
        );
        let hash = bond["refund"]["hash"]
            .as_str()
            .context("refund transaction hash missing")?;
        let receipt = tempo_rpc(self.client, "eth_getTransactionReceipt", json!([hash])).await?;
        let expected = U256::from_str_radix(bond["initial"]["amount"].as_str().unwrap(), 10)?;
        let wallet = self.state(contract, &id).await?["refund_wallet"]
            .as_str()
            .unwrap()
            .to_string();
        let transfer = receipt["logs"]
            .as_array()
            .context("refund logs missing")?
            .iter()
            .any(|log| {
                log["address"]
                    .as_str()
                    .is_some_and(|address| address.eq_ignore_ascii_case(TEST_OUSD))
                    && log["topics"][0]
                        == keccak256("Transfer(address,address,uint256)").to_string()
                    && log["topics"][1].as_str().is_some_and(|from| {
                        from.eq_ignore_ascii_case(&format!(
                            "0x{:0>64}",
                            contract.trim_start_matches("0x")
                        ))
                    })
                    && log["topics"][2].as_str().is_some_and(|to| {
                        to.eq_ignore_ascii_case(&format!(
                            "0x{:0>64}",
                            wallet.trim_start_matches("0x")
                        ))
                    })
                    && log["data"].as_str().and_then(|data| {
                        U256::from_str_radix(data.trim_start_matches("0x"), 16).ok()
                    }) == Some(expected)
            });
        ensure!(
            transfer,
            "refund receipt lacks the exact principal transfer to the original wallet"
        );
        bond["refund_principal_verified"] = json!(true);
        Ok(())
    }
}
fn saved_operation(path: &Path, create: impl FnOnce() -> Value) -> Result<Value> {
    if path.exists() {
        return Ok(serde_json::from_slice(&std::fs::read(path)?)?);
    }
    let input = create();
    private_write(path, &serde_json::to_string_pretty(&input)?)?;
    #[cfg(unix)]
    std::fs::File::open(path.parent().context("operation directory missing")?)?.sync_all()?;
    Ok(input)
}

pub(super) async fn resume(mesh: &Path, root: &Path) -> Result<()> {
    let root = root.canonicalize()?;
    if root.join("bond-result.json").exists() {
        println!("Bond trial already completed; existing evidence preserved");
        return Ok(());
    }
    ensure!(
        root.join("bond-deployment.json").exists(),
        "resume requires an existing escrow record"
    );
    let client = trial_client()?;
    let seller_ports = Ports::new()?;
    let buyer_ports = Ports::new()?;
    let (seller, _) = start_seller(mesh, &root, seller_ports, &client, true).await?;
    let buyer = start_buyer(mesh, &root, buyer_ports, &client, true).await?;
    Trial {
        mesh,
        root: &root,
        client: &client,
        seller_ports,
        buyer_ports,
        seller: Some(seller),
        buyer: Some(buyer),
    }
    .execute()
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resumed_operations_keep_the_exact_original_inputs() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("lock.json");
        let input = json!({"command":"bond_lock","operation_id":uuid::Uuid::from_u128(42).to_string(),"exit_delay_seconds":123,"salt":B256::repeat_byte(3).to_string()});
        assert_eq!(saved_operation(&path, || input.clone()).unwrap(), input);
        assert_eq!(
            saved_operation(&path, || panic!("must not generate new transaction inputs")).unwrap(),
            input
        );
        std::fs::write(&path, "{").unwrap();
        assert!(saved_operation(&path, || panic!("must not replace damaged input")).is_err());
    }
}
