//! Paired phones in the app settings (never the profile) and the pairing code.

use super::pairing::{hash_token, new_token, token_matches, AuthError, Pairing};
use super::protocol::clean_name;
use crate::config::{RemoteDevice, SharedSettings};
use parking_lot::Mutex;
use std::net::IpAddr;
use std::time::Instant;

pub struct RemoteAuth {
    settings: SharedSettings,
    pairing: Mutex<Pairing>,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

impl RemoteAuth {
    pub fn new(settings: SharedSettings) -> RemoteAuth {
        RemoteAuth {
            settings,
            pairing: Mutex::new(Pairing::default()),
        }
    }

    pub fn start_pairing(&self, now: Instant) -> (String, Instant) {
        self.pairing.lock().start(now)
    }

    pub fn cancel_pairing(&self) {
        self.pairing.lock().cancel();
    }

    pub fn pending(&self, now: Instant) -> Option<Instant> {
        self.pairing.lock().pending(now)
    }

    pub fn devices(&self) -> Vec<RemoteDevice> {
        self.settings.lock().settings.remote.devices.clone()
    }

    /// Use the pairing code: a new device and its token (returned once, stored hashed).
    pub fn pair(
        &self,
        code: &str,
        name: &str,
        ip: IpAddr,
        now: Instant,
    ) -> Result<(RemoteDevice, String), AuthError> {
        self.pairing.lock().attempt(code, ip, now)?;
        let token = new_token();
        let at = now_ms();
        let device = RemoteDevice {
            id: uuid::Uuid::new_v4().to_string(),
            name: clean_name(name),
            token_hash: hash_token(&token),
            created_at: at,
            last_seen: Some(at),
        };
        let mut store = self.settings.lock();
        store.settings.remote.devices.push(device.clone());
        if let Err(e) = store.save() {
            store.settings.remote.devices.retain(|d| d.id != device.id);
            tracing::warn!(error = %e, "paired phone could not be saved");
            return Err(AuthError::Unavailable);
        }
        tracing::info!(device = %device.id, name = %device.name, %ip, "phone paired");
        Ok((device, token))
    }

    /// Sign in with a token; failures count against the address like wrong codes.
    pub fn authenticate(
        &self,
        token: &str,
        ip: IpAddr,
        now: Instant,
    ) -> Result<RemoteDevice, AuthError> {
        if !self.pairing.lock().allowed(ip, now) {
            return Err(AuthError::RateLimited);
        }
        let mut store = self.settings.lock();
        // Every stored hash is compared, so the time taken does not tell which one matched.
        let mut found = None;
        for (i, d) in store.settings.remote.devices.iter().enumerate() {
            if token_matches(token, &d.token_hash) {
                found = Some(i);
            }
        }
        let Some(i) = found else {
            drop(store);
            self.pairing.lock().record_failure(ip, now);
            return Err(AuthError::Rejected);
        };
        store.settings.remote.devices[i].last_seen = Some(now_ms());
        if let Err(e) = store.save() {
            tracing::warn!(error = %e, "last-seen time of a phone could not be saved");
        }
        Ok(store.settings.remote.devices[i].clone())
    }

    /// Forget a device; its token stops working. False when there was no such device.
    pub fn revoke(&self, device_id: &str) -> Result<bool, String> {
        let mut store = self.settings.lock();
        let before = store.settings.remote.devices.clone();
        store.settings.remote.devices.retain(|d| d.id != device_id);
        if store.settings.remote.devices.len() == before.len() {
            return Ok(false);
        }
        if let Err(e) = store.save() {
            store.settings.remote.devices = before;
            return Err(e.to_string());
        }
        tracing::info!(device = %device_id, "phone revoked");
        Ok(true)
    }
}
