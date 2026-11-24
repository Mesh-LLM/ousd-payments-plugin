use crate::contract::mpp::{Bill, ChannelFunding, Closure, RequestTerms, Voucher};
use serde::{Deserialize, Serialize};

pub const CHANNEL: &str = "pay.stablecoin.v1";
pub const MPP_VERSION: u16 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Message {
    /// One step of a paid request, keyed by its request UUID.
    Mpp {
        version: u16,
        id: String,
        message: Box<Step>,
    },
    /// The seller's finalized close of the pair's channel.
    Closed {
        version: u16,
        channel_id: String,
        closure: Closure,
    },
    Error {
        id: String,
        reason: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Intent {
    pub model: String,
    pub body_digest: String,
    pub payer: String,
    pub output_limit: u64,
    /// Byte length of the canonical body covered by `body_digest`.
    pub input_bytes: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "snake_case", deny_unknown_fields)]
pub enum Step {
    Hello {
        intent: Intent,
    },
    Quote {
        terms: RequestTerms,
    },
    QuoteRejected {
        reason: String,
    },
    /// The seller has no paid price for the model and serves it free.
    Unpriced,
    /// The seller cannot quote yet: the buyer's channel is still being
    /// opened, the same body is in flight, or the buyer has three unpaid
    /// bills. The buyer says hello again.
    Busy,
    /// The buyer's latest voucher; after a bill it pays the bill.
    Voucher {
        funding: ChannelFunding,
        voucher: Voucher,
    },
    Ready,
    Bill(Bill),
    /// The seller has no usage for the request and never bills it.
    Unbilled,
    /// The buyer received the seller's `Unbilled` notice.
    UnbilledSeen,
    Cancel,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::mpp::Usage;
    use serde_json::json;

    #[test]
    fn bills_and_close_notices_round_trip_with_exact_amounts() {
        let bill = Bill {
            seq: 0,
            usage: Some(Usage {
                prompt_tokens: 1,
                completion_tokens: 4,
                cached_tokens: 0,
            }),
            cost: 5,
            running_total: u64::MAX,
        };
        let message = Message::Mpp {
            version: MPP_VERSION,
            id: "id".into(),
            message: Box::new(Step::Bill(bill.clone())),
        };
        let wire = serde_json::to_value(&message).unwrap();
        assert_eq!(wire["message"]["step"], "bill");
        assert_eq!(wire["message"]["running_total"], u64::MAX.to_string());
        let parsed: Message = serde_json::from_value(wire.clone()).unwrap();
        assert!(
            matches!(parsed, Message::Mpp { message, .. } if matches!(message.as_ref(), Step::Bill(b) if *b == bill))
        );
        let mut extra = wire;
        extra["message"]["capture"] = json!("1");
        assert!(serde_json::from_value::<Message>(extra).is_err());
        let closed = json!({"type":"closed","version":MPP_VERSION,"channel_id":"0x01",
            "closure":{"tx_hash":"0x02","channel_id":"0x01","captured":"5","refunded":"15","block_number":3}});
        let parsed: Message = serde_json::from_value(closed).unwrap();
        assert!(
            matches!(parsed, Message::Closed { closure, .. } if closure.captured + closure.refunded == 20)
        );
    }
}
