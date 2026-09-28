//! Paired devices: the `devices` table, a row per paired peer with the
//! certificate it's pinned to and how it last described itself.

use rusqlite::{OptionalExtension, Row, params};
use serde_json::Value;
use x509_parser::parse_x509_certificate;

use super::{PerDevice, Store, StoreError, now_millis};
use crate::{config::is_valid_device_id, protocol::DeviceType};

/// A paired peer's certificate pin.
#[derive(Clone, PartialEq, Eq)]
pub struct TrustedDevice {
    pub device_id: String,
    pub certificate_der: Vec<u8>,
    pub last_trusted_protocol_version: u8,
    /// How the peer last described itself over an authenticated connection,
    /// so it can be listed while it is offline. `None` until it's first
    /// seen that way.
    pub last_identity: Option<TrustedIdentity>,
}

/// The parts of a paired peer's identity worth showing while it is offline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustedIdentity {
    pub device_name: String,
    pub device_type: DeviceType,
    pub incoming_capabilities: Vec<String>,
    pub outgoing_capabilities: Vec<String>,
}

const COLUMNS: &str = "device_id, certificate_der, protocol_version, name, device_type, \
                       incoming_capabilities, outgoing_capabilities";

impl Store {
    /// The paired device `device_id`, if it is one.
    pub async fn device(&self, device_id: &str) -> Result<Option<TrustedDevice>, StoreError> {
        let device_id = device_id.to_owned();
        self.read(move |connection| {
            connection
                .query_row(
                    &format!("SELECT {COLUMNS} FROM devices WHERE device_id = ?1"),
                    [device_id],
                    StoredDevice::read,
                )
                .optional()?
                .map(StoredDevice::decode)
                .transpose()
        })
        .await
    }

    /// Every paired device, by id. A record that doesn't decode is left
    /// out, and logged.
    pub async fn devices(&self) -> Result<Vec<TrustedDevice>, StoreError> {
        self.read(move |connection| {
            let mut statement =
                connection.prepare(&format!("SELECT {COLUMNS} FROM devices ORDER BY device_id"))?;
            let rows = statement.query_map([], StoredDevice::read)?;
            let mut devices = Vec::new();
            for row in rows {
                let row = row?;
                let device_id = row.device_id.clone();
                match row.decode() {
                    Ok(device) => devices.push(device),
                    Err(error) => tracing::warn!(device_id, %error, "ignoring a paired device"),
                }
            }
            Ok(devices)
        })
        .await
    }

    /// Add or replace a paired device's record. When it was first paired
    /// is kept.
    pub async fn put_device(&self, device: &TrustedDevice) -> Result<(), StoreError> {
        validate(device)?;
        let device = device.clone();
        self.transaction(move |transaction| {
            let identity = device.last_identity.as_ref();
            let capabilities = |list: fn(&TrustedIdentity) -> &Vec<String>| {
                identity
                    .map(|identity| serde_json::to_string(list(identity)))
                    .transpose()
                    .map_err(StoreError::Encoding)
            };
            let incoming = capabilities(|identity| &identity.incoming_capabilities)?;
            let outgoing = capabilities(|identity| &identity.outgoing_capabilities)?;
            let device_type = identity
                .map(|identity| device_type_name(identity.device_type))
                .transpose()?;
            let now = now_millis();
            transaction.inner.execute(
                "INSERT INTO devices (device_id, certificate_der, protocol_version, name, device_type,
                                      incoming_capabilities, outgoing_capabilities, paired_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
                 ON CONFLICT (device_id) DO UPDATE SET
                   certificate_der = excluded.certificate_der,
                   protocol_version = excluded.protocol_version,
                   name = excluded.name,
                   device_type = excluded.device_type,
                   incoming_capabilities = excluded.incoming_capabilities,
                   outgoing_capabilities = excluded.outgoing_capabilities,
                   updated_at = excluded.updated_at",
                params![
                    device.device_id,
                    device.certificate_der,
                    device.last_trusted_protocol_version,
                    identity.map(|identity| &identity.device_name),
                    device_type,
                    incoming,
                    outgoing,
                    now,
                ],
            )?;
            Ok(())
        })
        .await
    }

    /// Remove a paired device, and its [`PerDevice`] configs with it;
    /// whether it was paired.
    pub async fn remove_device(&self, device_id: &str) -> Result<bool, StoreError> {
        let device_id = device_id.to_owned();
        self.transaction(move |transaction| {
            let removed = transaction
                .inner
                .execute("DELETE FROM devices WHERE device_id = ?1", [&device_id])?;
            transaction.remove_scope::<PerDevice>(&device_id)?;
            Ok(removed > 0)
        })
        .await
    }
}

/// A `devices` row as read, before it is checked.
struct StoredDevice {
    device_id: String,
    certificate_der: Vec<u8>,
    protocol_version: i64,
    name: Option<String>,
    device_type: Option<String>,
    incoming_capabilities: Option<String>,
    outgoing_capabilities: Option<String>,
}

impl StoredDevice {
    fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            device_id: row.get(0)?,
            certificate_der: row.get(1)?,
            protocol_version: row.get(2)?,
            name: row.get(3)?,
            device_type: row.get(4)?,
            incoming_capabilities: row.get(5)?,
            outgoing_capabilities: row.get(6)?,
        })
    }

    fn decode(self) -> Result<TrustedDevice, StoreError> {
        let last_identity = match (
            self.name,
            self.device_type,
            self.incoming_capabilities,
            self.outgoing_capabilities,
        ) {
            (None, None, None, None) => None,
            (Some(device_name), Some(device_type), Some(incoming), Some(outgoing)) => {
                Some(TrustedIdentity {
                    device_name,
                    device_type: serde_json::from_value(Value::String(device_type))
                        .map_err(|_| StoreError::CorruptDevice)?,
                    incoming_capabilities: serde_json::from_str(&incoming)
                        .map_err(|_| StoreError::CorruptDevice)?,
                    outgoing_capabilities: serde_json::from_str(&outgoing)
                        .map_err(|_| StoreError::CorruptDevice)?,
                })
            }
            _ => return Err(StoreError::CorruptDevice),
        };
        let device = TrustedDevice {
            device_id: self.device_id,
            certificate_der: self.certificate_der,
            last_trusted_protocol_version: self
                .protocol_version
                .try_into()
                .map_err(|_| StoreError::UnsupportedProtocolVersion)?,
            last_identity,
        };
        validate(&device)?;
        Ok(device)
    }
}

fn validate(device: &TrustedDevice) -> Result<(), StoreError> {
    if !is_valid_device_id(&device.device_id) {
        return Err(StoreError::InvalidDeviceId);
    }
    if !matches!(device.last_trusted_protocol_version, 7 | 8) {
        return Err(StoreError::UnsupportedProtocolVersion);
    }
    let (remaining, _) = parse_x509_certificate(&device.certificate_der)
        .map_err(|_| StoreError::InvalidCertificate)?;
    if !remaining.is_empty() {
        return Err(StoreError::InvalidCertificate);
    }
    Ok(())
}

/// `phone`, `laptop`, ...: the name the identity packet uses.
fn device_type_name(device_type: DeviceType) -> Result<String, StoreError> {
    match serde_json::to_value(device_type).map_err(StoreError::Encoding)? {
        Value::String(name) => Ok(name),
        _ => unreachable!("device types serialize as strings"),
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};

    use super::*;

    /// A paired phone called "FOSS Phone" with a fresh certificate.
    pub(crate) fn trusted_device(device_id: &str) -> TrustedDevice {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::default();
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, device_id);
        let certificate = params.self_signed(&key).unwrap();
        TrustedDevice {
            device_id: device_id.into(),
            certificate_der: certificate.der().to_vec(),
            last_trusted_protocol_version: 8,
            last_identity: Some(TrustedIdentity {
                device_name: "FOSS Phone".into(),
                device_type: DeviceType::Phone,
                incoming_capabilities: vec!["kdeconnect.ping".into()],
                outgoing_capabilities: Vec::new(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{testing::trusted_device, *};
    use crate::store::ConfigKey;

    const PHONE: &str = "740bd4b9b4184ee497d6caf1da8151be";
    const LAPTOP: &str = "2c1f3a9e0b7d4c5e8f6a1b2c3d4e5f60";

    #[tokio::test]
    async fn records_round_trip_and_can_be_removed() {
        let store = Store::open_in_memory().await.unwrap();
        let device = trusted_device(PHONE);

        store.put_device(&device).await.unwrap();
        assert!(store.device(PHONE).await.unwrap() == Some(device.clone()));
        assert!(store.devices().await.unwrap() == vec![device]);
        assert!(store.remove_device(PHONE).await.unwrap());
        assert!(store.device(PHONE).await.unwrap().is_none());
        assert!(!store.remove_device(PHONE).await.unwrap());
    }

    #[tokio::test]
    async fn records_persist_across_opens() {
        let directory = tempfile::tempdir().unwrap();
        let device = trusted_device(PHONE);
        Store::open(directory.path())
            .await
            .unwrap()
            .put_device(&device)
            .await
            .unwrap();
        let store = Store::open(directory.path()).await.unwrap();
        assert!(store.devices().await.unwrap() == vec![device]);
    }

    #[tokio::test]
    async fn a_record_is_replaced_and_keeps_when_it_was_paired() {
        let store = Store::open_in_memory().await.unwrap();
        let mut device = trusted_device(PHONE);
        store.put_device(&device).await.unwrap();
        async fn paired_at(store: &Store) -> i64 {
            store
                .read(|connection| {
                    Ok(connection.query_row(
                        "SELECT paired_at FROM devices WHERE device_id = ?1",
                        [PHONE],
                        |row| row.get(0),
                    )?)
                })
                .await
                .unwrap()
        }
        let first = paired_at(&store).await;

        device.last_identity.as_mut().unwrap().device_name = "Renamed".into();
        store.put_device(&device).await.unwrap();
        assert!(store.device(PHONE).await.unwrap() == Some(device));
        assert_eq!(paired_at(&store).await, first);
    }

    #[tokio::test]
    async fn records_without_an_identity_still_load() {
        let store = Store::open_in_memory().await.unwrap();
        let mut device = trusted_device(PHONE);
        device.last_identity = None;
        store.put_device(&device).await.unwrap();
        assert!(store.device(PHONE).await.unwrap() == Some(device));
    }

    #[tokio::test]
    async fn devices_are_listed_by_id_and_a_bad_record_is_left_out() {
        let store = Store::open_in_memory().await.unwrap();
        store.put_device(&trusted_device(PHONE)).await.unwrap();
        store.put_device(&trusted_device(LAPTOP)).await.unwrap();
        store
            .transaction(|transaction| {
                transaction.inner.execute(
                    "UPDATE devices SET certificate_der = x'00' WHERE device_id = ?1",
                    [PHONE],
                )?;
                Ok::<_, StoreError>(())
            })
            .await
            .unwrap();

        let listed: Vec<_> = store
            .devices()
            .await
            .unwrap()
            .into_iter()
            .map(|device| device.device_id)
            .collect();
        assert_eq!(listed, [LAPTOP]);
        assert!(matches!(
            store.device(PHONE).await,
            Err(StoreError::InvalidCertificate)
        ));
    }

    #[tokio::test]
    async fn invalid_records_are_refused() {
        let store = Store::open_in_memory().await.unwrap();
        let mut device = trusted_device("../../not-a-device");
        assert!(matches!(
            store.put_device(&device).await,
            Err(StoreError::InvalidDeviceId)
        ));
        device.device_id = PHONE.into();
        device.last_trusted_protocol_version = 6;
        assert!(matches!(
            store.put_device(&device).await,
            Err(StoreError::UnsupportedProtocolVersion)
        ));
    }

    #[tokio::test]
    async fn removing_a_device_removes_its_configs() {
        const MUTED: ConfigKey<bool, PerDevice> = ConfigKey::new("test.muted");
        let store = Store::open_in_memory().await.unwrap();
        store.put_device(&trusted_device(PHONE)).await.unwrap();
        store.set(&MUTED.of(PHONE), &true).await.unwrap();
        store.set(&MUTED.of(LAPTOP), &true).await.unwrap();

        store.remove_device(PHONE).await.unwrap();
        assert_eq!(store.get(&MUTED.of(PHONE)).await.unwrap(), None);
        assert_eq!(store.get(&MUTED.of(LAPTOP)).await.unwrap(), Some(true));
    }
}
