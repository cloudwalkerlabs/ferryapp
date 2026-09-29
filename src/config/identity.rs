use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, PublicKeyData};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;
use x509_parser::parse_x509_certificate;

use crate::store::{ConfigKey, Store, StoreError};

/// Where the identity is kept.
pub const IDENTITY: ConfigKey<StoredIdentity> = ConfigKey::new("core.identity");

/// Persistent TLS identity used by the local Ferry device.
///
/// This deliberately does not implement `Debug`, preventing accidental key
/// disclosure through otherwise harmless diagnostic formatting.
#[derive(Clone, PartialEq, Eq)]
pub struct LocalIdentity {
    device_id: String,
    certificate_der: Vec<u8>,
    private_key_der: Vec<u8>,
}

/// [`LocalIdentity`] as stored under [`IDENTITY`], its certificate and key
/// in base64. Checked only when it is loaded.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StoredIdentity {
    pub device_id: String,
    #[serde(with = "base64_bytes")]
    pub certificate_der: Vec<u8>,
    #[serde(with = "base64_bytes")]
    pub private_key_der: Vec<u8>,
}

impl LocalIdentity {
    /// Load the identity from `store`, creating and storing one if there is
    /// none. A stored identity that is invalid is an error, never replaced:
    /// a new one would be a new device ID, and every pairing lost.
    pub async fn load_or_create(store: &Store) -> Result<Self, IdentityError> {
        store
            .transaction(|transaction| {
                let stored = transaction
                    .get_strict(&IDENTITY)
                    .map_err(|error| match error {
                        StoreError::Undecodable { .. } => IdentityError::Corrupt,
                        error => IdentityError::Store(error),
                    })?;
                if let Some(stored) = stored {
                    return Self::from_stored(stored);
                }
                let identity = Self::generate()?;
                transaction.set(&IDENTITY, &identity.to_stored())?;
                Ok(identity)
            })
            .await
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    pub fn certificate_der(&self) -> &[u8] {
        &self.certificate_der
    }

    pub fn private_key_der(&self) -> &[u8] {
        &self.private_key_der
    }

    fn generate() -> Result<Self, IdentityError> {
        let device_id = Uuid::new_v4().simple().to_string();
        if !is_local_device_id(&device_id) {
            return Err(IdentityError::InvalidDeviceId);
        }

        let signing_key = KeyPair::generate().map_err(|_| IdentityError::Generation)?;
        let mut params = CertificateParams::default();
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, device_id.as_str());
        let certificate = params
            .self_signed(&signing_key)
            .map_err(|_| IdentityError::Generation)?;

        let identity = Self {
            device_id,
            certificate_der: certificate.der().to_vec(),
            private_key_der: signing_key.serialize_der(),
        };
        identity.validate()?;
        Ok(identity)
    }

    fn from_stored(stored: StoredIdentity) -> Result<Self, IdentityError> {
        let identity = Self {
            device_id: stored.device_id,
            certificate_der: stored.certificate_der,
            private_key_der: stored.private_key_der,
        };
        identity.validate()?;
        Ok(identity)
    }

    fn to_stored(&self) -> StoredIdentity {
        StoredIdentity {
            device_id: self.device_id.clone(),
            certificate_der: self.certificate_der.clone(),
            private_key_der: self.private_key_der.clone(),
        }
    }

    fn validate(&self) -> Result<(), IdentityError> {
        if !is_local_device_id(&self.device_id) {
            return Err(IdentityError::InvalidDeviceId);
        }

        let (remaining, certificate) = parse_x509_certificate(&self.certificate_der)
            .map_err(|_| IdentityError::InvalidCertificate)?;
        if !remaining.is_empty() {
            return Err(IdentityError::InvalidCertificate);
        }

        let mut common_names = certificate.subject().iter_common_name();
        let common_name = common_names
            .next()
            .and_then(|value| value.as_str().ok())
            .ok_or(IdentityError::CertificateIdentityMismatch)?;
        if common_name != self.device_id || common_names.next().is_some() {
            return Err(IdentityError::CertificateIdentityMismatch);
        }
        if certificate.subject() != certificate.issuer() {
            return Err(IdentityError::InvalidCertificate);
        }

        let signing_key = KeyPair::try_from(self.private_key_der.as_slice())
            .map_err(|_| IdentityError::InvalidPrivateKey)?;
        if certificate.public_key().raw != signing_key.subject_public_key_info() {
            return Err(IdentityError::KeyCertificateMismatch);
        }

        Ok(())
    }
}

fn is_local_device_id(device_id: &str) -> bool {
    device_id.len() == 32 && device_id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Bytes as a base64 string, for [`StoredIdentity`].
mod base64_bytes {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        STANDARD
            .decode(String::deserialize(deserializer)?)
            .map_err(D::Error::custom)
    }
}

#[derive(Debug, Error)]
pub enum IdentityError {
    #[error("identity storage operation failed")]
    Store(#[from] StoreError),
    #[error("the stored identity is corrupt")]
    Corrupt,
    #[error("identity generation failed")]
    Generation,
    #[error("device ID must be exactly 32 hexadecimal characters")]
    InvalidDeviceId,
    #[error("identity certificate is invalid")]
    InvalidCertificate,
    #[error("certificate Common Name does not match the device ID")]
    CertificateIdentityMismatch,
    #[error("identity private key is invalid")]
    InvalidPrivateKey,
    #[error("identity private key does not match its certificate")]
    KeyCertificateMismatch,
}

#[cfg(test)]
mod tests {
    use base64::{Engine, engine::general_purpose::STANDARD};

    use super::*;

    #[tokio::test]
    async fn identity_is_persistent_and_well_formed() {
        let directory = tempfile::tempdir().unwrap();
        let first = LocalIdentity::load_or_create(&Store::open(directory.path()).await.unwrap())
            .await
            .unwrap();
        let second = LocalIdentity::load_or_create(&Store::open(directory.path()).await.unwrap())
            .await
            .unwrap();

        assert!(first == second);
        assert_eq!(first.device_id().len(), 32);
        assert!(
            first
                .device_id()
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        );
    }

    #[tokio::test]
    async fn the_certificate_and_key_are_stored_as_base64() {
        let store = Store::open_in_memory().await.unwrap();
        let identity = LocalIdentity::load_or_create(&store).await.unwrap();
        let json = serde_json::to_value(store.get(&IDENTITY).await.unwrap().unwrap()).unwrap();
        assert_eq!(json["deviceId"], identity.device_id());
        assert_eq!(
            json["certificateDer"]
                .as_str()
                .map(|text| STANDARD.decode(text).unwrap()),
            Some(identity.certificate_der().to_vec())
        );
        assert!(json["privateKeyDer"].is_string());
    }

    #[tokio::test]
    async fn a_corrupt_identity_is_rejected_without_replacement() {
        const PARTIAL: ConfigKey<serde_json::Value> = ConfigKey::new("core.identity");
        let store = Store::open_in_memory().await.unwrap();
        let partial = serde_json::json!({"deviceId": "unfinished"});
        store.set(&PARTIAL, &partial).await.unwrap();

        assert!(matches!(
            LocalIdentity::load_or_create(&store).await,
            Err(IdentityError::Corrupt)
        ));
        assert_eq!(store.get(&PARTIAL).await.unwrap(), Some(partial));
    }

    #[tokio::test]
    async fn certificate_common_name_must_match_device_id() {
        let store = Store::open_in_memory().await.unwrap();
        LocalIdentity::load_or_create(&store).await.unwrap();
        let mut stored = store.get(&IDENTITY).await.unwrap().unwrap();
        stored.device_id = "11111111111111111111111111111111".into();
        store.set(&IDENTITY, &stored).await.unwrap();

        assert!(matches!(
            LocalIdentity::load_or_create(&store).await,
            Err(IdentityError::CertificateIdentityMismatch)
        ));
    }
}
