//! Live market data over Hyperliquid's WebSocket.
//!
//! Zorro polls `BrokerAsset` for every asset about once a second. Answering
//! each poll with an HTTP request spends rate limit and returns a price that
//! is already a round trip old. Instead one connection streams every mid
//! price, plus the order book of each asset Zorro has asked about, into
//! memory, and polls are answered from there.
//!
//! Books come from `l2Book`, not the lighter `bbo`: on testnet `bbo` pushes
//! nothing at all for PURR/USDC, and it only pushes on a change, so a quiet
//! market would have no book after a reconnect. `l2Book` sends a snapshot
//! on every (re)subscribe, for perps and spot alike.
//!
//! The feed is only trusted while it is visibly alive: if nothing at all
//! has arrived for `STALE_AFTER` the connection is treated as down and
//! callers fall back to HTTP. A dropped connection is never a missing
//! price, only a slower one, and one down for `RESTART_AFTER` is replaced.
//! Any push counts, not just all-mids: on testnet those come only every
//! 5-6 seconds (35 in three minutes), while a liquid book updates many
//! times a second.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use hyperliquid_rust_sdk::{BaseUrl, BookLevel, InfoClient, Message, Subscription};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

use crate::runtime;

/// How long without any push before the feed counts as down. Several
/// all-mids intervals, so a slow one is not mistaken for a dead line: a
/// 10s limit with 30s restarts restarted the feed twice in one Zorro run.
pub const STALE_AFTER: Duration = Duration::from_secs(30);

/// How long a feed may stay down before it is replaced. The SDK reconnects
/// on a clean close, but a connection reset can leave it repeating the same
/// read error forever instead.
const RESTART_AFTER: Duration = Duration::from_secs(90);

/// How long a first request for an asset waits for its book snapshot, so
/// Zorro's first poll already gets a real spread.
const FIRST_BOOK_WAIT: Duration = Duration::from_millis(800);

/// Most lines held for Zorro's log, so a feed stuck repeating an error
/// cannot grow the queue without bound.
const MAX_PENDING_LOG: usize = 20;

/// Best bid and ask of one asset.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Book {
    pub bid: f64,
    pub ask: f64,
}

#[derive(Default)]
struct Data {
    mids: HashMap<String, f64>,
    /// When the last push of any kind arrived; `None` while disconnected.
    last_push: Option<Instant>,
    /// When the connection last dropped. The SDK reconnects by itself
    /// within seconds, so a drop starts the restart clock rather than
    /// counting as already overdue.
    dropped_at: Option<Instant>,
    books: HashMap<String, Book>,
    /// Assets with a book subscription, kept across reconnects because the
    /// SDK resubscribes them itself.
    subscribed: HashSet<String>,
    /// Messages for Zorro's log. Kept here because the feed runs on a
    /// background thread and Zorro's callbacks belong to its own.
    pending_log: Vec<String>,
    /// The last line queued, so a repeated error is logged once.
    last_log: Option<String>,
}

impl Data {
    fn log(&mut self, line: String) {
        if self.last_log.as_deref() == Some(line.as_str())
            || self.pending_log.len() >= MAX_PENDING_LOG
        {
            return;
        }
        self.last_log = Some(line.clone());
        self.pending_log.push(line);
    }
}

struct Feed {
    base_url: BaseUrl,
    started_at: Instant,
    client: tokio::sync::Mutex<InfoClient>,
    sender: UnboundedSender<Message>,
    data: Mutex<Data>,
}

static FEED: Mutex<Option<Arc<Feed>>> = Mutex::new(None);

/// Opens the feed for `base_url`, replacing any earlier one. Failure only
/// means prices come over HTTP, so it is reported, not fatal.
pub fn start(base_url: BaseUrl) -> Result<(), String> {
    stop();
    let (sender, mut receiver) = unbounded_channel();
    let client = runtime::get().block_on(async {
        let mut client = InfoClient::with_reconnect(None, Some(base_url))
            .await
            .map_err(|e| format!("feed connect failed: {e}"))?;
        client
            .subscribe(Subscription::AllMids, sender.clone())
            .await
            .map_err(|e| format!("all-mids subscription failed: {e}"))?;
        Ok::<_, String>(client)
    })?;

    let feed = Arc::new(Feed {
        base_url,
        started_at: Instant::now(),
        client: tokio::sync::Mutex::new(client),
        sender,
        data: Mutex::new(Data::default()),
    });
    // The reader holds only a weak reference: once `stop` drops the feed,
    // the client and its connection go with it and the reader ends.
    let weak: Weak<Feed> = Arc::downgrade(&feed);
    runtime::get().spawn(async move {
        while let Some(message) = receiver.recv().await {
            let Some(feed) = weak.upgrade() else { break };
            let mut data = feed.data.lock().unwrap_or_else(|e| e.into_inner());
            apply(&mut data, message, Instant::now());
        }
    });
    *FEED.lock().unwrap_or_else(|e| e.into_inner()) = Some(feed);
    Ok(())
}

/// Closes the feed. Safe to call when none is open.
pub fn stop() {
    FEED.lock().unwrap_or_else(|e| e.into_inner()).take();
}

fn current() -> Option<Arc<Feed>> {
    FEED.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Replaces a feed that has been down for `RESTART_AFTER`, resubscribing
/// the books it carried. Call from Zorro's thread; cheap when all is well.
pub fn maintain() {
    let Some(feed) = current() else { return };
    let (books, earlier_log) = {
        let mut data = feed.data.lock().unwrap_or_else(|e| e.into_inner());
        if !needs_restart(&data, feed.started_at, Instant::now()) {
            return;
        }
        // Kept so the old feed's last words, such as why it dropped, still
        // reach Zorro's log.
        (data.subscribed.clone(), std::mem::take(&mut data.pending_log))
    };
    for line in &earlier_log {
        crate::state::log_message(line);
    }
    let base_url = feed.base_url;
    drop(feed);
    let line = match start(base_url) {
        Ok(()) => {
            for symbol in &books {
                book(symbol);
            }
            "Hyperliquid: live feed restarted".to_string()
        }
        // `start` stopped the old feed first, so polling takes over until
        // the next login.
        Err(e) => format!("Hyperliquid: live feed restart failed ({e}); prices will be polled"),
    };
    crate::state::log_message(&line);
}

/// Folds one pushed message into the stored data.
fn apply(data: &mut Data, message: Message, now: Instant) {
    match message {
        Message::AllMids(all) => {
            for (coin, mid) in all.data.mids {
                if let Ok(mid) = mid.parse::<f64>() {
                    data.mids.insert(coin, mid);
                }
            }
            data.last_push = Some(now);
        }
        Message::L2Book(l2) => {
            data.last_push = Some(now);
            let top = |side: usize| l2.data.levels.get(side).and_then(|lv| lv.first()).cloned();
            match book_from_levels(&[top(0), top(1)]) {
                Some(book) => {
                    data.books.insert(l2.data.coin, book);
                }
                // One side empty: no spread to report, so drop the stale one.
                None => {
                    data.books.remove(&l2.data.coin);
                }
            }
        }
        // Sent by the SDK when the connection drops. Everything held is
        // now of unknown age; the SDK reconnects and resubscribes, and the
        // next pushes refill it.
        Message::NoData => {
            data.mids.clear();
            data.books.clear();
            data.last_push = None;
            data.dropped_at = Some(now);
            data.log("Hyperliquid: live feed dropped, reconnecting".to_string());
        }
        Message::HyperliquidError(e) => {
            data.log(format!("Hyperliquid: live feed error: {e}"));
        }
        _ => {}
    }
}

/// Bid and ask from the top of a book, `[bid, ask]`. `None` unless both
/// sides are present and the book is not crossed.
fn book_from_levels(levels: &[Option<BookLevel>]) -> Option<Book> {
    let side = |i: usize| -> Option<f64> {
        levels.get(i)?.as_ref()?.px.parse::<f64>().ok().filter(|p| *p > 0.0)
    };
    let (bid, ask) = (side(0)?, side(1)?);
    (ask >= bid).then_some(Book { bid, ask })
}

/// Whether the feed has been silent for `RESTART_AFTER`, counted from its
/// last push, else its last drop, else its start.
fn needs_restart(data: &Data, started_at: Instant, now: Instant) -> bool {
    let since = data.last_push.or(data.dropped_at).unwrap_or(started_at);
    now.duration_since(since) > RESTART_AFTER
}

fn is_live(data: &Data, now: Instant) -> bool {
    matches!(data.last_push, Some(at) if now.duration_since(at) <= STALE_AFTER)
}

/// Lines the feed wants in Zorro's log. Call from Zorro's own thread.
pub fn take_log() -> Vec<String> {
    match current() {
        Some(feed) => {
            std::mem::take(&mut feed.data.lock().unwrap_or_else(|e| e.into_inner()).pending_log)
        }
        None => Vec::new(),
    }
}

/// Mid price from the feed, if it is live and has one for `symbol`.
pub fn mid(symbol: &str) -> Option<f64> {
    let feed = current()?;
    let data = feed.data.lock().unwrap_or_else(|e| e.into_inner());
    if !is_live(&data, Instant::now()) {
        return None;
    }
    data.mids.get(symbol).copied()
}

/// Best bid and ask from the feed. The first request for an asset
/// subscribes to its book and waits briefly for the snapshot; `None` means
/// the feed is down or has nothing yet, and the caller should fall back.
pub fn book(symbol: &str) -> Option<Book> {
    let feed = current()?;
    let newly = {
        let mut data = feed.data.lock().unwrap_or_else(|e| e.into_inner());
        if !is_live(&data, Instant::now()) {
            return None;
        }
        if let Some(book) = data.books.get(symbol) {
            return Some(*book);
        }
        data.subscribed.insert(symbol.to_string())
    };
    if !newly {
        return None; // subscribed already, nothing pushed yet
    }

    let subscribed = runtime::get().block_on(async {
        feed.client
            .lock()
            .await
            .subscribe(Subscription::L2Book { coin: symbol.to_string() }, feed.sender.clone())
            .await
    });
    if let Err(e) = subscribed {
        let mut data = feed.data.lock().unwrap_or_else(|e| e.into_inner());
        data.subscribed.remove(symbol); // let a later poll try again
        data.log(format!("Hyperliquid: no live quotes for {symbol}: {e}"));
        return None;
    }

    let deadline = Instant::now() + FIRST_BOOK_WAIT;
    while Instant::now() < deadline {
        if let Some(book) = feed.data.lock().unwrap_or_else(|e| e.into_inner()).books.get(symbol) {
            return Some(*book);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn level(px: &str) -> Option<BookLevel> {
        Some(BookLevel { px: px.to_string(), sz: "1".to_string(), n: 1 })
    }

    #[test]
    fn a_two_sided_book_gives_bid_and_ask() {
        let book = book_from_levels(&[level("2250.1"), level("2250.4")]).unwrap();
        assert_eq!(book, Book { bid: 2250.1, ask: 2250.4 });
    }

    /// An empty side has no spread to report; zero would read as free.
    #[test]
    fn a_one_sided_book_gives_nothing() {
        assert_eq!(book_from_levels(&[level("2250.1"), None]), None);
        assert_eq!(book_from_levels(&[None, level("2250.4")]), None);
        assert_eq!(book_from_levels(&[]), None);
    }

    #[test]
    fn a_crossed_book_is_not_trusted() {
        assert_eq!(book_from_levels(&[level("2251"), level("2250")]), None);
    }

    #[test]
    fn the_feed_goes_stale_without_mids() {
        let now = Instant::now();
        let mut data = Data::default();
        assert!(!is_live(&data, now), "nothing received yet");
        data.last_push = Some(now);
        assert!(is_live(&data, now + STALE_AFTER));
        assert!(!is_live(&data, now + STALE_AFTER + Duration::from_millis(1)));
    }

    /// A dropped connection must not leave old prices looking current.
    #[test]
    fn a_disconnect_forgets_every_price() {
        let now = Instant::now();
        let mut data = Data::default();
        data.mids.insert("ETH".into(), 2250.0);
        data.books.insert("ETH".into(), Book { bid: 2250.0, ask: 2250.5 });
        data.subscribed.insert("ETH".into());
        data.last_push = Some(now);
        apply(&mut data, Message::NoData, now);
        assert!(data.mids.is_empty() && data.books.is_empty());
        assert!(!is_live(&data, now));
        assert!(data.subscribed.contains("ETH"), "the SDK resubscribes, so keep the record");
        assert_eq!(data.pending_log.len(), 1);
    }

    /// A drop gives the SDK's own reconnect time to work. Clearing the last
    /// push used to make a feed opened long ago look silent since its start,
    /// so every brief drop forced a restart at the next poll.
    #[test]
    fn a_drop_starts_the_restart_clock() {
        let opened = Instant::now();
        let dropped = opened + Duration::from_secs(600);
        let mut data = Data::default();
        data.last_push = Some(dropped - Duration::from_secs(1));
        apply(&mut data, Message::NoData, dropped);
        assert!(!needs_restart(&data, opened, dropped + Duration::from_secs(5)));
        assert!(!needs_restart(&data, opened, dropped + RESTART_AFTER));
        assert!(needs_restart(&data, opened, dropped + RESTART_AFTER + Duration::from_millis(1)));
    }

    #[test]
    fn a_silent_feed_is_restarted_from_its_last_push() {
        let opened = Instant::now();
        let mut data = Data::default();
        assert!(!needs_restart(&data, opened, opened + RESTART_AFTER));
        assert!(needs_restart(&data, opened, opened + RESTART_AFTER + Duration::from_millis(1)));
        data.last_push = Some(opened + Duration::from_secs(300));
        assert!(!needs_restart(&data, opened, opened + Duration::from_secs(300) + RESTART_AFTER));
    }

    /// A book push alone keeps the feed alive: testnet all-mids pushes can
    /// be 6 seconds apart while a liquid book updates continuously.
    #[test]
    fn a_book_push_counts_as_a_sign_of_life() {
        let now = Instant::now();
        let mut data = Data::default();
        let level = |px: &str| BookLevel { px: px.to_string(), sz: "1".to_string(), n: 1 };
        let l2 = hyperliquid_rust_sdk::L2Book {
            data: hyperliquid_rust_sdk::L2BookData {
                coin: "ETH".to_string(),
                time: 0,
                levels: vec![vec![level("2245.2")], vec![level("2245.5")]],
            },
        };
        apply(&mut data, Message::L2Book(l2), now);
        assert!(is_live(&data, now));
        assert_eq!(data.books["ETH"], Book { bid: 2245.2, ask: 2245.5 });
    }

    /// A reset connection repeats its read error many times a second.
    #[test]
    fn a_repeated_error_is_logged_once() {
        let mut data = Data::default();
        for _ in 0..50 {
            apply(&mut data, Message::HyperliquidError("reset".into()), Instant::now());
        }
        assert_eq!(data.pending_log.len(), 1);
    }

    #[test]
    fn the_log_queue_is_capped() {
        let mut data = Data::default();
        for i in 0..100 {
            data.log(format!("line {i}"));
        }
        assert_eq!(data.pending_log.len(), MAX_PENDING_LOG);
    }
}

#[cfg(test)]
mod live {
    /// Diagnostic: how often all-mids pushes arrive over three minutes.
    #[test]
    #[ignore]
    fn times_the_mids_pushes() {
        super::start(hyperliquid_rust_sdk::BaseUrl::Testnet).expect("feed");
        let feed = super::current().unwrap();
        let mut last = None;
        let mut max_gap = std::time::Duration::ZERO;
        let mut pushes = 0;
        let end = std::time::Instant::now() + std::time::Duration::from_secs(180);
        while std::time::Instant::now() < end {
            let at = feed.data.lock().unwrap().last_push;
            if at != last {
                if let (Some(a), Some(b)) = (last, at) {
                    let gap = b.duration_since(a);
                    if gap > max_gap {
                        max_gap = gap;
                        println!("new max gap {gap:?} after {pushes} pushes");
                    }
                }
                pushes += 1;
                last = at;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        println!("{pushes} pushes, max gap {max_gap:?}, log {:?}", feed.data.lock().unwrap().pending_log);
    }

    /// Diagnostic: watches the feed, with ETH's book subscribed as Zorro
    /// would, for `HL_WATCH_MINUTES` (default 20) and prints every silence
    /// longer than ten seconds, to see how often testnet goes quiet.
    #[test]
    #[ignore]
    fn watches_for_silent_gaps() {
        let minutes: u64 = std::env::var("HL_WATCH_MINUTES").ok().and_then(|m| m.parse().ok()).unwrap_or(20);
        super::start(hyperliquid_rust_sdk::BaseUrl::Testnet).expect("feed");
        super::book("ETH");
        let feed = super::current().unwrap();
        let begun = std::time::Instant::now();
        let mut last = None;
        let mut pushes = 0u64;
        let mut gaps = 0;
        while begun.elapsed() < std::time::Duration::from_secs(minutes * 60) {
            let at = feed.data.lock().unwrap().last_push;
            if at != last {
                if let (Some(a), Some(b)) = (last, at) {
                    let gap = b.duration_since(a);
                    if gap > std::time::Duration::from_secs(10) {
                        gaps += 1;
                        println!("at {:?}: silent for {gap:?}", a.duration_since(begun));
                    }
                }
                pushes += 1;
                last = at;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        println!("{pushes} pushes in {minutes} min, {gaps} silences over 10s, last push {:?} ago, log {:?}",
            last.map(|a: std::time::Instant| a.elapsed()), feed.data.lock().unwrap().pending_log);
    }
}
