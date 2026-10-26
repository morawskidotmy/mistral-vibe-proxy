//! Mistral API key resolution, mirroring how `vibe` itself finds the key.
//!
//! Order (env-wins, like `vibe.utils.api_keys.resolve_api_key` and the
//! `.env` loader in `vibe.core.config.vibe_schema`):
//!
//! 1. `MISTRAL_API_KEY` in the proxy's environment
//! 2. `MISTRAL_API_KEY` in `$VIBE_HOME/.env` (default `~/.vibe/.env`),
//!    unless the environment already holds a non-empty value
//! 3. the OS keyring: service `ai.mistral.vibe` (legacy service `vibe`),
//!    account `MISTRAL_API_KEY`
//!
//! The resolved key is cached for a short TTL so a `vibe --setup` run takes
//! effect without a proxy restart, and is never logged.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use tracing::{debug, warn};

pub const API_KEY_ENV: &str = "MISTRAL_API_KEY";
const KEYRING_SERVICES: [&str; 2] = ["ai.mistral.vibe", "vibe"];
/// How long a resolved key (or a confirmed absence) is trusted.
const CACHE_TTL: Duration = Duration::from_secs(60);

#[derive(Default)]
struct Cache {
    value: Option<Option<String>>,
    at: Option<Instant>,
}

static CACHE: Mutex<Cache> = Mutex::new(Cache { value: None, at: None });

pub fn vibe_home() -> std::path::PathBuf {
    if let Ok(home) = std::env::var("VIBE_HOME") {
        if !home.is_empty() {
            return std::path::PathBuf::from(home);
        }
    }
    let user_home = std::env::var("HOME").unwrap_or_default();
    std::path::PathBuf::from(user_home).join(".vibe")
}

/// Extract `name` from a dotenv-format file, stripping optional quotes.
/// Only the requested key is ever looked at.
fn read_dotenv_key(path: &std::path::Path, name: &str) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != name {
            continue;
        }
        let mut value = value.trim();
        if (value.starts_with('"') && value.ends_with('"') && value.len() >= 2)
            || (value.starts_with('\'') && value.ends_with('\'') && value.len() >= 2)
        {
            value = &value[1..value.len() - 1];
        }
        let value = value.trim();
        if value.is_empty() {
            return None;
        }
        return Some(value.to_string());
    }
    None
}

/// `secret-tool lookup service <service> username <account>`, when
/// `secret-tool` is installed. Prints the secret on stdout, like vibe's
/// keyring reads this never touches stderr and is never logged.
fn read_keyring(service: &str, account: &str) -> Option<String> {
    let out = std::process::Command::new("secret-tool")
        .arg("lookup")
        .arg("service")
        .arg(service)
        .arg("username")
        .arg(account)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

/// The key from the keyring alone: the current service first, then the
/// pre-rename legacy service.
fn keyring_key() -> Option<String> {
    for service in KEYRING_SERVICES {
        if let Some(k) = read_keyring(service, API_KEY_ENV) {
            debug!("Mistral key found in keyring service {service}");
            return Some(k);
        }
    }
    None
}

/// Resolve the Mistral API key. Logs only where the key came from, never
/// the key itself. `None` means no key could be found.
pub fn resolve_api_key() -> Option<String> {
    {
        let cache = CACHE.lock().unwrap();
        if let Some(at) = cache.at {
            if at.elapsed() < CACHE_TTL {
                return cache.value.clone().flatten();
            }
        }
    }

    let mut source = "";
    let key = std::env::var(API_KEY_ENV)
        .ok()
        .and_then(|v| {
            let v = v.trim().to_string();
            if v.is_empty() {
                None
            } else {
                Some(v)
            }
        })
        .or_else(|| {
            let env_path = vibe_home().join(".env");
            if env_path.is_file() {
                if let Some(k) = read_dotenv_key(&env_path, API_KEY_ENV) {
                    source = "$VIBE_HOME/.env";
                    return Some(k);
                }
            }
            None
        })
        .or_else(|| {
            let k = keyring_key();
            if k.is_some() {
                source = "keyring";
            }
            k
        });

    if key.is_none() {
        warn!("no {API_KEY_ENV} found in environment, $VIBE_HOME/.env, or keyring");
    } else if source.is_empty() {
        debug!("Mistral key resolved from environment");
    } else {
        debug!("Mistral key resolved from {source}");
    }

    let mut cache = CACHE.lock().unwrap();
    cache.value = Some(key.clone());
    cache.at = Some(Instant::now());
    key
}
