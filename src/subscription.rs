use age::armor::ArmoredReader;
use age::x25519;
use age::{Decryptor, Identity};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local};
use humansize::{BINARY, format_size};
use reqwest::{Client, Proxy};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Read;
use std::str::FromStr;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::config::{KoipyConfig, SubscriptionAgeConfig};

const MAX_SUBSCRIPTION_BYTES: usize = 50 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct SubscriptionCollector {
    client: Client,
    user_agent: String,
    proxy: Option<String>,
    cache_time: Duration,
    age: SubscriptionAgeConfig,
}

impl SubscriptionCollector {
    pub fn new(config: &KoipyConfig) -> Result<Self> {
        let mut builder = Client::builder().timeout(Duration::from_secs(20));
        if let Some(proxy) = config
            .network
            .http_proxy
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            builder = builder.proxy(Proxy::all(proxy).context("invalid http proxy")?);
        }
        Ok(Self {
            client: builder.build().context("failed to build HTTP client")?,
            user_agent: config.network.user_agent.clone(),
            proxy: config.network.http_proxy.clone(),
            cache_time: Duration::from_secs(config.bot.cache_time),
            age: config.subscription.age.clone(),
        })
    }

    pub async fn fetch_config(&self, url: &str) -> Result<Vec<u8>> {
        if let Some(bytes) = self.cached(url)? {
            return Ok(bytes);
        }
        tracing::info!(url, proxy = ?self.proxy, "fetching subscription");
        let mut request = self.client.get(url).header("user-agent", &self.user_agent);
        if self.age.enable
            && !self.age.public_key.trim().is_empty()
            && !self.age.public_key_header.trim().is_empty()
        {
            request = request.header(
                self.age.public_key_header.trim(),
                self.age.public_key.trim(),
            );
        }
        let response = request
            .send()
            .await
            .context("failed to request subscription")?;

        if !response.status().is_success() {
            bail!("subscription returned HTTP {}", response.status());
        }
        if let Some(len) = response.content_length() {
            if len as usize > MAX_SUBSCRIPTION_BYTES {
                bail!("subscription is larger than 50MB");
            }
        }
        let bytes = response
            .bytes()
            .await
            .context("failed to read subscription")?;
        if bytes.len() > MAX_SUBSCRIPTION_BYTES {
            bail!("subscription is larger than 50MB");
        }
        let bytes = self.decode_age_if_needed(&bytes)?;
        self.cache(url, &bytes)?;
        Ok(bytes)
    }

    pub async fn fetch_traffic(&self, url: &str) -> Result<Option<SubscriptionTraffic>> {
        let response = self
            .client
            .get(url)
            .header("user-agent", &self.user_agent)
            .send()
            .await
            .context("failed to request subscription traffic")?;
        let Some(header) = response.headers().get("subscription-userinfo") else {
            return Ok(None);
        };
        let header = header
            .to_str()
            .context("invalid subscription-userinfo header")?;
        Ok(Some(SubscriptionTraffic::parse(header)))
    }

    fn cached(&self, url: &str) -> Result<Option<Vec<u8>>> {
        cached_subscription(url, self.cache_time)
    }

    fn cache(&self, url: &str, bytes: &[u8]) -> Result<()> {
        cache_subscription(url, bytes, self.cache_time)
    }

    fn decode_age_if_needed(&self, bytes: &[u8]) -> Result<Vec<u8>> {
        decrypt_age_armor_if_enabled(bytes, &self.age)
    }
}

const AGE_ARMOR_BEGIN: &[u8] = b"-----BEGIN AGE ENCRYPTED FILE-----";

pub fn decrypt_age_armor_if_enabled(bytes: &[u8], age: &SubscriptionAgeConfig) -> Result<Vec<u8>> {
    if !age.enable || !looks_like_age_armor(bytes) {
        return Ok(bytes.to_vec());
    }
    let secret_key = age.secret_key.trim();
    if secret_key.is_empty() {
        bail!("subscription.age.secretKey is required for age encrypted subscriptions");
    }
    let identity = x25519::Identity::from_str(secret_key)
        .map_err(|err| anyhow::anyhow!("invalid subscription.age.secretKey: {err}"))?;
    let decryptor = Decryptor::new(ArmoredReader::new(bytes))
        .context("failed to parse age armored subscription")?;
    let mut reader = decryptor
        .decrypt(std::iter::once(&identity as &dyn Identity))
        .context("failed to decrypt age subscription")?;
    let mut out = Vec::new();
    reader
        .read_to_end(&mut out)
        .context("failed to read decrypted age subscription")?;
    Ok(out)
}

fn looks_like_age_armor(bytes: &[u8]) -> bool {
    bytes
        .split(|byte| matches!(byte, b'\n' | b'\r'))
        .find_map(|line| {
            let trimmed = trim_ascii_whitespace(line);
            (!trimmed.is_empty()).then_some(trimmed)
        })
        .map(|line| line == AGE_ARMOR_BEGIN)
        .unwrap_or(false)
}

fn trim_ascii_whitespace(mut bytes: &[u8]) -> &[u8] {
    while let Some((first, rest)) = bytes.split_first() {
        if first.is_ascii_whitespace() {
            bytes = rest;
        } else {
            break;
        }
    }
    while let Some((last, rest)) = bytes.split_last() {
        if last.is_ascii_whitespace() {
            bytes = rest;
        } else {
            break;
        }
    }
    bytes
}

#[derive(Debug, Clone)]
struct SubscriptionCacheEntry {
    created: std::time::Instant,
    bytes: Vec<u8>,
}

static SUBSCRIPTION_CACHE: OnceLock<Mutex<HashMap<String, SubscriptionCacheEntry>>> =
    OnceLock::new();

fn cached_subscription(url: &str, ttl: Duration) -> Result<Option<Vec<u8>>> {
    if ttl.is_zero() {
        return Ok(None);
    }
    let cache = SUBSCRIPTION_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = cache
        .lock()
        .map_err(|_| anyhow::anyhow!("subscription cache lock poisoned"))?;
    if let Some(entry) = guard.get(url) {
        if entry.created.elapsed() <= ttl {
            return Ok(Some(entry.bytes.clone()));
        }
    }
    guard.remove(url);
    Ok(None)
}

fn cache_subscription(url: &str, bytes: &[u8], ttl: Duration) -> Result<()> {
    if ttl.is_zero() {
        return Ok(());
    }
    let cache = SUBSCRIPTION_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = cache
        .lock()
        .map_err(|_| anyhow::anyhow!("subscription cache lock poisoned"))?;
    guard.insert(
        url.to_string(),
        SubscriptionCacheEntry {
            created: std::time::Instant::now(),
            bytes: bytes.to_vec(),
        },
    );
    Ok(())
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubscriptionTraffic {
    pub upload: u64,
    pub download: u64,
    pub total: u64,
    pub expire: Option<i64>,
}

impl SubscriptionTraffic {
    pub fn parse(header: &str) -> Self {
        let mut traffic = Self::default();
        for part in header.split(';') {
            let mut kv = part.trim().splitn(2, '=');
            let key = kv.next().unwrap_or_default();
            let value = kv
                .next()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or_default();
            match key {
                "upload" => traffic.upload = value,
                "download" => traffic.download = value,
                "total" => traffic.total = value,
                "expire" => traffic.expire = Some(value as i64),
                _ => {}
            }
        }
        traffic
    }

    pub fn used(&self) -> u64 {
        self.upload.saturating_add(self.download)
    }

    pub fn summary(&self) -> String {
        let expire = self
            .expire
            .and_then(|ts| DateTime::from_timestamp(ts, 0))
            .map(|dt| {
                dt.with_timezone(&Local)
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string()
            })
            .unwrap_or_else(|| "unknown".to_string());
        let percent = if self.total > 0 {
            format!("{:.1}%", self.used() as f64 / self.total as f64 * 100.0)
        } else {
            "unknown".to_string()
        };
        format!(
            "Upload: {}\nDownload: {}\nUsed: {}\nTotal: {}\nUsed ratio: {}\nExpire: {}",
            format_size(self.upload, BINARY),
            format_size(self.download, BINARY),
            format_size(self.used(), BINARY),
            format_size(self.total, BINARY),
            percent,
            expire,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_subscription_traffic() {
        let traffic =
            SubscriptionTraffic::parse("upload=1024; download=2048; total=4096; expire=2000000000");
        assert_eq!(traffic.used(), 3072);
        assert_eq!(traffic.total, 4096);
    }

    #[test]
    fn cache_respects_documented_ttl() {
        let url = "https://cache.example/sub";
        cache_subscription(url, b"first", Duration::from_secs(60)).expect("cache");
        assert_eq!(
            cached_subscription(url, Duration::from_secs(60)).expect("cached"),
            Some(b"first".to_vec())
        );
        assert_eq!(
            cached_subscription(url, Duration::from_secs(0)).expect("disabled"),
            None
        );
    }

    #[test]
    fn age_decrypt_ignores_plain_subscriptions() {
        let age = SubscriptionAgeConfig {
            enable: true,
            ..Default::default()
        };
        let plain = b"proxies:\n  - name: plain\n    type: ss\n";
        assert_eq!(
            decrypt_age_armor_if_enabled(plain, &age).expect("plain"),
            plain
        );
    }

    #[test]
    fn age_decrypt_requires_secret_for_armored_subscription() {
        let age = SubscriptionAgeConfig {
            enable: true,
            ..Default::default()
        };
        let armored = b"-----BEGIN AGE ENCRYPTED FILE-----\n-----END AGE ENCRYPTED FILE-----\n";
        let err = decrypt_age_armor_if_enabled(armored, &age).expect_err("missing secret");
        assert!(
            err.to_string()
                .contains("subscription.age.secretKey is required")
        );
    }

    #[test]
    fn decrypts_age_armored_subscription() {
        use age::armor::{ArmoredWriter, Format};
        use age::secrecy::ExposeSecret;
        use age::{Encryptor, Recipient};
        use std::io::Write;

        let identity = x25519::Identity::generate();
        let recipient = identity.to_public();
        let secret_key = identity.to_string().expose_secret().to_string();
        let plain = b"proxies:\n  - name: encrypted\n    type: ss\n";
        let mut encrypted = Vec::new();
        {
            let armor = ArmoredWriter::wrap_output(&mut encrypted, Format::AsciiArmor)
                .expect("armor writer");
            let encryptor =
                Encryptor::with_recipients(std::iter::once(&recipient as &dyn Recipient))
                    .expect("encryptor");
            let mut writer = encryptor.wrap_output(armor).expect("encrypt writer");
            writer.write_all(plain).expect("write plain");
            writer
                .finish()
                .and_then(|armor| armor.finish())
                .expect("finish encryption");
        }
        let age = SubscriptionAgeConfig {
            enable: true,
            secret_key,
            ..Default::default()
        };

        let decrypted = decrypt_age_armor_if_enabled(&encrypted, &age).expect("decrypt");

        assert_eq!(decrypted, plain);
    }
}
