use super::rpc::{Rpc, address, quantity};
use crate::contract::{
    Network, OUSD,
    mpp::{ChannelFunding, ChannelTerms, Closure, RESERVE, Voucher},
};
use alloy::{
    primitives::{Address, B256, U256, aliases::U96, keccak256},
    sol_types::{SolCall, SolEvent, SolValue},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use tempo_alloy::contracts::precompiles::{ISignatureVerifier, ITIP20ChannelReserve as Reserve};

pub fn descriptor(funding: &ChannelFunding) -> Result<Reserve::ChannelDescriptor> {
    funding.terms.validate()?;
    Ok(Reserve::ChannelDescriptor {
        payer: address(&funding.terms.payer)?,
        payee: address(&funding.terms.payee)?,
        operator: Address::ZERO,
        token: address(OUSD)?,
        salt: funding.terms.salt()?.parse()?,
        authorizedSigner: Address::ZERO,
        expiringNonceHash: funding.expiring_nonce_hash.parse()?,
    })
}
pub fn channel_id(network: Network, descriptor: &Reserve::ChannelDescriptor) -> B256 {
    keccak256(
        (
            descriptor.payer,
            descriptor.payee,
            descriptor.operator,
            descriptor.token,
            descriptor.salt,
            descriptor.authorizedSigner,
            descriptor.expiringNonceHash,
            RESERVE.parse::<Address>().unwrap(),
            U256::from(network.chain_id()),
        )
            .abi_encode(),
    )
}
pub fn domain(network: Network) -> B256 {
    keccak256((keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"),
        keccak256("TIP20 Channel Reserve"), keccak256("1"), U256::from(network.chain_id()), RESERVE.parse::<Address>().unwrap()).abi_encode())
}
pub fn voucher_digest(network: Network, id: B256, cum: u64) -> B256 {
    let hash = keccak256(
        (
            keccak256("Voucher(bytes32 channelId,uint96 cumulativeAmount)"),
            id,
            U96::from(cum),
        )
            .abi_encode(),
    );
    keccak256(
        [
            b"\x19\x01".as_slice(),
            domain(network).as_slice(),
            hash.as_slice(),
        ]
        .concat(),
    )
}
pub fn open_data(terms: &ChannelTerms, deposit: u64) -> Result<Vec<u8>> {
    Ok(Reserve::openCall {
        payee: address(&terms.payee)?,
        operator: Address::ZERO,
        token: address(OUSD)?,
        deposit: U96::from(deposit),
        salt: terms.salt()?.parse()?,
        authorizedSigner: Address::ZERO,
    }
    .abi_encode())
}
pub async fn call<C: SolCall>(rpc: &Rpc, to: &str, call: C, tag: &str) -> Result<C::Return> {
    let value = rpc
        .call(
            "eth_call",
            json!([{"to":to,"data":format!("0x{}",hex::encode(call.abi_encode()))},tag]),
        )
        .await?;
    Ok(C::abi_decode_returns_validate(&hex::decode(
        value
            .as_str()
            .context("invalid ABI response")?
            .trim_start_matches("0x"),
    )?)?)
}
pub async fn check_abi(rpc: &Rpc) -> Result<()> {
    rpc.check_network().await?;
    ensure!(
        call(rpc, RESERVE, Reserve::domainSeparatorCall {}, "finalized").await?
            == domain(rpc.network),
        "unsupported reserve domain"
    );
    ensure!(
        call(
            rpc,
            RESERVE,
            Reserve::CLOSE_GRACE_PERIODCall {},
            "finalized"
        )
        .await?
            == crate::contract::mpp::CLOSE_GRACE_SECONDS,
        "unsupported close grace period"
    );
    Ok(())
}
pub async fn state(
    rpc: &Rpc,
    funding: &ChannelFunding,
    tag: &str,
) -> Result<Reserve::ChannelState> {
    ensure!(
        funding.terms.asset.network()? == rpc.network,
        "channel belongs to another Tempo network"
    );
    call(
        rpc,
        RESERVE,
        Reserve::getChannelStateCall {
            channelId: funding.channel_id.parse()?,
        },
        tag,
    )
    .await
}
pub async fn finalized_receipt(rpc: &Rpc, hash: &str) -> Result<Option<Value>> {
    let finalized = rpc.finalized().await?;
    finalized_receipt_at(rpc, hash, &finalized).await
}

pub(super) async fn finalized_receipt_at(
    rpc: &Rpc,
    hash: &str,
    finalized: &Value,
) -> Result<Option<Value>> {
    let _: B256 = hash.parse()?;
    let receipt = rpc.call("eth_getTransactionReceipt", json!([hash])).await?;
    if receipt.is_null() {
        return Ok(None);
    }
    ensure!(
        receipt["transactionHash"]
            .as_str()
            .is_some_and(|v| v.eq_ignore_ascii_case(hash)),
        "wrong transaction receipt"
    );
    let height = quantity(&receipt["blockNumber"])?;
    let block = rpc
        .call(
            "eth_getBlockByNumber",
            json!([format!("0x{height:x}"), false]),
        )
        .await?;
    if block["hash"] != receipt["blockHash"] || quantity(&finalized["number"])? < height {
        return Ok(None);
    }
    let _: B256 = receipt["blockHash"]
        .as_str()
        .context("missing block hash")?
        .parse()?;
    Ok(Some(receipt))
}
pub fn event<E: SolEvent>(receipt: &Value) -> Result<E> {
    let mut found = None;
    for log in receipt["logs"].as_array().context("missing receipt logs")? {
        if !log["address"]
            .as_str()
            .is_some_and(|v| v.eq_ignore_ascii_case(RESERVE))
            || log["removed"] == true
        {
            continue;
        }
        let topics: Vec<B256> = log["topics"]
            .as_array()
            .context("missing topics")?
            .iter()
            .map(|v| Ok(v.as_str().context("invalid topic")?.parse()?))
            .collect::<Result<_>>()?;
        if topics.first() != Some(&E::SIGNATURE_HASH) {
            continue;
        }
        ensure!(
            log["transactionHash"] == receipt["transactionHash"]
                && log["blockHash"] == receipt["blockHash"],
            "log identity mismatch"
        );
        let bytes = hex::decode(
            log["data"]
                .as_str()
                .context("missing log data")?
                .trim_start_matches("0x"),
        )?;
        ensure!(found.is_none(), "ambiguous reserve events");
        found = Some(E::decode_raw_log_validate(topics, &bytes)?);
    }
    found.context("missing native reserve event")
}
/// Verifies the opening transaction and returns its initial deposit.
pub fn verify_open(funding: &ChannelFunding, receipt: &Value) -> Result<u64> {
    let d = descriptor(funding)?;
    let network = funding.terms.asset.network()?;
    ensure!(
        channel_id(network, &d) == funding.channel_id.parse::<B256>()?,
        "channel identity mismatch"
    );
    ensure!(quantity(&receipt["status"])? == 1, "open reverted");
    ensure!(
        receipt["from"]
            .as_str()
            .is_some_and(|v| v.eq_ignore_ascii_case(&funding.terms.payer)),
        "wrong funding sender"
    );
    let e = event::<Reserve::ChannelOpened>(receipt)?;
    ensure!(
        e.channelId == channel_id(network, &d)
            && e.payer == d.payer
            && e.payee == d.payee
            && e.operator == d.operator
            && e.token == d.token
            && e.salt == d.salt
            && e.authorizedSigner == d.authorizedSigner
            && e.expiringNonceHash == d.expiringNonceHash,
        "opening event differs from the agreed channel"
    );
    Ok(u64::try_from(e.deposit)?)
}
/// Verifies a top-up of `amount` and returns the resulting deposit.
pub fn verify_top_up(funding: &ChannelFunding, receipt: &Value, amount: u64) -> Result<u64> {
    ensure!(quantity(&receipt["status"])? == 1, "top-up reverted");
    let e = event::<Reserve::TopUp>(receipt)?;
    ensure!(
        e.channelId == funding.channel_id.parse::<B256>()?
            && e.payer == address(&funding.terms.payer)?
            && e.additionalDeposit == U96::from(amount),
        "top-up event differs from the requested amount"
    );
    Ok(u64::try_from(e.newDeposit)?)
}
pub fn verify_close_requested(funding: &ChannelFunding, receipt: &Value) -> Result<()> {
    ensure!(quantity(&receipt["status"])? == 1, "close request reverted");
    let e = event::<Reserve::CloseRequested>(receipt)?;
    ensure!(
        e.channelId == funding.channel_id.parse::<B256>()?
            && e.payer == address(&funding.terms.payer)?,
        "close request event differs from the channel"
    );
    Ok(())
}
/// Checks that `voucher` is the payer's signature for this channel and network.
pub async fn verify_signature(
    rpc: &Rpc,
    funding: &ChannelFunding,
    voucher: &Voucher,
) -> Result<()> {
    ensure!(
        voucher.channel_id == funding.channel_id,
        "voucher belongs to another channel"
    );
    let digest = voucher_digest(rpc.network, funding.channel_id.parse()?, voucher.cum);
    ensure!(
        call(
            rpc,
            RESERVE,
            Reserve::getVoucherDigestCall {
                channelId: funding.channel_id.parse()?,
                cumulativeAmount: U96::from(voucher.cum)
            },
            "finalized"
        )
        .await?
            == digest,
        "voucher domain mismatch"
    );
    let signature = hex::decode(voucher.signature.trim_start_matches("0x"))?;
    ensure!(
        signature.len() == 65,
        "only primitive secp256k1 vouchers are supported"
    );
    ensure!(
        call(
            rpc,
            "0x5165300000000000000000000000000000000000",
            ISignatureVerifier::verifyCall {
                signer: address(&funding.terms.payer)?,
                hash: digest,
                signature: signature.into()
            },
            "latest"
        )
        .await?,
        "invalid voucher signature"
    );
    Ok(())
}
/// Verifies a voucher against a finalized, open, unsettled channel that backs
/// it at finalized and latest state, and returns the finalized deposit.
pub async fn verify_voucher(rpc: &Rpc, funding: &ChannelFunding, voucher: &Voucher) -> Result<u64> {
    ensure!(
        funding.terms.asset.network()? == rpc.network,
        "funding belongs to another Tempo network"
    );
    check_abi(rpc).await?;
    let receipt = finalized_receipt(rpc, &funding.open_tx)
        .await?
        .context("funding is not finalized")?;
    verify_open(funding, &receipt)?;
    verify_signature(rpc, funding, voucher).await?;
    let mut deposit = 0;
    for tag in ["finalized", "latest"] {
        let s = state(rpc, funding, tag).await?;
        ensure!(
            s.deposit >= U96::from(voucher.cum)
                && s.settled == U96::ZERO
                && s.closeRequestedAt == 0,
            "channel does not back the voucher, is settled or is closing"
        );
        if tag == "finalized" {
            deposit = u64::try_from(s.deposit)?;
        }
    }
    Ok(deposit)
}
pub fn verify_close(funding: &ChannelFunding, receipt: &Value) -> Result<Closure> {
    ensure!(quantity(&receipt["status"])? == 1, "close reverted");
    let e = event::<Reserve::ChannelClosed>(receipt)?;
    ensure!(
        e.channelId == funding.channel_id.parse::<B256>()?
            && e.payer == address(&funding.terms.payer)?
            && e.payee == address(&funding.terms.payee)?,
        "close event differs from the channel"
    );
    Ok(Closure {
        tx_hash: receipt["transactionHash"]
            .as_str()
            .context("missing close hash")?
            .into(),
        channel_id: funding.channel_id.clone(),
        captured: u64::try_from(e.settledToPayee)?,
        refunded: u64::try_from(e.refundedToPayer)?,
        block_number: quantity(&receipt["blockNumber"])?,
    })
}
/// Lowercase IDs of `payee`'s channels named in close requests, cancelled
/// close requests and closes from block `from` through `to`.
pub async fn close_event_channels(
    rpc: &Rpc,
    payee: &str,
    from: u64,
    to: u64,
) -> Result<std::collections::BTreeSet<String>> {
    let kinds = [
        Reserve::CloseRequested::SIGNATURE_HASH,
        Reserve::CloseRequestCancelled::SIGNATURE_HASH,
        Reserve::ChannelClosed::SIGNATURE_HASH,
    ];
    let payee = B256::left_padding_from(address(payee)?.as_slice());
    let logs = rpc
        .call(
            "eth_getLogs",
            json!([{"address":RESERVE,"fromBlock":format!("0x{from:x}"),"toBlock":format!("0x{to:x}"),
        "topics":[kinds.map(|k| k.to_string()),null,null,payee.to_string()]}]),
        )
        .await?;
    let mut channels = std::collections::BTreeSet::new();
    for log in logs.as_array().context("invalid close logs")? {
        let topics: Vec<B256> = serde_json::from_value(log["topics"].clone())?;
        ensure!(
            log["address"]
                .as_str()
                .is_some_and(|a| a.eq_ignore_ascii_case(RESERVE))
                && topics.len() == 4
                && kinds.contains(&topics[0])
                && topics[3] == payee,
            "close log does not match the query"
        );
        channels.insert(topics[1].to_string());
    }
    Ok(channels)
}
pub async fn find_close(rpc: &Rpc, funding: &ChannelFunding) -> Result<Option<Closure>> {
    ensure!(
        funding.terms.asset.network()? == rpc.network,
        "funding belongs to another Tempo network"
    );
    let open = finalized_receipt(rpc, &funding.open_tx)
        .await?
        .context("opening receipt unavailable")?;
    verify_open(funding, &open)?;
    let logs = rpc
        .call(
            "eth_getLogs",
            json!([{"address":RESERVE,"fromBlock":open["blockNumber"],"toBlock":"finalized",
        "topics":[Reserve::ChannelClosed::SIGNATURE_HASH.to_string(),funding.channel_id]}]),
        )
        .await?;
    for log in logs.as_array().context("invalid close logs")? {
        let hash = log["transactionHash"]
            .as_str()
            .context("missing close transaction")?;
        if let Some(receipt) = finalized_receipt(rpc, hash).await? {
            return Ok(Some(verify_close(funding, &receipt)?));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn eip712_matches_independent_ethers_vectors_and_deployed_domains() {
        // ethers 6.17 TypedDataEncoder, canonical reserve, channel 0x11..11, cumulative 10.
        for (network, separator, digest) in [
            (
                Network::Testnet,
                "0xa3aa475070603a80823360e5a4a0d3b4fe2cd4e82a109ee3c21a4640a199688d",
                "0x163e39f492a9786e450377a6b3646b3da2b187eb5394a2e2c732158a5003c959",
            ),
            (
                Network::Mainnet,
                "0x6465ffa0b0c1e7cfc1a894ba54d79615bc06fd7cc9cfec558f3df82189d17367",
                "0x6577ae01dea92b9b5de11cd83e8ea88b050b1f6d407e1da0fba71cd6622da109",
            ),
        ] {
            assert_eq!(domain(network).to_string(), separator);
            assert_eq!(
                voucher_digest(network, B256::repeat_byte(0x11), 10).to_string(),
                digest
            );
        }
    }
}
