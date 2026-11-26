use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;
use helios_proto::{DeviceId, DeviceRole, PairingToken};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthorizedDevice {
    pub device_id: DeviceId,
    pub name: String,
    pub os: String,
    pub role: DeviceRole,
    pub public_key_b64: String,
    pub paired_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StoreFile {
    #[serde(default)]
    devices: Vec<AuthorizedDevice>,
    #[serde(default)]
    pairing_tokens: Vec<PairingToken>,
}

/// Persistent TOML-backed allowlist of paired devices and active one-time pairing tokens.
#[derive(Debug)]
pub struct DeviceStore {
    path: Option<PathBuf>,
    devices: HashMap<DeviceId, AuthorizedDevice>,
    pairing_tokens: HashMap<String, PairingToken>,
    /// Optional static bootstrap token (from env `HELIOS_BOOTSTRAP_TOKEN`) for initial droplet setup.
    bootstrap_token: Option<String>,
}

impl DeviceStore {
    pub fn in_memory(bootstrap_token: Option<String>) -> Self {
        Self {
            path: None,
            devices: HashMap::new(),
            pairing_tokens: HashMap::new(),
            bootstrap_token,
        }
    }

    pub fn load_or_create(path: impl AsRef<Path>, bootstrap_token: Option<String>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut store = Self {
            path: Some(path.clone()),
            devices: HashMap::new(),
            pairing_tokens: HashMap::new(),
            bootstrap_token,
        };

        if path.exists() {
            let raw = fs::read_to_string(&path)
                .with_context(|| format!("failed to read device store at {}", path.display()))?;
            let parsed: StoreFile = toml::from_str(&raw)
                .with_context(|| format!("failed to parse device store at {}", path.display()))?;
            for dev in parsed.devices {
                store.devices.insert(dev.device_id.clone(), dev);
            }
            for tok in parsed.pairing_tokens {
                if !tok.is_expired() {
                    store.pairing_tokens.insert(tok.token.clone(), tok);
                }
            }
        } else {
            store.save()?;
        }

        Ok(store)
    }

    pub fn save(&self) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let mut devices: Vec<AuthorizedDevice> = self.devices.values().cloned().collect();
        devices.sort_by(|a, b| a.device_id.0.cmp(&b.device_id.0));
        let pairing_tokens: Vec<PairingToken> = self
            .pairing_tokens
            .values()
            .filter(|t| !t.is_expired())
            .cloned()
            .collect();

        let file = StoreFile {
            devices,
            pairing_tokens,
        };
        let encoded = toml::to_string_pretty(&file)?;
        fs::write(path, encoded)?;
        Ok(())
    }

    pub fn is_authorized(&self, device_id: &DeviceId, public_key_b64: &str) -> bool {
        self.devices
            .get(device_id)
            .is_some_and(|d| d.public_key_b64 == public_key_b64)
    }

    pub fn issue_pairing_token(&mut self, ttl_secs: u64) -> Result<PairingToken> {
        self.prune_expired_tokens();
        let tok = PairingToken::generate(ttl_secs);
        self.pairing_tokens.insert(tok.token.clone(), tok.clone());
        self.save()?;
        Ok(tok)
    }

    /// Reload devices and pairing tokens from disk (so tokens added via
    /// `docker exec helios-relay helios-relay gen-pairing-token` take effect immediately).
    pub fn reload_from_disk(&mut self) -> Result<()> {
        let Some(path) = self.path.clone() else {
            return Ok(());
        };
        if !path.exists() {
            return Ok(());
        }
        let raw = fs::read_to_string(&path)?;
        let parsed: StoreFile = toml::from_str(&raw)?;
        for dev in parsed.devices {
            self.devices.entry(dev.device_id.clone()).or_insert(dev);
        }
        for tok in parsed.pairing_tokens {
            if !tok.is_expired() {
                self.pairing_tokens.entry(tok.token.clone()).or_insert(tok);
            }
        }
        Ok(())
    }

    /// Consume a one-time pairing token (or verify the bootstrap token) and enroll the device.
    pub fn enroll_with_token(
        &mut self,
        token: &str,
        device_id: DeviceId,
        name: String,
        os: String,
        role: DeviceRole,
        public_key_b64: String,
    ) -> Result<bool> {
        let _ = self.reload_from_disk();
        self.prune_expired_tokens();

        let bootstrap_ok = self
            .bootstrap_token
            .as_deref()
            .is_some_and(|bt| !bt.is_empty() && bt == token);

        let one_time_ok = if !bootstrap_ok {
            if let Some(entry) = self.pairing_tokens.remove(token) {
                !entry.is_expired()
            } else {
                false
            }
        } else {
            false
        };

        if !bootstrap_ok && !one_time_ok {
            return Ok(false);
        }

        let dev = AuthorizedDevice {
            device_id: device_id.clone(),
            name,
            os,
            role,
            public_key_b64,
            paired_at: Utc::now().to_rfc3339(),
        };
        self.devices.insert(device_id, dev);
        self.save()?;
        Ok(true)
    }

    pub fn list_devices(&self) -> Vec<AuthorizedDevice> {
        let mut list: Vec<_> = self.devices.values().cloned().collect();
        list.sort_by(|a, b| a.device_id.0.cmp(&b.device_id.0));
        list
    }

    fn prune_expired_tokens(&mut self) {
        self.pairing_tokens.retain(|_, t| !t.is_expired());
    }
}
