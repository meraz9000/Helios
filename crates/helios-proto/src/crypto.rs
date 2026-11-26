use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use hmac::{Hmac, Mac};
use rand::RngCore;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use thiserror::Error;

type HmacSha1 = Hmac<Sha1>;

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("invalid public key encoding: {0}")]
    InvalidPublicKey(String),
    #[error("invalid private key encoding: {0}")]
    InvalidPrivateKey(String),
    #[error("invalid signature encoding: {0}")]
    InvalidSignature(String),
    #[error("signature verification failed")]
    VerificationFailed,
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

/// Human-friendly deterministic device identifier derived from an Ed25519 public key
/// (e.g., `HL-A1B2-C3D4`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceId(pub String);

impl std::fmt::Display for DeviceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Derive a stable, readable device ID from a 32-byte Ed25519 public key.
pub fn derive_device_id(public_key_bytes: &[u8; 32]) -> DeviceId {
    let hash = Sha256::digest(public_key_bytes);
    let hex_str = hex::encode_upper(&hash[..4]);
    DeviceId(format!("HL-{}-{}", &hex_str[0..4], &hex_str[4..8]))
}

/// Generate a readable, high-entropy 12-character Host Access Passcode
/// (e.g., `K7M9-P2X4-Q8R3`) using an unambiguous alphabet (no 0/O/1/I).
pub fn generate_access_passcode() -> String {
    const ALPHABET: &[u8] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";
    let mut raw = [0u8; 12];
    OsRng.fill_bytes(&mut raw);
    let chars: Vec<char> = raw
        .iter()
        .map(|&b| ALPHABET[(b as usize) % ALPHABET.len()] as char)
        .collect();
    format!(
        "{}{}{}{}-{}{}{}{}-{}{}{}{}",
        chars[0],
        chars[1],
        chars[2],
        chars[3],
        chars[4],
        chars[5],
        chars[6],
        chars[7],
        chars[8],
        chars[9],
        chars[10],
        chars[11]
    )
}

/// Persisted representation of a device's Ed25519 keypair, display label, and unique Host Access Passcode.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredIdentity {
    pub name: String,
    pub secret_key_b64: String,
    pub public_key_b64: String,
    pub device_id: DeviceId,
    #[serde(default)]
    pub access_passcode: Option<String>,
}

/// Device cryptographic identity used to authenticate with the DigitalOcean relay
/// and establish mutual trust between Host and Client.
#[derive(Clone)]
pub struct DeviceIdentity {
    pub name: String,
    signing_key: SigningKey,
    pub device_id: DeviceId,
    /// Unique persistent passcode required from connecting Clients before the Host
    /// wakes screen capture or accepts remote input.
    pub access_passcode: String,
}

impl std::fmt::Debug for DeviceIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceIdentity")
            .field("name", &self.name)
            .field("device_id", &self.device_id)
            .field("public_key_b64", &self.public_key_b64())
            .finish()
    }
}

impl DeviceIdentity {
    /// Generate a brand-new Ed25519 identity + unique access passcode using OS randomness.
    pub fn generate(name: impl Into<String>) -> Self {
        let mut csprng = OsRng;
        let signing_key = SigningKey::generate(&mut csprng);
        let pub_bytes = signing_key.verifying_key().to_bytes();
        let device_id = derive_device_id(&pub_bytes);
        Self {
            name: name.into(),
            signing_key,
            device_id,
            access_passcode: generate_access_passcode(),
        }
    }

    /// Load an existing identity from JSON file, or create and save one if missing.
    pub fn load_or_create(path: &Path, default_name: impl Into<String>) -> Result<Self, CryptoError> {
        if path.exists() {
            let mut id = Self::load(path)?;
            // Persist newly generated passcode if upgrading an older identity file
            if id.access_passcode.is_empty() {
                id.access_passcode = generate_access_passcode();
                id.save(path)?;
            }
            Ok(id)
        } else {
            let id = Self::generate(default_name);
            id.save(path)?;
            Ok(id)
        }
    }

    pub fn load(path: &Path) -> Result<Self, CryptoError> {
        let raw = fs::read_to_string(path)?;
        let stored: StoredIdentity = serde_json::from_str(&raw)?;
        let secret_bytes = B64
            .decode(&stored.secret_key_b64)
            .map_err(|e| CryptoError::InvalidPrivateKey(e.to_string()))?;
        let secret_arr: [u8; 32] = secret_bytes
            .try_into()
            .map_err(|_| CryptoError::InvalidPrivateKey("expected 32 bytes".into()))?;
        let signing_key = SigningKey::from_bytes(&secret_arr);
        let pub_bytes = signing_key.verifying_key().to_bytes();
        let device_id = derive_device_id(&pub_bytes);
        let access_passcode = stored
            .access_passcode
            .filter(|s| !s.is_empty())
            .unwrap_or_else(generate_access_passcode);
        Ok(Self {
            name: stored.name,
            signing_key,
            device_id,
            access_passcode,
        })
    }

    pub fn save(&self, path: &Path) -> Result<(), CryptoError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let stored = StoredIdentity {
            name: self.name.clone(),
            secret_key_b64: B64.encode(self.signing_key.to_bytes()),
            public_key_b64: self.public_key_b64(),
            device_id: self.device_id.clone(),
            access_passcode: Some(self.access_passcode.clone()),
        };
        let json = serde_json::to_string_pretty(&stored)?;
        fs::write(path, json)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = fs::Permissions::from_mode(0o600);
            let _ = fs::set_permissions(path, perms);
        }

        Ok(())
    }

    pub fn public_key_bytes(&self) -> [u8; 32] {
        self.signing_key.verifying_key().to_bytes()
    }

    pub fn public_key_b64(&self) -> String {
        B64.encode(self.public_key_bytes())
    }

    /// Sign an arbitrary payload (typically a relay-issued challenge nonce) and
    /// return base64-encoded Ed25519 signature.
    pub fn sign_b64(&self, message: &[u8]) -> String {
        let sig: Signature = self.signing_key.sign(message);
        B64.encode(sig.to_bytes())
    }
}

/// Verify an Ed25519 signature given base64-encoded public key and signature.
pub fn verify_signature(
    public_key_b64: &str,
    message: &[u8],
    signature_b64: &str,
) -> Result<DeviceId, CryptoError> {
    let pub_bytes = B64
        .decode(public_key_b64)
        .map_err(|e| CryptoError::InvalidPublicKey(e.to_string()))?;
    let pub_arr: [u8; 32] = pub_bytes
        .try_into()
        .map_err(|_| CryptoError::InvalidPublicKey("expected 32 bytes".into()))?;
    let verifying_key = VerifyingKey::from_bytes(&pub_arr)
        .map_err(|e| CryptoError::InvalidPublicKey(e.to_string()))?;

    let sig_bytes = B64
        .decode(signature_b64)
        .map_err(|e| CryptoError::InvalidSignature(e.to_string()))?;
    let sig_arr: [u8; 64] = sig_bytes
        .try_into()
        .map_err(|_| CryptoError::InvalidSignature("expected 64 bytes".into()))?;
    let signature = Signature::from_bytes(&sig_arr);

    verifying_key
        .verify(message, &signature)
        .map_err(|_| CryptoError::VerificationFailed)?;

    Ok(derive_device_id(&pub_arr))
}

/// Generate a random 32-byte base64 challenge nonce.
pub fn generate_nonce_b64() -> String {
    let mut buf = [0u8; 32];
    OsRng.fill_bytes(&mut buf);
    B64.encode(buf)
}

/// Short-lived pairing token generated on the DigitalOcean relay so new
/// Windows/macOS devices can self-register their Ed25519 public key once.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairingToken {
    pub token: String,
    pub expires_at_unix: u64,
}

impl PairingToken {
    pub fn generate(ttl_secs: u64) -> Self {
        let mut buf = [0u8; 16];
        OsRng.fill_bytes(&mut buf);
        let hex_str = hex::encode_upper(buf);
        // Format cleanly in 4-char groups for easy copy/paste
        let token = format!(
            "PAIR-{}-{}-{}-{}",
            &hex_str[0..4],
            &hex_str[4..8],
            &hex_str[8..12],
            &hex_str[12..16]
        );
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self {
            token,
            expires_at_unix: now + ttl_secs,
        }
    }

    pub fn is_expired(&self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        now > self.expires_at_unix
    }
}

/// ICE server entry returned by the relay (STUN and/or TURN).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IceServer {
    pub urls: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

/// Time-limited TURN credentials compatible with `coturn`'s `use-auth-secret`
/// (RFC / TURN REST API draft).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TurnCredentials {
    pub username: String,
    pub credential: String,
    pub ttl_secs: u64,
}

/// Generate ephemeral `coturn` credentials valid for `ttl_secs` using the shared
/// static auth secret configured on the DigitalOcean droplet.
pub fn generate_turn_credentials(
    device_id: &DeviceId,
    shared_secret: &str,
    ttl_secs: u64,
) -> TurnCredentials {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let expiry = now + ttl_secs;
    let username = format!("{}:{}", expiry, device_id.0);

    let mut mac =
        HmacSha1::new_from_slice(shared_secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(username.as_bytes());
    let credential = B64.encode(mac.finalize().into_bytes());

    TurnCredentials {
        username,
        credential,
        ttl_secs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_sign_and_verify() {
        let id = DeviceIdentity::generate("win-workstation");
        let nonce = generate_nonce_b64();
        let sig = id.sign_b64(nonce.as_bytes());

        let verified_id = verify_signature(&id.public_key_b64(), nonce.as_bytes(), &sig).unwrap();
        assert_eq!(verified_id, id.device_id);
        assert!(verified_id.0.starts_with("HL-"));

        // Tampered message must fail
        assert!(verify_signature(&id.public_key_b64(), b"tampered", &sig).is_err());
    }

    #[test]
    fn turn_credentials_format() {
        let id = DeviceId("HL-1234-ABCD".into());
        let creds = generate_turn_credentials(&id, "super-secret-turn-key", 3600);
        assert!(creds.username.ends_with(":HL-1234-ABCD"));
        assert!(!creds.credential.is_empty());
        assert_eq!(creds.ttl_secs, 3600);
    }
}
