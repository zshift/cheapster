//! HTTP access plus an on-disk cache for slow-changing game data.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use reqwest::blocking::Client;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Game data only changes with patches, so a week is a safe default.
const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

pub struct Http {
    pub client: Client,
    cache_dir: PathBuf,
    refresh: bool,
}

impl Http {
    pub fn new(refresh: bool) -> Result<Self> {
        let client = Client::builder()
            .user_agent(concat!("cheapster/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(120))
            .build()?;
        let cache_dir = cache_dir();
        fs::create_dir_all(&cache_dir).with_context(|| format!("creating cache dir {}", cache_dir.display()))?;
        Ok(Self { client, cache_dir, refresh })
    }

    pub fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T> {
        let resp = self.client.get(url).send().with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        let body = resp.text()?;
        if !status.is_success() {
            anyhow::bail!("GET {url} returned {status}: {}", truncate(&body, 300));
        }
        serde_json::from_str(&body).with_context(|| format!("parsing response from {url}"))
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
        fs::write(&path, serde_json::to_string(&value)?)
            .with_context(|| format!("writing cache file {}", path.display()))?;
        Ok(value)
    }
}

fn is_fresh(path: &PathBuf) -> bool {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age < MAX_AGE)
}

fn cache_dir() -> PathBuf {
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
