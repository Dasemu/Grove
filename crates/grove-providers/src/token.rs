//! Token storage: `GROVE_TOKEN_<ID>` env var first (headless machines, tests), then the
//! desktop Secret Service. Tokens are never written to the config file.

use crate::{Error, Result};

const SERVICE: &str = "grove";

pub fn env_name(provider_id: &str) -> String {
    let id: String = provider_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("GROVE_TOKEN_{id}")
}

pub fn get(provider_id: &str) -> Result<String> {
    if let Ok(t) = std::env::var(env_name(provider_id))
        && !t.trim().is_empty()
    {
        return Ok(t.trim().to_string());
    }
    let e = keyring::Entry::new(SERVICE, provider_id).map_err(|e| Error::Other(e.to_string()))?;
    match e.get_password() {
        Ok(t) => Ok(t),
        Err(keyring::Error::NoEntry) => Err(Error::NoToken(provider_id.into())),
        Err(e) => Err(Error::Other(format!("secret service: {e}"))),
    }
}

pub fn set(provider_id: &str, token: &str) -> Result<()> {
    keyring::Entry::new(SERVICE, provider_id)
        .and_then(|e| e.set_password(token))
        .map_err(|e| Error::Other(format!("secret service: {e}")))
}

pub fn delete(provider_id: &str) -> Result<()> {
    match keyring::Entry::new(SERVICE, provider_id).and_then(|e| e.delete_credential()) {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(Error::Other(format!("secret service: {e}"))),
    }
}
