//! Fabricated reserve state for host IPC smoke tests, never chain evidence.
use super::Fixture;
use crate::contract::mpp::{ChannelFunding, Closure, Request, Response, Voucher};
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, Transaction, params};

struct Channel {
    funding: ChannelFunding,
    deposit: u64,
    topups: u32,
    closure: Option<Closure>,
}
fn load(tx: &Transaction, id: &str) -> Result<Option<Channel>> {
    tx.query_row(
        "SELECT funding,deposit,topups,closure FROM fixture_channels WHERE id=?1",
        [id],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, u64>(1)?,
                r.get::<_, u32>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        },
    )
    .optional()?
    .map(|(funding, deposit, topups, closure)| {
        Ok(Channel {
            funding: serde_json::from_str(&funding)?,
            deposit,
            topups,
            closure: closure.map(|c| serde_json::from_str(&c)).transpose()?,
        })
    })
    .transpose()
}
/// The fabricated channel `funding` names, which must match it exactly.
fn channel(tx: &Transaction, funding: &ChannelFunding) -> Result<Channel> {
    let channel = load(tx, &funding.channel_id)?.context("unknown fixture channel")?;
    ensure!(channel.funding == *funding, "fixture funding mismatch");
    Ok(channel)
}
fn signature(cum: u64) -> String {
    format!("OFFLINE-voucher-{cum}")
}

impl Fixture {
    pub(super) fn execute_request(&self, request: Request) -> Result<Response> {
        let mut db = self.connection.lock().unwrap();
        let tx = db.transaction()?;
        let response = match request {
            Request::Open { terms, deposit, .. } => {
                terms.validate()?;
                ensure!(terms.payer == self.address, "fixture payer mismatch");
                let funding = ChannelFunding {
                    channel_id: terms.salt()?,
                    open_tx: format!("OFFLINE-open-{}", terms.nonce),
                    expiring_nonce_hash: "OFFLINE-context".into(),
                    terms,
                };
                if let Some(old) = load(&tx, &funding.channel_id)? {
                    ensure!(old.funding == funding, "fixture intent changed");
                } else {
                    tx.execute(
                        "INSERT INTO fixture_channels(id,funding,deposit,topups) VALUES(?1,?2,?3,0)",
                        params![funding.channel_id, serde_json::to_string(&funding)?, deposit],
                    )?;
                }
                Response {
                    funding: Some(funding),
                    deposit,
                    ..Default::default()
                }
            }
            Request::TopUp {
                funding,
                amount,
                index,
                ..
            } => top_up(&tx, &funding, amount, index)?,
            Request::SignVoucher { funding, cum } => Response {
                voucher: Some(Voucher {
                    channel_id: funding.channel_id,
                    cum,
                    signature: signature(cum),
                }),
                ..Default::default()
            },
            Request::VerifyVoucher { funding, voucher } => {
                let channel = channel(&tx, &funding)?;
                ensure!(
                    channel.closure.is_none()
                        && voucher.channel_id == funding.channel_id
                        && voucher.signature == signature(voucher.cum)
                        && voucher.cum <= channel.deposit,
                    "fixture voucher not backed"
                );
                Response {
                    deposit: channel.deposit,
                    ..Default::default()
                }
            }
            Request::Inspect { funding } => {
                let channel = channel(&tx, &funding)?;
                Response {
                    deposit: channel.deposit,
                    closure: channel.closure,
                    ..Default::default()
                }
            }
            Request::Close {
                funding,
                voucher,
                capture,
            } => {
                ensure!(
                    funding.terms.payee == self.address
                        && capture <= voucher.as_ref().map_or(0, |v| v.cum),
                    "fixture capture not authorized"
                );
                close(&tx, &funding, capture)?
            }
            Request::Recover { .. } => anyhow::bail!(
                "host smoke fixture does not simulate grace recovery; use native RPC tests"
            ),
        };
        tx.commit()?;
        Ok(response)
    }
}
fn top_up(tx: &Transaction, funding: &ChannelFunding, amount: u64, index: u32) -> Result<Response> {
    let mut channel = channel(tx, funding)?;
    ensure!(channel.closure.is_none(), "fixture channel closed");
    if index == channel.topups + 1 {
        channel.deposit += amount;
        tx.execute(
            "UPDATE fixture_channels SET deposit=?2,topups=?3 WHERE id=?1",
            params![funding.channel_id, channel.deposit, index],
        )?;
    } else {
        ensure!(index <= channel.topups, "fixture top-up skipped an index");
    }
    Ok(Response {
        deposit: channel.deposit,
        ..Default::default()
    })
}
fn close(tx: &Transaction, funding: &ChannelFunding, capture: u64) -> Result<Response> {
    let channel = channel(tx, funding)?;
    let closure = Closure {
        tx_hash: format!("OFFLINE-close-{}", funding.terms.nonce),
        channel_id: funding.channel_id.clone(),
        captured: capture,
        refunded: channel.deposit - capture,
        block_number: 10,
    };
    if let Some(old) = channel.closure {
        ensure!(old == closure, "fixture close changed");
    }
    tx.execute(
        "UPDATE fixture_channels SET closure=?2 WHERE id=?1",
        params![funding.channel_id, serde_json::to_string(&closure)?],
    )?;
    Ok(Response {
        closure: Some(closure),
        ..Default::default()
    })
}
