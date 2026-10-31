//! HTTP access plus an on-disk cache for slow-changing game data.
//!
//! Every request is rate limited per host and retried with exponential backoff on rate limiting
//! (429), server errors and network failures. Anything that isn't market data or the user's own
//! Teamcraft list is cached on disk for a week.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use reqwest::StatusCode;
use reqwest::blocking::Client;
use reqwest::header::RETRY_AFTER;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Game data only changes with patches, so a week is a safe default.
const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Retries after the first attempt; with doubling waits from `FIRST_BACKOFF` that's ~15s total.
const MAX_RETRIES: u32 = 5;
const FIRST_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

type Logger = Box<dyn Fn(&str) + Send + Sync>;

pub struct Http {
    pub client: Client,
    cache_dir: PathBuf,
    refresh: bool,
    log: Logger,
}

impl Http {
    /// `log` receives progress messages; the CLI prints them to stderr, the TUI shows them.
    pub fn new(refresh: bool, log: impl Fn(&str) + Send + Sync + 'static) -> Result<Self> {
        let client = Client::builder()
            .user_agent(concat!("cheapster/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(120))
            .build()?;
        let cache_dir = cache_dir();
        fs::create_dir_all(&cache_dir).with_context(|| format!("creating cache dir {}", cache_dir.display()))?;
        Ok(Self { client, cache_dir, refresh, log: Box::new(log) })
    }

    pub fn log(&self, msg: &str) {
        (self.log)(msg);
    }

    /// Fetches and parses JSON, uncached. Use for live data (market prices, Teamcraft lists).
    pub fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T> {
        let body = self.get_text(url)?;
        serde_json::from_str(&body).with_context(|| format!("parsing response from {url}"))
    }

    /// Like `get_json`, but cached on disk (keyed by URL) for game data that rarely changes.
    pub fn get_json_cached<T: DeserializeOwned>(&self, url: &str) -> Result<T> {
        let value: serde_json::Value = self.cached_for_url(url, || self.get_json::<serde_json::Value>(url))?;
        serde_json::from_value(value).with_context(|| format!("parsing cached response from {url}"))
    }

    /// `cached`, keyed by a URL (for results built from one or more requests starting there).
    pub fn cached_for_url<T, F>(&self, url: &str, fetch: F) -> Result<T>
    where
        T: Serialize + DeserializeOwned,
        F: FnOnce() -> Result<T>,
    {
        self.cached(&format!("http/{}", cache_key(url)), fetch)
    }

    /// GET with per-host rate limiting, retrying transient failures with exponential backoff.
    fn get_text(&self, url: &str) -> Result<String> {
        let host = host_of(url);
        let mut backoff = FIRST_BACKOFF;
        for attempt in 0..=MAX_RETRIES {
            throttle(&host);
            let last = attempt == MAX_RETRIES;
            let (problem, retry_after) = match self.client.get(url).send() {
                Ok(resp) if resp.status().is_success() => match resp.text() {
                    Ok(body) => return Ok(body),
                    Err(e) if !last => (format!("reading the response failed ({e})"), None),
                    Err(e) => return Err(e).with_context(|| format!("reading response from {url}")),
                },
                Ok(resp) => {
                    let status = resp.status();
                    let retry_after = resp
                        .headers()
                        .get(RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.trim().parse::<u64>().ok())
                        .map(Duration::from_secs);
                    if last || !retryable(status) {
                        let body = resp.text().unwrap_or_default();
                        bail!("GET {url} returned {status}: {}", truncate(&body, 300));
                    }
                    (format!("returned {status}"), retry_after)
                }
                Err(e) if !last && (e.is_timeout() || e.is_connect() || e.is_request()) => {
                    (format!("failed ({e})"), None)
                }
                Err(e) => return Err(e).with_context(|| format!("GET {url}")),
            };
            let wait = retry_after.unwrap_or(backoff).min(MAX_BACKOFF);
            self.log(&format!("{host} {problem}; retrying in {:.1}s...", wait.as_secs_f32()));
            std::thread::sleep(wait);
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
        unreachable!("the last attempt always returns")
    }

    /// Returns the cached value stored under `name`, or builds it with `fetch` and caches it.
    pub fn cached<T, F>(&self, name: &str, fetch: F) -> Result<T>
    where
        T: Serialize + DeserializeOwned,
        F: FnOnce() -> Result<T>,
    {
        let path = self.cache_dir.join(format!("{name}.json"));
        if !self.refresh
            && is_fresh(&path)
            && let Ok(text) = fs::read_to_string(&path)
            && let Ok(value) = serde_json::from_str(&text)
        {
            return Ok(value);
        }
        let value = fetch()?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        fs::write(&path, serde_json::to_string(&value)?)
            .with_context(|| format!("writing cache file {}", path.display()))?;
        Ok(value)
    }
}

fn retryable(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::REQUEST_TIMEOUT || status.is_server_error()
}

/// Minimum time between requests to one host, to stay under its rate limit.
fn min_interval(host: &str) -> Duration {
    match host {
        // Universalis allows 25 requests/second.
        "universalis.app" => Duration::from_millis(60),
        // XIVAPI asks clients to be gentle; recipe expansion can fire many searches in a row.
        "v2.xivapi.com" => Duration::from_millis(100),
        _ => Duration::from_millis(50),
    }
}

/// When each host may next be called. Shared by every `Http` (the TUI runs several threads).
static NEXT_SLOT: Mutex<Option<HashMap<String, Instant>>> = Mutex::new(None);

/// Waits until `host` may be called again, then reserves the next slot.
fn throttle(host: &str) {
    let wait = {
        let mut guard = NEXT_SLOT.lock().unwrap_or_else(|e| e.into_inner());
        let slots = guard.get_or_insert_with(HashMap::new);
        let now = Instant::now();
        let start = slots.get(host).copied().filter(|t| *t > now).unwrap_or(now);
        slots.insert(host.to_string(), start + min_interval(host));
        start - now
    };
    if !wait.is_zero() {
        std::thread::sleep(wait);
    }
}

fn host_of(url: &str) -> String {
    reqwest::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default()
}

/// A stable file-name-safe key for a URL (FNV-1a; std's hasher isn't stable across releases).
fn cache_key(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn is_fresh(path: &Path) -> bool {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age < MAX_AGE)
}

pub fn cache_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("CHEAPSTER_CACHE_DIR") {
        return dir.into();
    }
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("XDG_CACHE_HOME"))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("cheapster")
}

fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_keys_are_stable_and_distinct() {
        assert_eq!(
            cache_key("https://v2.xivapi.com/api/sheet/Item?rows=1"),
            cache_key("https://v2.xivapi.com/api/sheet/Item?rows=1")
        );
        assert_ne!(cache_key("a"), cache_key("b"));
        // Pinned so cache files survive compiler upgrades.
        assert_eq!(cache_key(""), "cbf29ce484222325");
    }

    #[test]
    fn throttle_spaces_out_requests_to_the_same_host() {
        let host = "throttle-test.invalid";
        let start = Instant::now();
        for _ in 0..4 {
            throttle(host);
        }
        // Four calls need three gaps of the default 50ms.
        assert!(start.elapsed() >= Duration::from_millis(150), "{:?}", start.elapsed());
    }

    #[test]
    fn retries_only_transient_statuses() {
        assert!(retryable(StatusCode::TOO_MANY_REQUESTS));
        assert!(retryable(StatusCode::BAD_GATEWAY));
        assert!(!retryable(StatusCode::NOT_FOUND));
        assert!(!retryable(StatusCode::FORBIDDEN));
    }
}
