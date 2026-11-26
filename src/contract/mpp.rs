//! Reusable TIP-1034 channels for OUSD on the selected Tempo network.
use super::{Asset, SCALE, decimal, validate_address};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const RESERVE: &str = "0x4d50500000000000000000000000000000000000";
pub const CLOSE_GRACE_SECONDS: u64 = 900;

/// The identity of one channel between a buyer and a seller node.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ChannelTerms {
    pub buyer_peer: String,
    pub seller_peer: String,
    pub payer: String,
    pub payee: String,
    pub asset: Asset,
    /// 32 random bytes chosen by the buyer at open, so a pair can reopen after a close.
    pub nonce: String,
}
impl ChannelTerms {
    pub fn validate(&self) -> Result<()> {
        self.asset.network()?;
        validate_address(&self.payer)?;
        validate_address(&self.payee)?;
        ensure!(
            !self.payer.eq_ignore_ascii_case(&self.payee),
            "self channel refused"
        );
        ensure!(
            self.nonce.len() == 64 && hex::decode(&self.nonce).is_ok(),
            "invalid channel nonce"
        );
        Ok(())
    }
    pub fn salt(&self) -> Result<String> {
        self.validate()?;
        let mut digest = Sha256::new();
        digest.update(b"mesh-llm/mpp/channel/v1\0");
        digest.update(serde_json::to_vec(self)?);
        Ok(format!("0x{}", hex::encode(digest.finalize())))
    }
}

/// A finalized open; vouchers are separate.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ChannelFunding {
    pub terms: ChannelTerms,
    pub open_tx: String,
    pub channel_id: String,
    pub expiring_nonce_hash: String,
}

/// A cumulative EIP-712 authorization of `cum` from the channel deposit.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Voucher {
    pub channel_id: String,
    #[serde(with = "decimal")]
    pub cum: u64,
    /// TIP-1020 primitive secp256k1 encoding.
    pub signature: String,
}

/// A seller's quote for one request on the pair's channel.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RequestTerms {
    pub request_id: String,
    pub buyer_peer: String,
    pub seller_peer: String,
    pub model: String,
    pub body_digest: String,
    pub payee: String,
    pub asset: Asset,
    #[serde(with = "decimal")]
    pub input_per_million: u64,
    #[serde(with = "decimal")]
    pub output_per_million: u64,
    /// Percentage taken off reported cached input only.
    pub cache_discount_percent: u8,
    pub output_limit: u64,
    pub input_bytes: u64,
    /// The voucher increase over `cum_before` the seller requires before work.
    /// It is credit netted against later bills; only buyer policy bounds it.
    #[serde(with = "decimal")]
    pub upfront: u64,
    /// The seller's best voucher on the channel before this request.
    #[serde(with = "decimal")]
    pub cum_before: u64,
    /// The sum of the channel's bills when quoted, which may include bills
    /// the buyer has not received or paid yet.
    #[serde(with = "decimal")]
    pub cost_before: u64,
    /// The seller's channel with this buyer, or `None` when a new one is expected.
    pub channel_id: Option<String>,
    pub expires_at: u64,
}

/// Backend-reported token counts; cached tokens are part of prompt tokens.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
}
impl Usage {
    /// A complete usage report, or `None` when it is missing or inconsistent,
    /// so an unknown report never becomes a zero-token bill.
    pub fn from_event(event: &serde_json::Value) -> Option<Self> {
        let usage = event.get("usage")?;
        let prompt_tokens = usage.get("prompt_tokens")?.as_u64()?;
        let completion_tokens = usage.get("completion_tokens")?.as_u64()?;
        let cached_tokens = match usage.get("prompt_tokens_details") {
            None | Some(serde_json::Value::Null) => 0,
            Some(details) => match details.as_object()?.get("cached_tokens") {
                None => 0,
                Some(value) => value.as_u64()?,
            },
        };
        (cached_tokens <= prompt_tokens).then_some(Self {
            prompt_tokens,
            completion_tokens,
            cached_tokens,
        })
    }
}

impl RequestTerms {
    pub fn validate(&self) -> Result<()> {
        self.asset.network()?;
        validate_address(&self.payee)?;
        ensure!(
            !self.request_id.is_empty() && self.request_id.len() <= 128,
            "invalid request ID"
        );
        ensure!(
            self.body_digest.len() == 64 && hex::decode(&self.body_digest)?.len() == 32,
            "invalid body digest"
        );
        ensure!(
            (1..=4096).contains(&self.output_limit),
            "invalid output limit"
        );
        ensure!(
            (1..=64 * 1024).contains(&self.input_bytes),
            "invalid input size"
        );
        ensure!(
            self.cache_discount_percent <= 100,
            "cache discount must be 0..=100 percent"
        );
        if let Some(id) = &self.channel_id {
            ensure!(
                id.len() == 66 && id.starts_with("0x") && hex::decode(&id[2..]).is_ok(),
                "invalid channel ID"
            );
        }
        Ok(())
    }
    /// The largest bill the buyer accepts: every body byte as an uncached
    /// input token plus the full output limit, at the quoted rates.
    pub fn worst_case(&self) -> u64 {
        self.charge(Usage {
            prompt_tokens: self.input_bytes,
            completion_tokens: self.output_limit,
            cached_tokens: 0,
        })
    }
    /// What the seller bills and the buyer pays for `usage`: the charge,
    /// capped at the worst case, so the seller absorbs any prompt longer
    /// than the body bound and an honest bill is never refused.
    pub fn bill(&self, usage: Usage) -> u64 {
        self.charge(usage).min(self.worst_case())
    }
    /// The bill for `usage` at the quoted rates. Output beyond `output_limit`
    /// is not billed, and an unrepresentable bill saturates rather than wraps.
    pub fn charge(&self, usage: Usage) -> u64 {
        // Weight cached and uncached input in percent, then round once.
        let cached = usage.cached_tokens.min(usage.prompt_tokens);
        let input = u128::from(usage.prompt_tokens - cached)
            .saturating_mul(100)
            .saturating_add(
                u128::from(cached)
                    .saturating_mul((100 - self.cache_discount_percent.min(100)).into()),
            );
        let numerator = input
            .saturating_mul(self.input_per_million.into())
            .saturating_add(
                u128::from(usage.completion_tokens.min(self.output_limit))
                    .saturating_mul(self.output_per_million.into())
                    .saturating_mul(100),
            );
        numerator
            .div_ceil(u128::from(SCALE) * 100)
            .min(u64::MAX.into()) as u64
    }
}

/// A seller's bill after inference; `usage` is `None` only for a waived bill.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Bill {
    /// The bill's number on its channel, from 0; the buyer accepts bills in
    /// this order.
    pub seq: u64,
    pub usage: Option<Usage>,
    #[serde(with = "decimal")]
    pub cost: u64,
    /// The channel's bills including this one.
    #[serde(with = "decimal")]
    pub running_total: u64,
}

/// A finalized close or withdrawal; the deposit at close is `captured + refunded`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Closure {
    pub tx_hash: String,
    pub channel_id: String,
    #[serde(with = "decimal")]
    pub captured: u64,
    #[serde(with = "decimal")]
    pub refunded: u64,
    pub block_number: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Open {
        terms: ChannelTerms,
        #[serde(with = "decimal")]
        deposit: u64,
        #[serde(with = "decimal")]
        max_fee: u64,
    },
    /// The `index`th top-up of the channel; retries of one index never add twice.
    TopUp {
        funding: ChannelFunding,
        #[serde(with = "decimal")]
        amount: u64,
        index: u32,
        #[serde(with = "decimal")]
        max_fee: u64,
    },
    /// Off-chain signing; takes no transaction slot.
    SignVoucher {
        funding: ChannelFunding,
        #[serde(with = "decimal")]
        cum: u64,
    },
    VerifyVoucher {
        funding: ChannelFunding,
        voucher: Voucher,
    },
    /// Captures `capture` and refunds the rest; capture 0 needs no valid voucher.
    Close {
        funding: ChannelFunding,
        voucher: Option<Voucher>,
        #[serde(with = "decimal")]
        capture: u64,
    },
    /// Payer recovery starts a native grace period, then withdraws on later calls.
    Recover {
        funding: ChannelFunding,
        #[serde(with = "decimal")]
        max_fee: u64,
    },
    Inspect {
        funding: ChannelFunding,
    },
}
impl Request {
    pub fn terms(&self) -> &ChannelTerms {
        match self {
            Self::Open { terms, .. } => terms,
            Self::TopUp { funding, .. }
            | Self::SignVoucher { funding, .. }
            | Self::VerifyVoucher { funding, .. }
            | Self::Close { funding, .. }
            | Self::Recover { funding, .. }
            | Self::Inspect { funding } => &funding.terms,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Response {
    /// The open was never finalized: it expired unsent or reverted.
    pub unfunded: bool,
    pub funding: Option<ChannelFunding>,
    pub voucher: Option<Voucher>,
    /// The channel deposit after an open, top-up, verification or inspection.
    pub deposit: u64,
    pub closure: Option<Closure>,
    pub pending: bool,
    pub close_requested_at: u64,
}

/// The seller's channels named in close events, from a scan of the reserve's
/// logs up to block `to`.
#[derive(Clone, Debug, Default)]
pub struct CloseEvents {
    /// Lowercase channel IDs of close requests, cancellations and closes.
    pub channels: std::collections::BTreeSet<String>,
    pub to: u64,
    /// No logs were read, at the first scan or after a long gap, so the
    /// caller inspects every channel instead.
    pub skipped: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    fn terms() -> RequestTerms {
        RequestTerms {
            request_id: "request".into(),
            buyer_peer: "buyer".into(),
            seller_peer: "seller".into(),
            model: "model".into(),
            body_digest: "11".repeat(32),
            payee: "0x2222222222222222222222222222222222222222".into(),
            asset: Asset::default(),
            input_per_million: 1_000_000,
            output_per_million: 2_000_000,
            cache_discount_percent: 75,
            output_limit: 100,
            input_bytes: 10,
            upfront: 0,
            cum_before: 0,
            cost_before: 0,
            channel_id: None,
            expires_at: 100,
        }
    }

    #[test]
    fn charge_saturates_and_never_bills_output_beyond_the_limit() {
        let mut terms = terms();
        terms.input_per_million = u64::MAX;
        terms.output_per_million = u64::MAX;
        let huge = Usage {
            prompt_tokens: u64::MAX,
            completion_tokens: u64::MAX,
            cached_tokens: 0,
        };
        assert_eq!(terms.charge(huge), u64::MAX);
        let terms = self::terms();
        let output = |completion_tokens| Usage {
            completion_tokens,
            ..Default::default()
        };
        assert_eq!(terms.charge(output(100)), 200);
        assert_eq!(terms.charge(output(101)), 200);
        assert_eq!(terms.charge(output(u64::MAX)), 200);
    }

    #[test]
    fn salt_commits_to_every_channel_field() {
        let mut channel = ChannelTerms {
            buyer_peer: "aa".repeat(32),
            seller_peer: "bb".repeat(32),
            payer: "0x1111111111111111111111111111111111111111".into(),
            payee: "0x2222222222222222222222222222222222222222".into(),
            asset: Asset::default(),
            nonce: "33".repeat(32),
        };
        let original = channel.salt().unwrap();
        channel.asset = super::super::Network::Mainnet.asset();
        assert_ne!(channel.salt().unwrap(), original);
        channel.asset = Asset::default();
        channel.nonce = "34".repeat(32);
        assert_ne!(channel.salt().unwrap(), original);
        channel.nonce = "zz".repeat(32);
        assert!(channel.validate().is_err());
        channel.nonce = "33".repeat(32);
        channel.payee = channel.payer.clone();
        assert!(channel.validate().is_err());
    }

    #[test]
    fn quotes_may_show_unpaid_bills_and_any_upfront_parses() {
        let mut terms = terms();
        terms.upfront = u64::MAX;
        terms.validate().unwrap();
        let wire = serde_json::to_string(&terms).unwrap();
        assert_eq!(serde_json::from_str::<RequestTerms>(&wire).unwrap(), terms);
        terms.cost_before = 1;
        terms.validate().unwrap();
        terms.cost_before = 0;
        terms.channel_id = Some("0x12".into());
        assert!(terms.validate().is_err());
        terms.channel_id = Some(format!("0x{}", "12".repeat(32)));
        terms.validate().unwrap();
        terms.input_bytes = 0;
        assert!(terms.validate().is_err());
    }

    #[test]
    fn cache_discount_applies_to_cached_input_with_one_rounding() {
        let terms = terms();
        let usage = |prompt, cached, completion| Usage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            cached_tokens: cached,
        };
        // 6 uncached + 4 cached at 25% = 7 input tokens, plus 3 output at 2 each.
        assert_eq!(terms.charge(usage(10, 4, 3)), 13);
        // One cached token at 25% of 1 rounds up to a whole unit.
        assert_eq!(terms.charge(usage(1, 1, 0)), 1);
        assert_eq!(terms.charge(usage(0, 0, 0)), 0);
    }

    #[test]
    fn usage_reads_cached_tokens_and_rejects_inconsistent_reports() {
        let event = |usage| serde_json::json!({ "usage": usage });
        let parsed = Usage::from_event(&event(serde_json::json!({
            "prompt_tokens": 10,
            "completion_tokens": 3,
            "prompt_tokens_details": {"cached_tokens": 4}
        })));
        assert_eq!(
            parsed,
            Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 3,
                cached_tokens: 4
            })
        );
        let uncached = Usage::from_event(&event(serde_json::json!({
            "prompt_tokens": 10,
            "completion_tokens": 3,
            "prompt_tokens_details": null
        })));
        assert_eq!(uncached.map(|u| u.cached_tokens), Some(0));
        assert!(
            Usage::from_event(&event(serde_json::json!({
                "prompt_tokens": 1,
                "completion_tokens": 3,
                "prompt_tokens_details": {"cached_tokens": 2}
            })))
            .is_none()
        );
        assert!(Usage::from_event(&event(serde_json::json!({"prompt_tokens": 1}))).is_none());
    }
}
