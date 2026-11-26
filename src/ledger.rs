use crate::config::now;
use crate::contract::mpp::{Bill, ChannelFunding, ChannelTerms, Closure, RequestTerms, Voucher};
use crate::protocol::Intent;
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    path::Path,
    sync::{Arc, Mutex},
};

#[path = "ledger_settings.rs"]
mod settings;

/// The payment state database, shared with the wallet's transaction journal.
pub type Database = Arc<Mutex<Connection>>;

const VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Negotiating,
    Ready,
    Running,
    UsageUnknown,
    /// The seller billed the request and awaits a voucher covering it.
    Billed,
    Closed,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    pub peer: String,
    pub seller: bool,
    pub intent: Intent,
    pub terms: Option<RequestTerms>,
    /// The key of the channel this request is paid from.
    pub channel: Option<String>,
    pub state: State,
    pub observation: Option<String>,
    /// The seller's terminal usage, or the usage the buyer observed itself.
    pub usage: Option<crate::contract::mpp::Usage>,
    pub bill: Option<Bill>,
    /// When the seller first sent the bill; 0 until then.
    pub billed_at: u64,
    /// The channel's voucher after this request's admission or bill.
    pub cum_after: Option<u64>,
    pub error: Option<String>,
    /// The seller quoted a later request on this pair, or told the buyer that
    /// this one is never billed, while its usage was unknown.
    #[serde(default)]
    pub superseded: bool,
    /// The buyer acknowledged the seller's notice that this request is never
    /// billed.
    pub unbilled_seen: bool,
    /// The seller answered the buyer's hello as busy; the buyer says hello
    /// again within its admission wait.
    pub busy: bool,
}
impl Request {
    pub fn new(id: String, peer: String, seller: bool, intent: Intent) -> Self {
        Self {
            id,
            peer,
            seller,
            intent,
            terms: None,
            channel: None,
            state: State::Negotiating,
            observation: None,
            usage: None,
            bill: None,
            billed_at: 0,
            cum_after: None,
            error: None,
            superseded: false,
            unbilled_seen: false,
            busy: false,
        }
    }
    /// Whether the request takes its peer's slot for its body in its role, so
    /// identical bodies run one at a time. Unknown usage does not, nor does a
    /// buyer's finished request awaiting its bill or the seller's notice that
    /// none follows.
    pub fn holds_peer(&self) -> bool {
        match self.state {
            State::Closed | State::UsageUnknown => false,
            State::Running => self.seller || self.usage.is_none(),
            _ => true,
        }
    }
    /// The `active` column: 0 when closed, 1 while it holds the peer's slot
    /// for its body, and 2 while it stays open without it.
    fn activity(&self) -> u8 {
        match (self.state == State::Closed, self.holds_peer()) {
            (true, _) => 0,
            (false, true) => 1,
            (false, false) => 2,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChannelState {
    Opening,
    Open,
    Closing,
    Closed,
}

/// One buyer-seller channel, reused by requests until it closes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Channel {
    pub key: String,
    pub peer: String,
    pub seller: bool,
    pub terms: ChannelTerms,
    pub funding: Option<ChannelFunding>,
    #[serde(with = "crate::contract::decimal")]
    pub deposit: u64,
    /// Finalized top-ups, so a retried top-up keeps its index.
    pub topups: u32,
    /// The buyer's highest signed voucher, or the seller's best received one.
    pub voucher: Option<Voucher>,
    /// The sum of bills: sent by the seller, acknowledged by the buyer.
    #[serde(with = "crate::contract::decimal")]
    pub running_total: u64,
    /// The number of bills: sent by the seller, accepted in order by the buyer.
    pub bills: u64,
    /// The seller's bills this channel closed without being paid.
    pub lost_bills: u64,
    /// The operator forgave the lost bills, so they no longer count as strikes.
    pub forgiven: bool,
    pub active_at: u64,
    pub state: ChannelState,
    /// The seller's capture, fixed when closing starts.
    pub capture: Option<u64>,
    pub closure: Option<Closure>,
    pub error: Option<String>,
}
impl Channel {
    pub fn new(terms: ChannelTerms, peer: String, seller: bool, deposit: u64) -> Result<Self> {
        Ok(Self {
            key: terms.salt()?,
            peer,
            seller,
            terms,
            funding: None,
            deposit,
            topups: 0,
            voucher: None,
            running_total: 0,
            bills: 0,
            lost_bills: 0,
            forgiven: false,
            active_at: now(),
            state: ChannelState::Opening,
            capture: None,
            closure: None,
            error: None,
        })
    }
    pub fn cum(&self) -> u64 {
        self.voucher.as_ref().map_or(0, |v| v.cum)
    }
}

pub struct Ledger {
    pub(crate) db: Database,
    _lock: File,
}
impl Ledger {
    pub fn open(directory: &Path) -> Result<Self> {
        std::fs::create_dir_all(directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options.open(directory.join("payments.lock"))?;
        lock.try_lock_exclusive()
            .context("another payment engine owns this state directory")?;
        let path = directory.join("payments.sqlite");
        let _private_db = options.open(&path)?;
        let mut db = Connection::open(path)?;
        let version: u32 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
        ensure!(
            version == 0 || version == VERSION,
            "unsupported payment ledger version {version}; use a compatible plugin without resetting state"
        );
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA busy_timeout=5000;",
        )?;
        let tx = db.transaction()?;
        // A request row's `active` is 1 only while it holds its peer's slot
        // for its body in its role; see `Request::activity`.
        tx.execute_batch("CREATE TABLE IF NOT EXISTS mpp_requests(id TEXT PRIMARY KEY,peer TEXT NOT NULL,active INTEGER NOT NULL,body TEXT NOT NULL);
            CREATE UNIQUE INDEX IF NOT EXISTS mpp_one_body ON mpp_requests(peer,json_extract(body,'$.seller'),json_extract(body,'$.intent.body_digest')) WHERE active=1;
            CREATE TABLE IF NOT EXISTS channels(key TEXT PRIMARY KEY,peer TEXT NOT NULL,seller INTEGER NOT NULL,active INTEGER NOT NULL,body TEXT NOT NULL);
            CREATE UNIQUE INDEX IF NOT EXISTS channel_one_peer ON channels(peer,seller) WHERE active=1;
            CREATE TABLE IF NOT EXISTS spend(day INTEGER PRIMARY KEY,amount INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS identity(singleton INTEGER PRIMARY KEY CHECK(singleton=1),node TEXT,wallet_id TEXT,address TEXT,chain_id INTEGER);
            INSERT OR IGNORE INTO identity(singleton) VALUES(1);")?;
        tx.pragma_update(None, "user_version", VERSION)?;
        tx.commit()?;
        Ok(Self {
            db: Arc::new(Mutex::new(db)),
            _lock: lock,
        })
    }
    pub fn pin_node(&self, node: &str) -> Result<()> {
        crate::config::validate_peer(node)?;
        ensure!(
            node == node.to_ascii_lowercase(),
            "Mesh node identity must be canonical lowercase hexadecimal"
        );
        let db = self.db.lock().unwrap();
        let pinned: Option<String> = db.query_row("SELECT node FROM identity", [], |r| r.get(0))?;
        match pinned {
            Some(pinned) => ensure!(
                pinned == node,
                "stable Mesh identity required: node ID changed; restore the pinned node key and configure a persistent Mesh host identity"
            ),
            None => {
                db.execute("UPDATE identity SET node=?1", [node])?;
            }
        }
        Ok(())
    }
    /// Voucher increments the buyer signed today.
    pub fn spent_today(&self) -> Result<u64> {
        spent(&self.db.lock().unwrap(), now() / 86400)
    }

    pub fn mpp_get(&self, id: &str) -> Result<Option<Request>> {
        let db = self.db.lock().unwrap();
        let body: Option<String> = db
            .query_row(
                "SELECT body FROM mpp_requests WHERE id=?1",
                [request_key(id)],
                |r| r.get(0),
            )
            .optional()?;
        Ok(body.map(|body| serde_json::from_str(&body)).transpose()?)
    }
    pub fn mpp_observed(&self, observation: &str) -> Result<Option<Request>> {
        let db = self.db.lock().unwrap();
        let mut query = db.prepare(
            "SELECT body FROM mpp_requests WHERE json_extract(body,'$.observation')=?1 LIMIT 2",
        )?;
        let mut found = None;
        for body in query.query_map([observation], |r| r.get::<_, String>(0))? {
            ensure!(found.is_none(), "ambiguous terminal observation");
            found = Some(serde_json::from_str(&body?)?);
        }
        Ok(found)
    }
    pub fn mpp_active(&self) -> Result<Vec<Request>> {
        let db = self.db.lock().unwrap();
        let mut query =
            db.prepare("SELECT body FROM mpp_requests WHERE active<>0 ORDER BY rowid")?;
        query
            .query_map([], |r| r.get::<_, String>(0))?
            .map(|body| Ok(serde_json::from_str(&body?)?))
            .collect()
    }
    pub fn mpp_history(&self, limit: u32) -> Result<Vec<Request>> {
        ensure!((1..=100).contains(&limit), "history limit must be 1..100");
        let db = self.db.lock().unwrap();
        let mut query = db.prepare("SELECT body FROM mpp_requests ORDER BY rowid DESC LIMIT ?1")?;
        query
            .query_map([limit], |r| r.get::<_, String>(0))?
            .map(|body| Ok(serde_json::from_str(&body?)?))
            .collect()
    }
    pub fn mpp_insert(&self, request: &Request) -> Result<()> {
        let db = self.db.lock().unwrap();
        let key = request_key(&request.id);
        ensure!(
            !db.query_row(
                "SELECT EXISTS(SELECT 1 FROM mpp_requests WHERE id=?1)",
                [&key],
                |r| r.get::<_, bool>(0)
            )?,
            "request identity already exists"
        );
        db.execute(
            "INSERT INTO mpp_requests VALUES(?1,?2,?3,?4)",
            params![
                key,
                request.peer,
                request.activity(),
                serde_json::to_string(request)?
            ],
        )?;
        Ok(())
    }
    /// Applies one state change to a request in a single transaction.
    pub fn mpp_update<T>(
        &self,
        id: &str,
        update: impl FnOnce(&mut Request) -> Result<T>,
    ) -> Result<T> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let result = update_in(&tx, id, update)?;
        tx.commit()?;
        Ok(result)
    }
    /// Applies one state change to a request and its channel in a single
    /// transaction. A buyer voucher increase counts against `daily_budget`.
    pub fn mpp_update_both<T>(
        &self,
        id: &str,
        key: &str,
        daily_budget: u64,
        update: impl FnOnce(&mut Request, &mut Channel) -> Result<T>,
    ) -> Result<T> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let result = channel_update_in(&tx, key, Some(daily_budget), |channel| {
            update_in(&tx, id, |request| update(request, channel))
        })?;
        tx.commit()?;
        Ok(result)
    }
    pub fn mpp_restart(&self) -> Result<()> {
        for request in self.mpp_active()? {
            self.mpp_update(&request.id, |r| {
                // A buyer that already recorded its own usage can still verify its bill.
                if r.state == State::Running && (r.seller || r.usage.is_none()) {
                    r.state = State::UsageUnknown;
                    r.error = Some(if r.seller {"restart interrupted terminal observation; usage is unknown, inference must not be replayed"} else {"restart interrupted the buyer's own usage observation; its bill cannot be verified"}.into());
                }
                if !r.seller && matches!(r.state, State::Negotiating | State::Ready) {r.observation = None;}
                Ok(())
            })?;
        }
        Ok(())
    }

    pub fn channel_insert(&self, channel: &Channel) -> Result<()> {
        insert_channel(&self.db.lock().unwrap(), channel)
    }
    /// Inserts a seller's new channel and applies the request change that
    /// uses it in a single transaction.
    pub fn mpp_insert_channel<T>(
        &self,
        id: &str,
        channel: &Channel,
        update: impl FnOnce(&mut Request, &mut Channel) -> Result<T>,
    ) -> Result<T> {
        ensure!(channel.seller, "buyer channels open before any voucher");
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        insert_channel(&tx, channel)?;
        let result = channel_update_in(&tx, &channel.key, None, |channel| {
            update_in(&tx, id, |request| update(request, channel))
        })?;
        tx.commit()?;
        Ok(result)
    }
    /// Applies a channel's closing change and closes its active requests
    /// with `reason` in a single transaction.
    pub fn channel_close(
        &self,
        key: &str,
        reason: &str,
        update: impl FnOnce(&mut Channel) -> Result<()>,
    ) -> Result<()> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        channel_update_in(&tx, key, None, update)?;
        let ids = {
            let mut query = tx.prepare(
                "SELECT id FROM mpp_requests WHERE active<>0 AND json_extract(body,'$.channel')=?1",
            )?;
            query
                .query_map([key], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for id in ids {
            update_in(&tx, &id, |r| {
                r.state = State::Closed;
                r.error = Some(reason.into());
                Ok(())
            })?;
        }
        tx.commit()?;
        Ok(())
    }
    /// The most recently closed channel with `peer` in one role.
    pub fn channel_last_closed(&self, peer: &str, seller: bool) -> Result<Option<Channel>> {
        let db = self.db.lock().unwrap();
        let body: Option<String> = db
            .query_row(
                "SELECT body FROM channels WHERE peer=?1 AND seller=?2 AND active=0 ORDER BY rowid DESC LIMIT 1",
                params![peer, seller],
                |r| r.get(0),
            )
            .optional()?;
        Ok(body.map(|body| serde_json::from_str(&body)).transpose()?)
    }
    pub fn channel_get(&self, key: &str) -> Result<Option<Channel>> {
        let db = self.db.lock().unwrap();
        let body: Option<String> = db
            .query_row("SELECT body FROM channels WHERE key=?1", [key], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(body.map(|body| serde_json::from_str(&body)).transpose()?)
    }
    /// The open, opening or closing channel with `peer` in one role.
    pub fn channel_active(&self, peer: &str, seller: bool) -> Result<Option<Channel>> {
        let db = self.db.lock().unwrap();
        let body: Option<String> = db
            .query_row(
                "SELECT body FROM channels WHERE peer=?1 AND seller=?2 AND active=1",
                params![peer, seller],
                |r| r.get(0),
            )
            .optional()?;
        Ok(body.map(|body| serde_json::from_str(&body)).transpose()?)
    }
    /// The seller's bills to `peer` that closed channels lost and the
    /// operator has not forgiven.
    pub fn lost_bills(&self, peer: &str) -> Result<u64> {
        let db = self.db.lock().unwrap();
        Ok(db.query_row(
            "SELECT COALESCE(SUM(json_extract(body,'$.lost_bills')),0) FROM channels WHERE peer=?1 AND seller=1 AND active=0 AND json_extract(body,'$.forgiven')=0",
            [peer],
            |r| r.get(0),
        )?)
    }
    /// Forgives the seller's lost bills to `peer`; returns how many.
    pub fn forgive_lost_bills(&self, peer: &str) -> Result<u64> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let keys = {
            let mut query = tx.prepare("SELECT key FROM channels WHERE peer=?1 AND seller=1 AND active=0 AND json_extract(body,'$.lost_bills')>0 AND json_extract(body,'$.forgiven')=0")?;
            query
                .query_map([peer], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut forgiven = 0;
        for key in keys {
            forgiven += channel_update_in(&tx, &key, None, |c| {
                c.forgiven = true;
                Ok(c.lost_bills)
            })?;
        }
        tx.commit()?;
        Ok(forgiven)
    }
    pub fn channels_active(&self) -> Result<Vec<Channel>> {
        let db = self.db.lock().unwrap();
        let mut query = db.prepare("SELECT body FROM channels WHERE active=1 ORDER BY rowid")?;
        query
            .query_map([], |r| r.get::<_, String>(0))?
            .map(|body| Ok(serde_json::from_str(&body?)?))
            .collect()
    }
    /// The most recent channels, newest first.
    pub fn channel_history(&self, limit: u32) -> Result<Vec<Channel>> {
        ensure!((1..=100).contains(&limit), "limit must be 1..100");
        let db = self.db.lock().unwrap();
        let mut query = db.prepare("SELECT body FROM channels ORDER BY rowid DESC LIMIT ?1")?;
        query
            .query_map([limit], |r| r.get::<_, String>(0))?
            .map(|body| Ok(serde_json::from_str(&body?)?))
            .collect()
    }
    /// Applies one state change to a channel in a single transaction; it
    /// cannot raise a buyer voucher, which needs `mpp_update_both`.
    pub fn channel_update<T>(
        &self,
        key: &str,
        update: impl FnOnce(&mut Channel) -> Result<T>,
    ) -> Result<T> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let result = channel_update_in(&tx, key, None, update)?;
        tx.commit()?;
        Ok(result)
    }
}

fn insert_channel(db: &Connection, channel: &Channel) -> Result<()> {
    ensure!(
        channel.voucher.is_none() || channel.seller,
        "buyer vouchers must be budgeted"
    );
    db.execute(
        "INSERT INTO channels VALUES(?1,?2,?3,?4,?5)",
        params![
            channel.key,
            channel.peer,
            channel.seller,
            channel.state != ChannelState::Closed,
            serde_json::to_string(channel)?
        ],
    )?;
    Ok(())
}

fn spent(db: &Connection, day: u64) -> Result<u64> {
    Ok(db
        .query_row("SELECT amount FROM spend WHERE day=?1", [day], |r| {
            r.get::<_, u64>(0)
        })
        .optional()?
        .unwrap_or(0))
}

fn update_in<T>(
    tx: &Transaction,
    id: &str,
    update: impl FnOnce(&mut Request) -> Result<T>,
) -> Result<T> {
    let key = request_key(id);
    let body: String = tx
        .query_row("SELECT body FROM mpp_requests WHERE id=?1", [&key], |r| {
            r.get(0)
        })
        .optional()?
        .context("unknown MPP request")?;
    let mut request: Request = serde_json::from_str(&body)?;
    let result = update(&mut request)?;
    tx.execute(
        "UPDATE mpp_requests SET body=?2,active=?3 WHERE id=?1",
        params![key, serde_json::to_string(&request)?, request.activity()],
    )?;
    Ok(result)
}

fn channel_update_in<T>(
    tx: &Transaction,
    key: &str,
    daily_budget: Option<u64>,
    update: impl FnOnce(&mut Channel) -> Result<T>,
) -> Result<T> {
    let body: String = tx
        .query_row("SELECT body FROM channels WHERE key=?1", [key], |r| {
            r.get(0)
        })
        .optional()?
        .context("unknown payment channel")?;
    let mut channel: Channel = serde_json::from_str(&body)?;
    let before = channel.cum();
    let result = update(&mut channel)?;
    ensure!(channel.key == key, "channel identity changed");
    if !channel.seller && channel.cum() != before {
        let increment = channel
            .cum()
            .checked_sub(before)
            .context("buyer voucher decreased")?;
        let limit = daily_budget.context("buyer voucher increase must be budgeted")?;
        let day = now() / 86400;
        let total = spent(tx, day)?
            .checked_add(increment)
            .context("daily budget overflow")?;
        ensure!(total <= limit, "daily budget exceeded");
        tx.execute(
            "INSERT INTO spend VALUES(?1,?2) ON CONFLICT(day) DO UPDATE SET amount=?2",
            params![day, total],
        )?;
    }
    tx.execute(
        "UPDATE channels SET body=?2,active=?3 WHERE key=?1",
        params![
            key,
            serde_json::to_string(&channel)?,
            channel.state != ChannelState::Closed
        ],
    )?;
    Ok(result)
}

/// Every spelling of a request UUID addresses one record.
fn request_key(id: &str) -> String {
    uuid::Uuid::parse_str(id).map_or_else(|_| id.to_owned(), |id| id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::Asset;

    #[test]
    fn only_one_engine_can_own_and_recover_a_ledger() {
        let temp = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(temp.path()).unwrap();
        assert!(Ledger::open(temp.path()).is_err());
        drop(ledger);
        Ledger::open(temp.path()).unwrap();
    }

    fn channel(peer: &str, nonce: u8) -> Channel {
        let terms = ChannelTerms {
            buyer_peer: "aa".repeat(32),
            seller_peer: peer.into(),
            payer: "0x1111111111111111111111111111111111111111".into(),
            payee: "0x2222222222222222222222222222222222222222".into(),
            asset: Asset::default(),
            nonce: format!("{nonce:02x}").repeat(32),
        };
        Channel::new(terms, peer.into(), false, 100).unwrap()
    }
    fn voucher(cum: u64) -> Option<Voucher> {
        Some(Voucher {
            channel_id: "channel".into(),
            cum,
            signature: "signature".into(),
        })
    }
    /// Raises the buyer voucher of `channel` to `cum` within `limit`.
    fn sign(ledger: &Ledger, id: &str, channel: &Channel, cum: u64, limit: u64) -> Result<()> {
        ledger.mpp_update_both(id, &channel.key, limit, |_, c| {
            c.voucher = voucher(cum);
            Ok(())
        })
    }

    #[test]
    fn budget_counts_voucher_increments_across_restarts_and_future_versions_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(temp.path()).unwrap();
        let open = channel("bb", 1);
        ledger.channel_insert(&open).unwrap();
        ledger.mpp_insert(&request()).unwrap();
        sign(&ledger, "request", &open, 6, 10).unwrap();
        sign(&ledger, "request", &open, 8, 10).unwrap();
        assert_eq!(ledger.spent_today().unwrap(), 8);
        assert!(
            ledger
                .channel_update(&open.key, |c| {
                    c.voucher = voucher(9);
                    Ok(())
                })
                .is_err()
        );
        drop(ledger);
        let ledger = Ledger::open(temp.path()).unwrap();
        assert!(sign(&ledger, "request", &open, 11, 10).is_err());
        assert!(sign(&ledger, "request", &open, 7, 10).is_err());
        assert_eq!(ledger.channel_get(&open.key).unwrap().unwrap().cum(), 8);
        sign(&ledger, "request", &open, 10, 10).unwrap();
        let db = ledger.db.lock().unwrap();
        db.pragma_update(None, "user_version", VERSION + 1).unwrap();
        drop(db);
        drop(ledger);
        assert!(
            Ledger::open(temp.path())
                .err()
                .unwrap()
                .to_string()
                .contains("unsupported payment ledger version")
        );
    }

    #[test]
    fn concurrent_vouchers_cannot_overspend_the_daily_budget() {
        let temp = tempfile::tempdir().unwrap();
        let ledger = std::sync::Arc::new(Ledger::open(temp.path()).unwrap());
        let channels: Vec<_> = (0..2)
            .map(|n| {
                let channel = channel(&format!("peer-{n}"), n + 1);
                ledger.channel_insert(&channel).unwrap();
                let mut row = request();
                row.id = n.to_string();
                row.peer = channel.peer.clone();
                ledger.mpp_insert(&row).unwrap();
                channel
            })
            .collect();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let tasks: Vec<_> = channels
            .into_iter()
            .enumerate()
            .map(|(n, channel)| {
                let ledger = ledger.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    sign(&ledger, &n.to_string(), &channel, 6, 10).is_ok()
                })
            })
            .collect();
        assert_eq!(
            tasks
                .into_iter()
                .filter_map(|t| t.join().unwrap().then_some(()))
                .count(),
            1
        );
    }

    #[test]
    fn one_active_channel_per_peer_and_role_and_closed_history_survives() {
        let temp = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(temp.path()).unwrap();
        let first = channel("bb", 1);
        ledger.channel_insert(&first).unwrap();
        assert!(ledger.channel_insert(&channel("bb", 2)).is_err());
        let mut seller = channel("bb", 3);
        seller.seller = true;
        seller.voucher = voucher(5);
        ledger.channel_insert(&seller).unwrap();
        ledger
            .channel_update(&first.key, |c| {
                c.state = ChannelState::Closed;
                Ok(())
            })
            .unwrap();
        ledger.channel_insert(&channel("bb", 2)).unwrap();
        drop(ledger);
        let ledger = Ledger::open(temp.path()).unwrap();
        assert_eq!(ledger.channels_active().unwrap().len(), 2);
        assert_eq!(
            ledger.channel_get(&first.key).unwrap().unwrap().state,
            ChannelState::Closed
        );
        assert_eq!(ledger.channel_active("bb", true).unwrap().unwrap().cum(), 5);
    }

    #[test]
    fn a_channel_closes_with_its_requests_and_a_seller_channel_opens_with_its_request() {
        let temp = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(temp.path()).unwrap();
        let mut seller = channel("bb", 1);
        seller.seller = true;
        let mut paid = request();
        paid.peer = "bb".into();
        ledger.mpp_insert(&paid).unwrap();
        let refused = ledger.mpp_insert_channel("request", &seller, |_, _| -> Result<()> {
            anyhow::bail!("request consumed concurrently")
        });
        assert!(refused.is_err());
        assert!(ledger.channels_active().unwrap().is_empty());
        ledger
            .mpp_insert_channel("request", &seller, |r, c| {
                r.channel = Some(c.key.clone());
                Ok(())
            })
            .unwrap();
        let failed = ledger.channel_close(&seller.key, "closed", |_| anyhow::bail!("conflict"));
        assert!(failed.is_err());
        assert_eq!(ledger.mpp_active().unwrap().len(), 1);
        ledger
            .channel_close(&seller.key, "closed", |c| {
                c.state = ChannelState::Closed;
                Ok(())
            })
            .unwrap();
        assert!(ledger.mpp_active().unwrap().is_empty());
        assert_eq!(
            ledger.mpp_get("request").unwrap().unwrap().error.as_deref(),
            Some("closed")
        );
        assert_eq!(
            ledger.channel_last_closed("bb", true).unwrap().unwrap().key,
            seller.key
        );
    }

    #[test]
    fn node_identity_is_pinned_across_restarts() {
        let temp = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(temp.path()).unwrap();
        ledger.pin_node(&"aa".repeat(32)).unwrap();
        drop(ledger);
        let ledger = Ledger::open(temp.path()).unwrap();
        ledger.pin_node(&"aa".repeat(32)).unwrap();
        assert!(ledger.pin_node(&"bb".repeat(32)).is_err());
        assert!(ledger.pin_node(&"AA".repeat(32)).is_err());
    }
    fn request() -> Request {
        Request::new(
            "request".into(),
            "peer".into(),
            false,
            crate::protocol::Intent {
                model: "model".into(),
                body_digest: "11".repeat(32),
                payer: "payer".into(),
                output_limit: 4,
                input_bytes: 10,
            },
        )
    }

    #[test]
    fn every_uuid_spelling_addresses_one_agreement() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(dir.path()).unwrap();
        let uuid = uuid::Uuid::from_u128(0xabcdef);
        let mut original = request();
        original.id = uuid.to_string().to_uppercase();
        ledger.mpp_insert(&original).unwrap();
        let mut duplicate = original.clone();
        duplicate.id = uuid.urn().to_string();
        assert!(ledger.mpp_insert(&duplicate).is_err());
        ledger
            .mpp_update(&uuid.simple().to_string(), |row| {
                row.state = State::Closed;
                Ok(())
            })
            .unwrap();
        let stored = ledger.mpp_get(&uuid.braced().to_string()).unwrap().unwrap();
        assert_eq!(stored.state, State::Closed);
    }

    #[test]
    fn request_identity_and_closed_history_survive_restart() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(dir.path()).unwrap();
        ledger.mpp_insert(&request()).unwrap();
        drop(ledger);
        let ledger = Ledger::open(dir.path()).unwrap();
        assert_eq!(ledger.mpp_active().unwrap()[0].intent.input_bytes, 10);
        ledger
            .mpp_update("request", |r| {
                r.state = State::Closed;
                Ok(())
            })
            .unwrap();
        assert!(ledger.mpp_active().unwrap().is_empty());
        assert_eq!(ledger.mpp_history(1).unwrap()[0].state, State::Closed);
        assert!(ledger.mpp_history(0).is_err());
        assert!(ledger.mpp_history(101).is_err());
    }
}
