//! Loopback RPC evidence only: real Tempo signing/ABI, deterministic reserve execution fixture.
use super::{
    journal::{Attempt, Journal},
    keys::Profile,
    receipt as chain,
    rpc::{Rpc, quantity},
    wallet::{Settings, Wallet},
};
use crate::contract::{
    Network, OUSD,
    mpp::{ChannelFunding, ChannelTerms, RESERVE, Request, Response, Voucher},
};
use alloy::{
    consensus::transaction::SignerRecoverable,
    eips::eip2718::Decodable2718,
    primitives::{Address, B256, Signature, TxKind, U256, aliases::U96, keccak256},
    signers::{Signer, local::PrivateKeySigner},
    sol_types::{SolCall, SolEvent, SolValue},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use tempo_alloy::{
    contracts::precompiles::{ISignatureVerifier, ITIP20ChannelReserve as Reserve},
    primitives::TempoTxEnvelope,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

#[path = "bond_fixture_tests.rs"]
mod bond_tests;

struct Channel {
    descriptor: Reserve::ChannelDescriptor,
    deposit: u64,
    close_at: u32,
    closed: bool,
}
pub(crate) struct Fixture {
    pub(crate) chain_id: u64,
    bonds: BTreeMap<B256, crate::bonds::Escrow::Bond>,
    bond_code_valid: bool,
    calls: Vec<String>,
    channels: BTreeMap<B256, Channel>,
    receipts: BTreeMap<String, Value>,
    raws: Vec<String>,
    nonces: BTreeMap<Address, u64>,
    balances: BTreeMap<Address, u64>,
    now: u64,
    finalized_height: u64,
    next_finalized: Option<(u64, u64)>,
    lose_reply: bool,
    pub(crate) hide_receipt: bool,
    hidden_receipts: std::collections::BTreeSet<String>,
    revert: bool,
}
impl Default for Fixture {
    fn default() -> Self {
        Self {
            chain_id: Network::Testnet.chain_id(),
            bonds: Default::default(),
            bond_code_valid: true,
            calls: Vec::new(),
            channels: BTreeMap::new(),
            receipts: BTreeMap::new(),
            raws: Vec::new(),
            nonces: BTreeMap::new(),
            balances: BTreeMap::new(),
            now: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            finalized_height: 10,
            next_finalized: None,
            lose_reply: false,
            hide_receipt: false,
            hidden_receipts: Default::default(),
            revert: false,
        }
    }
}
fn abi(value: impl SolValue) -> Value {
    json!(format!("0x{}", hex::encode(value.abi_encode())))
}
fn bytes(value: &Value) -> Result<Vec<u8>> {
    Ok(hex::decode(
        value
            .as_str()
            .context("expected hex")?
            .trim_start_matches("0x"),
    )?)
}
fn log<E: SolEvent>(event: E, hash: &str) -> Value {
    log_at(RESERVE, event, hash)
}
fn log_at<E: SolEvent>(address: &str, event: E, hash: &str) -> Value {
    let data = event.encode_log_data();
    json!({"address":address,"topics":data.topics().iter().map(ToString::to_string).collect::<Vec<_>>(),"data":format!("0x{}",hex::encode(data.data)),
        "transactionHash":hash,"blockHash":B256::repeat_byte(0xbb).to_string(),"logIndex":"0x0","removed":false})
}
impl Fixture {
    fn call(&mut self, method: &str, params: &Value) -> Result<Value> {
        self.calls.push(method.into());
        match method {
            "eth_chainId" => Ok(json!(format!("0x{:x}", self.chain_id))),
            "eth_getCode" => Ok(
                if params[0]
                    .as_str()
                    .unwrap()
                    .eq_ignore_ascii_case(bond_tests::ESCROW)
                    && self.bond_code_valid
                {
                    json!(include_str!("../../contracts/runtime.hex").trim())
                } else {
                    json!("0xef")
                },
            ),
            "eth_gasPrice" => Ok(json!("0x23c34600")),
            "eth_estimateGas" => {
                ensure!(
                    params[0]["feeToken"]
                        .as_str()
                        .unwrap()
                        .eq_ignore_ascii_case(OUSD),
                    "wrong fee token"
                );
                Ok(json!("0xc350"))
            }
            "eth_getBlockByNumber" => {
                let finalized = params[0] == "finalized";
                let height = if finalized {
                    self.finalized_height
                } else {
                    quantity(&params[0])?
                };
                let block = json!({"number":format!("0x{height:x}"),"hash":B256::repeat_byte(0xbb).to_string(),"timestamp":format!("0x{:x}",self.now)});
                if finalized && let Some((height, timestamp)) = self.next_finalized.take() {
                    self.finalized_height = height;
                    self.now = timestamp;
                }
                Ok(block)
            }
            "eth_getTransactionCount" => {
                if params[1] != "pending" {
                    ensure!(
                        params[1]["blockHash"] == B256::repeat_byte(0xbb).to_string()
                            && params[1]["requireCanonical"] == true,
                        "nonce lookup must use the finalized block hash"
                    );
                }
                Ok(json!(format!(
                    "0x{:x}",
                    self.nonces
                        .get(&params[0].as_str().unwrap().parse::<Address>()?)
                        .copied()
                        .unwrap_or_default()
                )))
            }
            "eth_getTransactionReceipt" => Ok(
                if self.hide_receipt || self.hidden_receipts.contains(params[0].as_str().unwrap()) {
                    Value::Null
                } else {
                    self.receipts
                        .get(params[0].as_str().unwrap())
                        .cloned()
                        .unwrap_or(Value::Null)
                },
            ),
            "eth_blockNumber" => Ok(json!(format!("0x{:x}", self.finalized_height))),
            "eth_getLogs" => self.logs(&params[0]),
            "eth_call" => self.view(&params[0]),
            "eth_sendRawTransaction" => self.send(params[0].as_str().unwrap()),
            _ => anyhow::bail!("unexpected fixture RPC {method}"),
        }
    }
    /// Logs of receipts in the filter's block range whose topics match, where a
    /// null topic matches anything and a list matches any of its entries.
    fn logs(&self, filter: &Value) -> Result<Value> {
        let bound = |key: &str, default: u64| match &filter[key] {
            Value::String(tag) if tag.starts_with("0x") => quantity(&filter[key]),
            _ => Ok(default),
        };
        let range = bound("fromBlock", 0)?..=bound("toBlock", u64::MAX)?;
        let wanted = filter["topics"].as_array().context("missing topics")?;
        let mut logs = Vec::new();
        for receipt in self.receipts.values() {
            if !range.contains(&quantity(&receipt["blockNumber"])?) {
                continue;
            }
            for log in receipt["logs"].as_array().context("missing logs")? {
                let matches = wanted.iter().enumerate().all(|(i, want)| match want {
                    Value::Null => true,
                    Value::Array(any) => any.contains(&log["topics"][i]),
                    one => log["topics"][i] == *one,
                });
                if matches {
                    logs.push(log.clone());
                }
            }
        }
        Ok(json!(logs))
    }
    fn view(&self, request: &Value) -> Result<Value> {
        let data = bytes(&request["data"])?;
        if request["to"].as_str().unwrap().eq_ignore_ascii_case(OUSD) {
            return Ok(match data.get(..4) {
                Some([0x31, 0x3c, 0xe5, 0x67]) => abi(U256::from(6)),
                Some([0x70, 0xa0, 0x82, 0x31]) => {
                    let who = Address::from_slice(&data[16..36]);
                    abi(U256::from(
                        self.balances.get(&who).copied().unwrap_or(1_000_000),
                    ))
                }
                _ => abi(U256::ZERO),
            });
        }
        if request["to"]
            .as_str()
            .unwrap()
            .eq_ignore_ascii_case(bond_tests::ESCROW)
        {
            return bond_tests::view(self, &data);
        }
        let selector = &data[..4];
        if selector == Reserve::domainSeparatorCall::SELECTOR {
            return Ok(abi(chain::domain(Network::from_chain_id(self.chain_id)?)));
        }
        if selector == Reserve::CLOSE_GRACE_PERIODCall::SELECTOR {
            return Ok(abi(900u64));
        }
        if selector == Reserve::getVoucherDigestCall::SELECTOR {
            let call = Reserve::getVoucherDigestCall::abi_decode_validate(&data)?;
            return Ok(abi(chain::voucher_digest(
                Network::from_chain_id(self.chain_id)?,
                call.channelId,
                u64::try_from(call.cumulativeAmount)?,
            )));
        }
        if selector == ISignatureVerifier::verifyCall::SELECTOR {
            let call = ISignatureVerifier::verifyCall::abi_decode_validate(&data)?;
            let valid = Signature::from_raw(&call.signature)?
                .recover_address_from_prehash(&call.hash)?
                == call.signer;
            return Ok(abi(valid));
        }
        let call = Reserve::getChannelStateCall::abi_decode_validate(&data)?;
        let state = self
            .channels
            .get(&call.channelId)
            .filter(|c| !c.closed)
            .map(|c| Reserve::ChannelState {
                settled: U96::ZERO,
                deposit: U96::from(c.deposit),
                closeRequestedAt: c.close_at,
            })
            .unwrap_or(Reserve::ChannelState {
                settled: U96::ZERO,
                deposit: U96::ZERO,
                closeRequestedAt: 0,
            });
        Ok(abi(state))
    }
    fn send(&mut self, raw: &str) -> Result<Value> {
        let encoded = hex::decode(raw.trim_start_matches("0x"))?;
        let hash = keccak256(&encoded).to_string();
        if self.receipts.contains_key(&hash) {
            return Ok(json!(hash));
        }
        let envelope = TempoTxEnvelope::decode_2718(&mut encoded.as_slice())?;
        let signed = envelope.as_aa().context("not a native Tempo transaction")?;
        let payer = signed.recover_signer()?;
        let tx = signed.tx();
        ensure!(
            tx.chain_id == self.chain_id && tx.fee_token == Some(OUSD.parse()?),
            "wrong transaction domain"
        );
        let reserve = tx.calls.len() == 1 && tx.calls[0].to == TxKind::Call(RESERVE.parse()?);
        let token = tx.calls.len() == 1 && tx.calls[0].to == TxKind::Call(OUSD.parse()?);
        ensure!(
            tx.nonce == *self.nonces.get(&payer).unwrap_or(&0),
            "nonce reused"
        );
        let data = &tx.calls[0].input;
        let context = signed.expiring_nonce_hash(payer);
        let logs = if self.revert {
            Vec::new()
        } else if reserve {
            self.execute(payer, context, data, &hash)?
        } else if token {
            self.transfer(payer, data, &hash)?
        } else {
            bond_tests::execute(self, payer, &tx.calls)?;
            Vec::new()
        };
        self.nonces.insert(payer, tx.nonce + 1);
        self.raws.push(raw.into());
        self.receipts.insert(hash.clone(),json!({"transactionHash":hash,"blockNumber":"0xa","blockHash":B256::repeat_byte(0xbb).to_string(),
            "status":if self.revert {"0x0"}else{"0x1"},"from":payer.to_string(),"logs":logs}));
        if self.lose_reply {
            self.lose_reply = false;
            anyhow::bail!("fixture lost response after inclusion");
        }
        Ok(json!(hash))
    }
    /// A TIP-20 transfer from the payer's balance.
    fn transfer(&mut self, payer: Address, data: &[u8], hash: &str) -> Result<Vec<Value>> {
        let call = super::send::transferCall::abi_decode_validate(data)?;
        let amount = u64::try_from(call.amount)?;
        let balance = self.balances.entry(payer).or_insert(1_000_000);
        ensure!(*balance >= amount, "insufficient token balance");
        *balance -= amount;
        *self.balances.entry(call.to).or_insert(1_000_000) += amount;
        let event = super::send::Transfer {
            from: payer,
            to: call.to,
            amount: call.amount,
        };
        Ok(vec![log_at(OUSD, event, hash)])
    }
    fn channel_id(&self, descriptor: &Reserve::ChannelDescriptor) -> Result<B256> {
        Ok(chain::channel_id(
            Network::from_chain_id(self.chain_id)?,
            descriptor,
        ))
    }
    /// Executes one reserve call with the precompile's semantics and returns its logs.
    fn execute(
        &mut self,
        payer: Address,
        context: B256,
        data: &[u8],
        hash: &str,
    ) -> Result<Vec<Value>> {
        let selector: [u8; 4] = data[..4].try_into()?;
        match selector {
            Reserve::openCall::SELECTOR => self.open(payer, context, data, hash),
            Reserve::topUpCall::SELECTOR => self.top_up(payer, data, hash),
            Reserve::requestCloseCall::SELECTOR => {
                let call = Reserve::requestCloseCall::abi_decode_validate(data)?;
                let id = self.channel_id(&call.descriptor)?;
                let now = self.now;
                let c = self.channels.get_mut(&id).context("unknown channel")?;
                ensure!(!c.closed && payer == c.descriptor.payer, "not payer");
                if c.close_at > 0 {
                    return Ok(Vec::new());
                }
                c.close_at = now.try_into()?;
                Ok(vec![log(
                    Reserve::CloseRequested {
                        channelId: id,
                        payer,
                        payee: c.descriptor.payee,
                        closeGraceEnd: U256::from(now + 900),
                    },
                    hash,
                )])
            }
            Reserve::closeCall::SELECTOR => {
                let call = Reserve::closeCall::abi_decode_validate(data)?;
                ensure!(
                    payer == call.descriptor.payee && call.captureAmount <= call.cumulativeAmount,
                    "unauthorized capture"
                );
                // Nothing ever settles, so only a nonzero capture needs a voucher.
                if call.captureAmount > U96::ZERO {
                    let digest = chain::voucher_digest(
                        Network::from_chain_id(self.chain_id)?,
                        self.channel_id(&call.descriptor)?,
                        u64::try_from(call.cumulativeAmount)?,
                    );
                    ensure!(
                        Signature::from_raw(&call.signature)?
                            .recover_address_from_prehash(&digest)?
                            == call.descriptor.payer,
                        "invalid voucher signature"
                    );
                }
                self.close(&call.descriptor, u64::try_from(call.captureAmount)?, hash)
            }
            _ => {
                let call = Reserve::withdrawCall::abi_decode_validate(data)?;
                let c = self
                    .channels
                    .get(&self.channel_id(&call.descriptor)?)
                    .context("unknown channel")?;
                ensure!(
                    payer == c.descriptor.payer
                        && c.close_at > 0
                        && self.now >= u64::from(c.close_at) + 900,
                    "withdraw before grace end"
                );
                self.close(&call.descriptor, 0, hash)
            }
        }
    }
    fn open(
        &mut self,
        payer: Address,
        context: B256,
        data: &[u8],
        hash: &str,
    ) -> Result<Vec<Value>> {
        let call = Reserve::openCall::abi_decode_validate(data)?;
        ensure!(
            call.operator == Address::ZERO
                && call.authorizedSigner == Address::ZERO
                && call.token == OUSD.parse::<Address>()?,
            "unexpected channel authority"
        );
        let d = Reserve::ChannelDescriptor {
            payer,
            payee: call.payee,
            operator: call.operator,
            token: call.token,
            salt: call.salt,
            authorizedSigner: call.authorizedSigner,
            expiringNonceHash: context,
        };
        let id = self.channel_id(&d)?;
        let deposit = u64::try_from(call.deposit)?;
        *self.balances.entry(payer).or_insert(1_000_000) -= deposit;
        self.channels.insert(
            id,
            Channel {
                descriptor: d.clone(),
                deposit,
                close_at: 0,
                closed: false,
            },
        );
        Ok(vec![log(
            Reserve::ChannelOpened {
                channelId: id,
                payer,
                payee: d.payee,
                operator: d.operator,
                token: d.token,
                authorizedSigner: d.authorizedSigner,
                salt: d.salt,
                expiringNonceHash: context,
                deposit: call.deposit,
            },
            hash,
        )])
    }
    fn top_up(&mut self, payer: Address, data: &[u8], hash: &str) -> Result<Vec<Value>> {
        let call = Reserve::topUpCall::abi_decode_validate(data)?;
        let id = self.channel_id(&call.descriptor)?;
        let amount = u64::try_from(call.additionalDeposit)?;
        let c = self.channels.get_mut(&id).context("unknown channel")?;
        ensure!(!c.closed && payer == c.descriptor.payer, "not payer");
        *self.balances.entry(payer).or_insert(1_000_000) -= amount;
        let c = self.channels.get_mut(&id).unwrap();
        c.deposit += amount;
        let mut logs = Vec::new();
        if c.close_at > 0 {
            c.close_at = 0;
            logs.push(log(
                Reserve::CloseRequestCancelled {
                    channelId: id,
                    payer,
                    payee: c.descriptor.payee,
                },
                hash,
            ));
        }
        logs.push(log(
            Reserve::TopUp {
                channelId: id,
                payer,
                payee: c.descriptor.payee,
                additionalDeposit: call.additionalDeposit,
                newDeposit: U96::from(c.deposit),
            },
            hash,
        ));
        Ok(logs)
    }
    fn close(
        &mut self,
        d: &Reserve::ChannelDescriptor,
        capture: u64,
        hash: &str,
    ) -> Result<Vec<Value>> {
        let id = self.channel_id(d)?;
        let c = self.channels.get_mut(&id).context("unknown channel")?;
        ensure!(
            !c.closed && capture <= c.deposit,
            "closed channel or excessive capture"
        );
        c.closed = true;
        let refund = c.deposit - capture;
        *self.balances.entry(d.payee).or_insert(1_000_000) += capture;
        *self.balances.entry(d.payer).or_insert(1_000_000) += refund;
        Ok(vec![log(
            Reserve::ChannelClosed {
                channelId: id,
                payer: d.payer,
                payee: d.payee,
                settledToPayee: U96::from(capture),
                refundedToPayer: U96::from(refund),
            },
            hash,
        )])
    }
}
async fn connection(mut socket: TcpStream, fixture: Arc<Mutex<Fixture>>) {
    let mut data = Vec::new();
    let mut buffer = [0u8; 4096];
    let (start, length) = loop {
        let count = socket.read(&mut buffer).await.unwrap();
        if count == 0 {
            return;
        }
        data.extend_from_slice(&buffer[..count]);
        if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            let header = std::str::from_utf8(&data[..end]).unwrap();
            let length = header
                .lines()
                .find_map(|line| {
                    let (k, v) = line.split_once(':')?;
                    k.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            break (end + 4, length);
        }
    };
    while data.len() < start + length {
        let count = socket.read(&mut buffer).await.unwrap();
        assert!(count > 0);
        data.extend_from_slice(&buffer[..count]);
    }
    let request: Value = serde_json::from_slice(&data[start..start + length]).unwrap();
    let response = match fixture
        .lock()
        .unwrap()
        .call(request["method"].as_str().unwrap(), &request["params"])
    {
        Ok(value) => json!({"jsonrpc":"2.0","id":1,"result":value}),
        Err(error) => {
            json!({"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":error.to_string()}})
        }
    };
    let body = serde_json::to_vec(&response).unwrap();
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    socket.write_all(header.as_bytes()).await.unwrap();
    socket.write_all(&body).await.unwrap();
}
pub(crate) async fn server() -> (Arc<Mutex<Fixture>>, String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let fixture = Arc::new(Mutex::new(Fixture::default()));
    let state = fixture.clone();
    let task = tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            tokio::spawn(connection(socket, state.clone()));
        }
    });
    (fixture, url, task)
}
pub(crate) fn wallet(dir: &std::path::Path, url: &str, key: u8) -> Wallet {
    wallet_on_network(dir, url, key, Network::Testnet)
}
pub(crate) fn wallet_on_network(
    dir: &std::path::Path,
    url: &str,
    key: u8,
    network: Network,
) -> Wallet {
    let signer = PrivateKeySigner::from_bytes(&B256::repeat_byte(key)).unwrap();
    let address = signer.address().to_string();
    Wallet {
        settings: Settings {
            network,
            rpc_url: Some(url.into()),
            ..Default::default()
        },
        profile: Profile {
            wallet_id: format!("fixture-{key}"),
            address: address.clone(),
            chain_id: network.chain_id(),
            token: OUSD.into(),
            key_file: "unused-fixture-key".into(),
        },
        signer: Some(signer),
        rpc: Rpc::new(url.into(), network).unwrap(),
        journal: Journal::open(
            crate::ledger::Ledger::open(dir).unwrap().db.clone(),
            &format!("fixture-{key}"),
            &address,
            network,
        )
        .unwrap(),
        send_lock: Default::default(),
    }
}
fn attempt(wallet: &Wallet, id: &str) -> Attempt {
    wallet.journal.get(id).unwrap().unwrap()
}
fn terms(buyer: &Wallet, seller: &Wallet, id: u8) -> ChannelTerms {
    ChannelTerms {
        buyer_peer: "aa".repeat(32),
        seller_peer: "bb".repeat(32),
        payer: buyer.profile.address.clone(),
        payee: seller.profile.address.clone(),
        asset: buyer.settings.network.asset(),
        nonce: format!("{id:02x}").repeat(32),
    }
}
/// The journal ID of a channel's `kind` attempt.
fn attempt_id(terms: &ChannelTerms, kind: &str) -> String {
    format!("channel/{}/{kind}", terms.salt().unwrap())
}
/// Opens a channel with a deposit of 10.
fn open_request(terms: &ChannelTerms) -> Request {
    Request::Open {
        terms: terms.clone(),
        deposit: 10,
        max_fee: 1000,
    }
}

#[tokio::test]
async fn rpc_network_mismatch_is_rejected_before_token_reads_signing_or_broadcast() {
    for (selected, reported) in [
        (Network::Testnet, Network::Mainnet),
        (Network::Mainnet, Network::Testnet),
    ] {
        let (fixture, url, task) = server().await;
        fixture.lock().unwrap().chain_id = reported.chain_id();
        let buyer_dir = tempfile::tempdir().unwrap();
        let seller_dir = tempfile::tempdir().unwrap();
        let buyer = wallet_on_network(buyer_dir.path(), &url, 1, selected);
        let seller = wallet_on_network(seller_dir.path(), &url, 2, selected);
        assert!(buyer.info().await.is_err());
        assert!(buyer.balance().await.is_err());
        let error = buyer
            .mpp(open_request(&terms(&buyer, &seller, 1)))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("selected Tempo"));
        assert!(buyer.journal.all::<Attempt>().unwrap().is_empty());
        let state = fixture.lock().unwrap();
        assert_eq!(state.calls, vec!["eth_chainId"; 3]);
        assert!(state.raws.is_empty());
        task.abort();
    }
}

#[tokio::test]
async fn cross_network_wallet_requests_are_rejected_without_rpc_or_journal_changes() {
    for network in [Network::Testnet, Network::Mainnet] {
        let (fixture, url, task) = server().await;
        fixture.lock().unwrap().chain_id = network.chain_id();
        let buyer_dir = tempfile::tempdir().unwrap();
        let seller_dir = tempfile::tempdir().unwrap();
        let buyer = wallet_on_network(buyer_dir.path(), &url, 1, network);
        let seller = wallet_on_network(seller_dir.path(), &url, 2, network);
        let other = if network == Network::Testnet {
            Network::Mainnet
        } else {
            Network::Testnet
        };
        let mut wrong = terms(&buyer, &seller, 1);
        wrong.asset = other.asset();
        let funding = ChannelFunding {
            terms: wrong.clone(),
            open_tx: String::new(),
            channel_id: String::new(),
            expiring_nonce_hash: String::new(),
        };
        let voucher = Voucher {
            channel_id: String::new(),
            cum: 5,
            signature: String::new(),
        };
        for request in [
            open_request(&wrong),
            Request::TopUp {
                funding: funding.clone(),
                amount: 10,
                index: 1,
                max_fee: 1000,
            },
            Request::SignVoucher {
                funding: funding.clone(),
                cum: 5,
            },
            Request::VerifyVoucher {
                funding: funding.clone(),
                voucher: voucher.clone(),
            },
            Request::Inspect {
                funding: funding.clone(),
            },
            Request::Close {
                funding: funding.clone(),
                voucher: Some(voucher),
                capture: 5,
            },
            Request::Recover {
                funding,
                max_fee: 1000,
            },
        ] {
            assert!(
                buyer
                    .mpp(request)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("another Tempo network")
            );
        }
        assert!(buyer.journal.all::<Attempt>().unwrap().is_empty());
        assert!(fixture.lock().unwrap().calls.is_empty());
        task.abort();
    }
}

#[tokio::test]
async fn voucher_signed_for_other_network_cannot_authorize_capture() {
    for network in [Network::Testnet, Network::Mainnet] {
        let (fixture, url, task) = server().await;
        fixture.lock().unwrap().chain_id = network.chain_id();
        let buyer_dir = tempfile::tempdir().unwrap();
        let seller_dir = tempfile::tempdir().unwrap();
        let buyer = wallet_on_network(buyer_dir.path(), &url, 1, network);
        let seller = wallet_on_network(seller_dir.path(), &url, 2, network);
        let funding = fund(&buyer, &terms(&buyer, &seller, 1)).await;
        let other = if network == Network::Testnet {
            Network::Mainnet
        } else {
            Network::Testnet
        };
        let digest = chain::voucher_digest(other, funding.channel_id.parse().unwrap(), 10);
        let signature = buyer
            .signer
            .as_ref()
            .unwrap()
            .sign_hash(&digest)
            .await
            .unwrap();
        let wrong = Voucher {
            channel_id: funding.channel_id.clone(),
            cum: 10,
            signature: format!("0x{}", hex::encode(signature.as_bytes())),
        };
        assert!(
            seller
                .mpp(Request::VerifyVoucher {
                    funding: funding.clone(),
                    voucher: wrong.clone()
                })
                .await
                .is_err()
        );
        assert!(
            seller
                .mpp(Request::Close {
                    funding: funding.clone(),
                    voucher: Some(wrong),
                    capture: 5
                })
                .await
                .is_err()
        );
        assert!(seller.journal.all::<Attempt>().unwrap().is_empty());
        assert_eq!(fixture.lock().unwrap().raws.len(), 1);
        let voucher = sign(&buyer, &funding, 10).await;
        let closure = close(&seller, &funding, Some(voucher), 5).await;
        assert_eq!(closure.closure.unwrap().refunded, 5);
        task.abort();
    }
}

async fn fund(buyer: &Wallet, terms: &ChannelTerms) -> ChannelFunding {
    for _ in 0..3 {
        let result = buyer.mpp(open_request(terms)).await.unwrap();
        if let Some(funding) = result.funding {
            assert_eq!(result.deposit, 10);
            return funding;
        }
    }
    panic!("fixture open did not finalize")
}
async fn sign(buyer: &Wallet, funding: &ChannelFunding, cum: u64) -> Voucher {
    buyer
        .mpp(Request::SignVoucher {
            funding: funding.clone(),
            cum,
        })
        .await
        .unwrap()
        .voucher
        .unwrap()
}
async fn close(
    seller: &Wallet,
    funding: &ChannelFunding,
    voucher: Option<Voucher>,
    capture: u64,
) -> Response {
    for _ in 0..3 {
        let result = seller
            .mpp(Request::Close {
                funding: funding.clone(),
                voucher: voucher.clone(),
                capture,
            })
            .await
            .unwrap();
        if result.closure.is_some() {
            return result;
        }
    }
    panic!("fixture close did not finalize")
}
async fn top_up(buyer: &Wallet, funding: &ChannelFunding, amount: u64, index: u32) -> u64 {
    for _ in 0..3 {
        let result = buyer
            .mpp(Request::TopUp {
                funding: funding.clone(),
                amount,
                index,
                max_fee: 1000,
            })
            .await
            .unwrap();
        if !result.pending {
            return result.deposit;
        }
    }
    panic!("fixture top-up did not finalize")
}
/// The journal keeps the signed bytes flat in its record and hides them publicly.
fn assert_journal_encoding(original: &Attempt) {
    let encoded = serde_json::to_value(original).unwrap();
    assert!(encoded.get("tx").is_none());
    assert_eq!(encoded["raw"], original.tx.raw);
    assert_eq!(
        encoded["expiring_nonce_hash"],
        original.tx.expiring_nonce_hash
    );
    let decoded: Attempt = serde_json::from_value(encoded.clone()).unwrap();
    assert_eq!(decoded.tx.raw, original.tx.raw);
    assert_eq!(decoded.tx.nonce, original.tx.nonce);
    let public = super::transaction::public_attempt(original).unwrap();
    assert!(public.get("raw").is_none());
    let mut expected = encoded;
    expected.as_object_mut().unwrap().remove("raw");
    assert_eq!(public, expected);
}
#[tokio::test]
async fn signed_native_open_partial_capture_and_atomic_refund_survive_lost_reply_restart() {
    for network in [Network::Testnet, Network::Mainnet] {
        let (fixture, url, server) = server().await;
        fixture.lock().unwrap().chain_id = network.chain_id();
        let bd = tempfile::tempdir().unwrap();
        let sd = tempfile::tempdir().unwrap();
        let buyer = wallet_on_network(bd.path(), &url, 1, network);
        let seller = wallet_on_network(sd.path(), &url, 2, network);
        let terms = terms(&buyer, &seller, 1);
        buyer.info().await.unwrap().validate_tempo().unwrap();
        fixture.lock().unwrap().lose_reply = true;
        assert!(buyer.mpp(open_request(&terms)).await.unwrap().pending);
        let reserved = buyer.balance().await.unwrap().reserved;
        assert!(reserved > 10);
        let attempt_id = attempt_id(&terms, "open");
        let original = attempt(&buyer, &attempt_id);
        let original_salt = original.terms.salt().unwrap();
        assert_journal_encoding(&original);
        drop(buyer);
        let other = if network == Network::Testnet {
            Network::Mainnet
        } else {
            Network::Testnet
        };
        let db = crate::ledger::Ledger::open(bd.path()).unwrap().db.clone();
        assert!(Journal::open(db, "fixture-1", &original.terms.payer, other).is_err());
        let buyer = wallet_on_network(bd.path(), &url, 1, network);
        let restored = attempt(&buyer, &attempt_id);
        assert_eq!(restored.tx.raw, original.tx.raw);
        assert_eq!(restored.tx.hash, original.tx.hash);
        assert_eq!(restored.terms.salt().unwrap(), original_salt);
        assert_eq!(restored.tx.status, "pending");
        assert_eq!(buyer.balance().await.unwrap().reserved, reserved);
        let funding = fund(&buyer, &terms).await;
        assert_eq!(buyer.balance().await.unwrap().reserved, 0);
        assert_eq!(fixture.lock().unwrap().raws.len(), 1);
        let voucher = sign(&buyer, &funding, 10).await;
        let verified = seller
            .mpp(Request::VerifyVoucher {
                funding: funding.clone(),
                voucher: voucher.clone(),
            })
            .await
            .unwrap();
        assert_eq!(verified.deposit, 10);
        assert!(
            seller
                .mpp(Request::Close {
                    funding: funding.clone(),
                    voucher: Some(voucher.clone()),
                    capture: 11
                })
                .await
                .is_err()
        );
        fixture.lock().unwrap().lose_reply = true;
        assert!(
            seller
                .mpp(Request::Close {
                    funding: funding.clone(),
                    voucher: Some(voucher.clone()),
                    capture: 5
                })
                .await
                .unwrap()
                .pending
        );
        drop(seller);
        let seller = wallet_on_network(sd.path(), &url, 2, network);
        let result = close(&seller, &funding, Some(voucher.clone()), 5)
            .await
            .closure
            .unwrap();
        assert_eq!((result.captured, result.refunded), (5, 5));
        assert_eq!(fixture.lock().unwrap().raws.len(), 2);
        assert_eq!(
            buyer
                .mpp(Request::Inspect {
                    funding: funding.clone()
                })
                .await
                .unwrap()
                .closure
                .unwrap(),
            result
        );
        assert!(
            seller
                .mpp(Request::VerifyVoucher { funding, voucher })
                .await
                .is_err()
        );
        let state = fixture.lock().unwrap();
        let balance =
            |wallet: &Wallet| state.balances[&wallet.profile.address.parse::<Address>().unwrap()];
        assert_eq!((balance(&buyer), balance(&seller)), (999_995, 1_000_005));
        server.abort();
    }
}
#[tokio::test]
async fn zero_full_capture_invalid_vouchers_and_funding_identity() {
    let (fixture, url, server) = server().await;
    let bd = tempfile::tempdir().unwrap();
    let sd = tempfile::tempdir().unwrap();
    let buyer = wallet(bd.path(), &url, 1);
    let seller = wallet(sd.path(), &url, 2);
    for capture in [0, 10] {
        let terms = terms(&buyer, &seller, capture as u8 + 1);
        let funding = fund(&buyer, &terms).await;
        let voucher = sign(&buyer, &funding, 10).await;
        let verify = |funding: ChannelFunding, voucher: Voucher| {
            seller.mpp(Request::VerifyVoucher { funding, voucher })
        };
        let mut wrong = voucher.clone();
        wrong.cum = 11;
        assert!(verify(funding.clone(), wrong).await.is_err());
        let mut wrong = voucher.clone();
        wrong.signature = format!("0x{}", "00".repeat(65));
        assert!(verify(funding.clone(), wrong).await.is_err());
        let mut wrong = funding.clone();
        wrong.expiring_nonce_hash = B256::repeat_byte(9).to_string();
        assert!(verify(wrong, voucher.clone()).await.is_err());
        assert!(
            verify(funding.clone(), sign(&buyer, &funding, 11).await)
                .await
                .is_err()
        );
        // A channel opened without an upfront starts with a zero voucher.
        let zero = sign(&buyer, &funding, 0).await;
        assert_eq!(verify(funding.clone(), zero).await.unwrap().deposit, 10);
        // A zero capture closes without presenting any voucher.
        let voucher = (capture > 0).then_some(voucher);
        let result = close(&seller, &funding, voucher, capture)
            .await
            .closure
            .unwrap();
        assert_eq!((result.captured, result.refunded), (capture, 10 - capture));
    }
    assert_eq!(fixture.lock().unwrap().raws.len(), 4);
    server.abort();
}
#[tokio::test]
async fn top_up_adds_deposit_once_per_index_and_cancels_a_close_request() {
    let (fixture, url, server) = server().await;
    let bd = tempfile::tempdir().unwrap();
    let sd = tempfile::tempdir().unwrap();
    let buyer = wallet(bd.path(), &url, 1);
    let seller = wallet(sd.path(), &url, 2);
    let terms = terms(&buyer, &seller, 4);
    let funding = fund(&buyer, &terms).await;
    let large = sign(&buyer, &funding, 30).await;
    let verify = |voucher: Voucher| {
        seller.mpp(Request::VerifyVoucher {
            funding: funding.clone(),
            voucher,
        })
    };
    assert!(verify(large.clone()).await.is_err());
    let recover = Request::Recover {
        funding: funding.clone(),
        max_fee: 1000,
    };
    buyer.mpp(recover.clone()).await.unwrap();
    let requested = buyer.mpp(recover).await.unwrap();
    assert!(requested.close_requested_at > 0);
    assert_eq!(top_up(&buyer, &funding, 20, 1).await, 30);
    let status = buyer
        .mpp(Request::Inspect {
            funding: funding.clone(),
        })
        .await
        .unwrap();
    assert_eq!((status.deposit, status.close_requested_at), (30, 0));
    assert_eq!(top_up(&buyer, &funding, 20, 1).await, 30);
    assert!(
        buyer
            .mpp(Request::TopUp {
                funding: funding.clone(),
                amount: 21,
                index: 1,
                max_fee: 1000
            })
            .await
            .is_err()
    );
    assert_eq!(fixture.lock().unwrap().raws.len(), 3);
    assert_eq!(
        attempt(&buyer, &attempt_id(&terms, "topup/1")).deposit,
        Some(30)
    );
    assert_eq!(verify(large.clone()).await.unwrap().deposit, 30);
    let result = close(&seller, &funding, Some(large), 25)
        .await
        .closure
        .unwrap();
    assert_eq!((result.captured, result.refunded), (25, 5));
    server.abort();
}
#[tokio::test]
async fn payer_recovery_enforces_exact_native_grace_and_refunds_without_seller() {
    let (fixture, url, server) = server().await;
    let bd = tempfile::tempdir().unwrap();
    let sd = tempfile::tempdir().unwrap();
    let buyer = wallet(bd.path(), &url, 1);
    let seller = wallet(sd.path(), &url, 2);
    let funding = fund(&buyer, &terms(&buyer, &seller, 3)).await;
    let voucher = sign(&buyer, &funding, 5).await;
    let recover = || Request::Recover {
        funding: funding.clone(),
        max_fee: 1000,
    };
    buyer.mpp(recover()).await.unwrap();
    buyer.mpp(recover()).await.unwrap();
    assert!(
        seller
            .mpp(Request::VerifyVoucher {
                funding: funding.clone(),
                voucher
            })
            .await
            .is_err()
    );
    fixture.lock().unwrap().now += 899;
    assert!(buyer.mpp(recover()).await.unwrap().closure.is_none());
    assert_eq!(fixture.lock().unwrap().raws.len(), 2);
    fixture.lock().unwrap().now += 1;
    buyer.mpp(recover()).await.unwrap();
    let result = buyer.mpp(recover()).await.unwrap().closure.unwrap();
    assert_eq!((result.captured, result.refunded), (0, 10));
    assert_eq!(fixture.lock().unwrap().raws.len(), 3);
    server.abort();
}
#[tokio::test]
async fn unresolved_nonce_and_reverted_open_cannot_duplicate_funding() {
    let (fixture, url, server) = server().await;
    let bd = tempfile::tempdir().unwrap();
    let sd = tempfile::tempdir().unwrap();
    let buyer = wallet(bd.path(), &url, 1);
    let seller = wallet(sd.path(), &url, 2);
    let first = terms(&buyer, &seller, 1);
    fixture.lock().unwrap().hide_receipt = true;
    assert!(buyer.mpp(open_request(&first)).await.unwrap().pending);
    assert!(
        buyer
            .mpp(open_request(&terms(&buyer, &seller, 2)))
            .await
            .is_err()
    );
    assert!(buyer.mpp(open_request(&first)).await.unwrap().pending);
    let mut changed = open_request(&first);
    if let Request::Open { deposit, .. } = &mut changed {
        *deposit = 11;
    }
    assert!(buyer.mpp(changed).await.is_err());
    fixture.lock().unwrap().hide_receipt = false;
    fund(&buyer, &first).await;
    fixture.lock().unwrap().revert = true;
    let second = terms(&buyer, &seller, 2);
    buyer.mpp(open_request(&second)).await.unwrap();
    assert!(buyer.mpp(open_request(&second)).await.unwrap().unfunded);
    assert_eq!(fixture.lock().unwrap().raws.len(), 2);
    server.abort();
}

#[tokio::test]
async fn native_close_evidence_must_identify_the_reserve_and_the_channel() {
    let (fixture, url, server) = server().await;
    let bd = tempfile::tempdir().unwrap();
    let sd = tempfile::tempdir().unwrap();
    let buyer = wallet(bd.path(), &url, 1);
    let seller = wallet(sd.path(), &url, 2);
    let funding = fund(&buyer, &terms(&buyer, &seller, 9)).await;
    let voucher = sign(&buyer, &funding, 5).await;
    let closure = close(&seller, &funding, Some(voucher), 5)
        .await
        .closure
        .unwrap();
    let receipt = fixture.lock().unwrap().receipts[&closure.tx_hash].clone();
    assert_eq!(chain::verify_close(&funding, &receipt).unwrap(), closure);
    let mut wrong = receipt.clone();
    wrong["logs"][0]["address"] = json!(OUSD);
    assert!(chain::verify_close(&funding, &wrong).is_err());
    let mut wrong = receipt.clone();
    wrong["logs"][0]["removed"] = json!(true);
    assert!(chain::verify_close(&funding, &wrong).is_err());
    let mut wrong = receipt;
    wrong["logs"][0]["topics"][1] = json!(B256::repeat_byte(7).to_string());
    assert!(chain::verify_close(&funding, &wrong).is_err());
    server.abort();
}

#[tokio::test]
async fn finalized_expiry_releases_unbroadcast_nonce_without_recreating_the_open() {
    let (fixture, url, server) = server().await;
    let bd = tempfile::tempdir().unwrap();
    let sd = tempfile::tempdir().unwrap();
    let buyer = wallet(bd.path(), &url, 1);
    let seller = wallet(sd.path(), &url, 2);
    let terms = terms(&buyer, &seller, 10);
    // Finality is already past the open's lifetime when it is first resumed.
    fixture.lock().unwrap().now = crate::config::now() + 700;
    for _ in 0..2 {
        let response = buyer.mpp(open_request(&terms)).await.unwrap();
        assert!(response.unfunded);
    }
    assert!(fixture.lock().unwrap().raws.is_empty());
    assert!(!buyer.journal.pending().unwrap());
    server.abort();
}

#[tokio::test]
async fn lower_payment_limit_blocks_new_channels_but_preserves_existing_funding_and_recovery() {
    let (fixture, url, server) = server().await;
    let bd = tempfile::tempdir().unwrap();
    let sd = tempfile::tempdir().unwrap();
    let buyer = wallet(bd.path(), &url, 1);
    let seller = wallet(sd.path(), &url, 2);
    let agreed = terms(&buyer, &seller, 11);
    assert!(buyer.mpp(open_request(&agreed)).await.unwrap().pending);
    let attempt_id = attempt_id(&agreed, "open");
    let raw = attempt(&buyer, &attempt_id).tx.raw;
    drop(buyer);
    let mut buyer = wallet(bd.path(), &url, 1);
    buyer.settings.max_payment = 1;
    let funding = buyer
        .mpp(open_request(&agreed))
        .await
        .unwrap()
        .funding
        .unwrap();
    assert_eq!(attempt(&buyer, &attempt_id).tx.raw, raw);
    assert!(
        buyer
            .mpp(open_request(&terms(&buyer, &seller, 12)))
            .await
            .is_err()
    );
    assert!(
        buyer
            .mpp(Request::TopUp {
                funding: funding.clone(),
                amount: 2,
                index: 1,
                max_fee: 1000
            })
            .await
            .is_err()
    );
    let recover = || Request::Recover {
        funding: funding.clone(),
        max_fee: 1000,
    };
    buyer.mpp(recover()).await.unwrap();
    buyer.mpp(recover()).await.unwrap();
    fixture.lock().unwrap().now += 900;
    buyer.mpp(recover()).await.unwrap();
    let closure = buyer.mpp(recover()).await.unwrap().closure.unwrap();
    assert_eq!((closure.captured, closure.refunded), (0, 10));
    assert_eq!(fixture.lock().unwrap().raws.len(), 3);
    server.abort();
}

#[tokio::test]
async fn reverted_close_retries_without_changing_capture_and_keeps_original_bytes() {
    let (fixture, url, server) = server().await;
    let bd = tempfile::tempdir().unwrap();
    let sd = tempfile::tempdir().unwrap();
    let buyer = wallet(bd.path(), &url, 1);
    let seller = wallet(sd.path(), &url, 2);
    let terms = terms(&buyer, &seller, 21);
    let funding = fund(&buyer, &terms).await;
    let voucher = Some(sign(&buyer, &funding, 10).await);
    let close_request = |capture| Request::Close {
        funding: funding.clone(),
        voucher: voucher.clone(),
        capture,
    };
    fixture.lock().unwrap().revert = true;
    seller.mpp(close_request(5)).await.unwrap();
    let original = attempt(&seller, &attempt_id(&terms, "close"));
    assert!(seller.mpp(close_request(6)).await.is_err());
    drop(seller);
    // Each attempt's fee stays within the wallet's per-transaction limit.
    let mut seller = wallet(sd.path(), &url, 2);
    seller.settings.max_fee = original.tx.reserved - 1;
    assert!(seller.mpp(close_request(5)).await.is_err());
    assert_eq!(seller.journal.all::<Attempt>().unwrap().len(), 1);
    seller.settings.max_fee = 10_000;
    fixture.lock().unwrap().revert = false;
    seller.mpp(close_request(5)).await.unwrap();
    let retry = attempt(&seller, &format!("{}/2", attempt_id(&terms, "close")));
    assert_ne!(retry.tx.hash, original.tx.hash);
    assert_eq!(retry.tx.nonce, original.tx.nonce + 1);
    let closure = close(&seller, &funding, voucher.clone(), 5)
        .await
        .closure
        .unwrap();
    assert_eq!((closure.captured, closure.refunded), (5, 5));
    let retained = attempt(&seller, &original.id);
    assert_eq!(retained.tx.status, "reverted");
    assert_eq!(retained.tx.raw, original.tx.raw);
    assert_eq!(fixture.lock().unwrap().raws.len(), 3);
    server.abort();
}

#[tokio::test]
async fn payment_settlement_retry_limit_survives_restart_and_can_be_explicitly_raised() {
    let (fixture, url, server) = server().await;
    let bd = tempfile::tempdir().unwrap();
    let sd = tempfile::tempdir().unwrap();
    let buyer = wallet(bd.path(), &url, 1);
    let seller = wallet(sd.path(), &url, 2);
    let funding = fund(&buyer, &terms(&buyer, &seller, 22)).await;
    let voucher = Some(sign(&buyer, &funding, 5).await);
    let close_request = || Request::Close {
        funding: funding.clone(),
        voucher: voucher.clone(),
        capture: 5,
    };
    fixture.lock().unwrap().revert = true;
    for _ in 0..3 {
        assert!(seller.mpp(close_request()).await.unwrap().pending);
    }
    drop(seller);
    let mut seller = wallet(sd.path(), &url, 2);
    assert!(
        seller
            .mpp(close_request())
            .await
            .unwrap_err()
            .to_string()
            .contains("retry limit reached")
    );
    assert_eq!(seller.journal.all::<Attempt>().unwrap().len(), 3);
    assert_eq!(seller.balance().await.unwrap().reserved, 0);
    fixture.lock().unwrap().revert = false;
    seller.settings.max_settlement_attempts = 4;
    assert_eq!(
        close(&seller, &funding, voucher.clone(), 5)
            .await
            .closure
            .unwrap()
            .captured,
        5
    );
    assert_eq!(fixture.lock().unwrap().raws.len(), 5);
    server.abort();
}

#[tokio::test]
async fn pending_settlement_with_a_missing_receipt_does_not_sign_a_retry() {
    let (fixture, url, server) = server().await;
    let bd = tempfile::tempdir().unwrap();
    let sd = tempfile::tempdir().unwrap();
    let buyer = wallet(bd.path(), &url, 1);
    let seller = wallet(sd.path(), &url, 2);
    let terms = terms(&buyer, &seller, 23);
    let funding = fund(&buyer, &terms).await;
    let voucher = Some(sign(&buyer, &funding, 5).await);
    let close_request = || Request::Close {
        funding: funding.clone(),
        voucher: voucher.clone(),
        capture: 5,
    };
    seller.mpp(close_request()).await.unwrap();
    let original = attempt(&seller, &attempt_id(&terms, "close"));
    {
        let mut state = fixture.lock().unwrap();
        state.hidden_receipts.insert(original.tx.hash.clone());
        state.now = original.tx.expires_at;
    }
    // The channel's finalized close is also hidden, so the exact attempt remains unresolved.
    assert!(seller.mpp(close_request()).await.is_err());
    assert_eq!(seller.journal.all::<Attempt>().unwrap().len(), 1);
    assert_eq!(attempt(&seller, &original.id).tx.raw, original.tx.raw);
    assert!(seller.journal.pending().unwrap());
    assert_eq!(fixture.lock().unwrap().raws.len(), 2);
    server.abort();
}

#[tokio::test]
async fn expired_unbroadcast_close_can_retry_only_after_finalized_expiry() {
    use super::transaction::call;
    let (fixture, url, server) = server().await;
    let bd = tempfile::tempdir().unwrap();
    let sd = tempfile::tempdir().unwrap();
    let buyer = wallet(bd.path(), &url, 1);
    let seller = wallet(sd.path(), &url, 2);
    let terms = terms(&buyer, &seller, 24);
    let funding = fund(&buyer, &terms).await;
    let voucher = sign(&buyer, &funding, 10).await;
    let data = Reserve::closeCall {
        descriptor: chain::descriptor(&funding).unwrap(),
        cumulativeAmount: U96::from(10),
        captureAmount: U96::from(5),
        signature: hex::decode(voucher.signature.trim_start_matches("0x"))
            .unwrap()
            .into(),
    }
    .abi_encode();
    let expiry = crate::config::now() - 1;
    let tx = seller
        .prepare_transaction(
            vec![call(RESERVE.parse().unwrap(), data)],
            expiry,
            0,
            10_000,
            2_000_000,
        )
        .await
        .unwrap();
    let original = Attempt {
        id: attempt_id(&terms, "close"),
        terms: terms.clone(),
        kind: "close".into(),
        amount: 5,
        voucher: Some(voucher.clone()),
        tx,
        funding: Some(funding.clone()),
        deposit: None,
        closure: None,
    };
    seller.journal.save(&original, true).unwrap();
    fixture.lock().unwrap().now = expiry - 1;
    assert!(
        seller
            .mpp(Request::Close {
                funding: funding.clone(),
                voucher: Some(voucher.clone()),
                capture: 5
            })
            .await
            .unwrap()
            .pending
    );
    assert_eq!(seller.journal.all::<Attempt>().unwrap().len(), 1);
    fixture.lock().unwrap().now = expiry;
    let closure = close(&seller, &funding, Some(voucher), 5)
        .await
        .closure
        .unwrap();
    assert_eq!(closure.captured, 5);
    let expired = attempt(&seller, &original.id);
    assert_eq!(expired.tx.status, "expired");
    assert_eq!(expired.tx.raw, original.tx.raw);
    assert_eq!(
        attempt(&seller, &format!("{}/2", original.id)).tx.nonce,
        original.tx.nonce
    );
    server.abort();
}

#[tokio::test]
async fn payer_recovery_retries_both_failed_steps_with_each_fee_bounded() {
    let (fixture, url, server) = server().await;
    let bd = tempfile::tempdir().unwrap();
    let sd = tempfile::tempdir().unwrap();
    let buyer = wallet(bd.path(), &url, 1);
    let seller = wallet(sd.path(), &url, 2);
    let terms = terms(&buyer, &seller, 25);
    let funding = fund(&buyer, &terms).await;
    let recover = || Request::Recover {
        funding: funding.clone(),
        max_fee: 1000,
    };
    fixture.lock().unwrap().revert = true;
    buyer.mpp(recover()).await.unwrap();
    fixture.lock().unwrap().revert = false;
    buyer.mpp(recover()).await.unwrap();
    buyer.mpp(recover()).await.unwrap();
    fixture.lock().unwrap().now += 900;
    fixture.lock().unwrap().revert = true;
    buyer.mpp(recover()).await.unwrap();
    fixture.lock().unwrap().revert = false;
    buyer.mpp(recover()).await.unwrap();
    assert_eq!(
        buyer
            .mpp(recover())
            .await
            .unwrap()
            .closure
            .unwrap()
            .refunded,
        10
    );
    for kind in ["request_close/2", "withdraw/2"] {
        assert!(
            buyer
                .journal
                .get::<Attempt>(&attempt_id(&terms, kind))
                .unwrap()
                .is_some()
        );
    }
    for attempt in buyer.journal.all::<Attempt>().unwrap() {
        let fee = attempt.tx.reserved - if attempt.kind == "open" { 10 } else { 0 };
        assert!(fee <= 1000);
    }
    assert_eq!(fixture.lock().unwrap().raws.len(), 5);
    server.abort();
}

#[tokio::test]
async fn recovery_requests_close_again_after_a_top_up_cancelled_the_request() {
    let (fixture, url, server) = server().await;
    let bd = tempfile::tempdir().unwrap();
    let sd = tempfile::tempdir().unwrap();
    let mut buyer = wallet(bd.path(), &url, 1);
    // Cancelled close requests succeeded, so they use up no retry.
    buyer.settings.max_settlement_attempts = 1;
    let seller = wallet(sd.path(), &url, 2);
    let terms = terms(&buyer, &seller, 26);
    let funding = fund(&buyer, &terms).await;
    let recover = || Request::Recover {
        funding: funding.clone(),
        max_fee: 1000,
    };
    buyer.mpp(recover()).await.unwrap();
    assert!(buyer.mpp(recover()).await.unwrap().close_requested_at > 0);
    for (index, slot) in [(1, "request_close/2"), (2, "request_close/3")] {
        assert_eq!(
            top_up(&buyer, &funding, 10, index).await,
            10 + 10 * u64::from(index)
        );
        buyer.mpp(recover()).await.unwrap();
        assert!(buyer.mpp(recover()).await.unwrap().close_requested_at > 0);
        let again = attempt(&buyer, &attempt_id(&terms, slot));
        assert_eq!(again.tx.status, "finalized");
    }
    assert_eq!(fixture.lock().unwrap().raws.len(), 6);
    server.abort();
}

#[tokio::test]
async fn polling_an_open_channel_reads_only_its_state_without_the_transaction_slot() {
    let (fixture, url, server) = server().await;
    let bd = tempfile::tempdir().unwrap();
    let sd = tempfile::tempdir().unwrap();
    let buyer = wallet(bd.path(), &url, 1);
    let seller = wallet(sd.path(), &url, 2);
    let funding = fund(&buyer, &terms(&buyer, &seller, 27)).await;
    let inspect = || Request::Inspect {
        funding: funding.clone(),
    };
    fixture.lock().unwrap().calls.clear();
    let held = seller.send_lock.lock().await;
    let status = tokio::time::timeout(std::time::Duration::from_secs(5), seller.mpp(inspect()))
        .await
        .unwrap()
        .unwrap();
    drop(held);
    assert_eq!((status.deposit, status.closure), (10, None));
    assert_eq!(fixture.lock().unwrap().calls, ["eth_call"]);
    let recover = || Request::Recover {
        funding: funding.clone(),
        max_fee: 1000,
    };
    buyer.mpp(recover()).await.unwrap();
    buyer.mpp(recover()).await.unwrap();
    fixture.lock().unwrap().calls.clear();
    // A close request makes the poll look for closure evidence.
    assert!(seller.mpp(inspect()).await.unwrap().close_requested_at > 0);
    assert!(
        fixture
            .lock()
            .unwrap()
            .calls
            .contains(&"eth_getLogs".to_string())
    );
    server.abort();
}

#[tokio::test]
async fn sends_are_journaled_once_per_operation_and_replay_their_original_bytes() {
    use super::send::SendAttempt;
    let (fixture, url, server) = server().await;
    let bd = tempfile::tempdir().unwrap();
    let sd = tempfile::tempdir().unwrap();
    let mut buyer = wallet(bd.path(), &url, 1);
    let seller = wallet(sd.path(), &url, 2);
    let to = seller.profile.address.clone();
    let operation = uuid::Uuid::new_v4().to_string();
    let own = buyer.profile.address.clone();
    let refused = buyer.send(&operation, &own, 5, 1000).await.unwrap_err();
    assert!(refused.to_string().contains("this wallet"));
    for contract in [OUSD, RESERVE] {
        let refused = buyer.send(&operation, contract, 5, 1000).await.unwrap_err();
        assert!(refused.to_string().contains("token or reserve"));
    }
    // Each attempt's fee is bounded by the request and by the wallet.
    assert!(buyer.send(&operation, &to, 5, 1).await.is_err());
    buyer.settings.max_fee = 1;
    assert!(buyer.send(&operation, &to, 5, 1000).await.is_err());
    buyer.settings.max_fee = 10_000;
    assert!(buyer.journal.all::<SendAttempt>().unwrap().is_empty());
    fixture.lock().unwrap().hide_receipt = true;
    let pending = buyer.send(&operation, &to, 5, 1000).await.unwrap();
    assert_eq!(pending["status"], "pending");
    assert!(pending.get("raw").is_none());
    let other = uuid::Uuid::new_v4().to_string();
    assert!(buyer.send(&other, &to, 1, 1000).await.is_err());
    assert!(buyer.send(&operation, &to, 6, 1000).await.is_err());
    let id = format!("send/{operation}");
    let original: SendAttempt = buyer.journal.get(&id).unwrap().unwrap();
    drop(buyer);
    let buyer = wallet(bd.path(), &url, 1);
    fixture.lock().unwrap().hide_receipt = false;
    let sent = buyer.send(&operation, &to, 5, 1000).await.unwrap();
    assert_eq!(
        (&sent["status"], &sent["verified"]),
        (&json!("finalized"), &json!(true))
    );
    assert_eq!(sent["hash"], pending["hash"]);
    let restored: SendAttempt = buyer.journal.get(&id).unwrap().unwrap();
    assert_eq!(restored.tx.raw, original.tx.raw);
    assert_eq!(buyer.send(&operation, &to, 5, 1000).await.unwrap(), sent);
    let mut state = fixture.lock().unwrap();
    assert_eq!(state.raws.len(), 1);
    let balance =
        |wallet: &Wallet| state.balances[&wallet.profile.address.parse::<Address>().unwrap()];
    assert_eq!((balance(&buyer), balance(&seller)), (999_995, 1_000_005));
    // The receipt must show exactly this transfer.
    let receipt = state.receipts.get_mut(&original.tx.hash).unwrap();
    super::send::verify_transfer(&own, &restored, receipt).unwrap();
    receipt["logs"][0]["topics"][2] = json!(format!("0x{:0>64}", "ab"));
    assert!(super::send::verify_transfer(&own, &restored, receipt).is_err());
    server.abort();
}

#[tokio::test]
async fn close_events_name_only_this_payees_channels_and_skip_long_gaps() {
    let (fixture, url, server) = server().await;
    let dirs: [_; 3] = std::array::from_fn(|_| tempfile::tempdir().unwrap());
    let buyer = wallet(dirs[0].path(), &url, 1);
    let seller = wallet(dirs[1].path(), &url, 2);
    let other = wallet(dirs[2].path(), &url, 3);
    let funding = fund(&buyer, &terms(&buyer, &seller, 31)).await;
    // The first scan only finds the current block.
    let first = seller.close_events(None).await.unwrap();
    assert!(first.skipped && first.channels.is_empty());
    assert_eq!(first.to, 10);
    let recover = || Request::Recover {
        funding: funding.clone(),
        max_fee: 1000,
    };
    buyer.mpp(recover()).await.unwrap();
    buyer.mpp(recover()).await.unwrap();
    let scan = seller.close_events(Some(0)).await.unwrap();
    assert!(!scan.skipped);
    assert_eq!(
        scan.channels,
        std::collections::BTreeSet::from([funding.channel_id.to_ascii_lowercase()])
    );
    assert!(
        other
            .close_events(Some(0))
            .await
            .unwrap()
            .channels
            .is_empty()
    );
    fixture.lock().unwrap().finalized_height = 20_000;
    let gap = seller.close_events(Some(0)).await.unwrap();
    assert!(gap.skipped && gap.channels.is_empty());
    assert_eq!(gap.to, 20_000);
    server.abort();
}
