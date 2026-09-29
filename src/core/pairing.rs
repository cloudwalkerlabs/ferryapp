//! Pairing: the state machine of a pairing session, in either direction,
//! and the trust it establishes or removes.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{task::JoinHandle, time::sleep};
use uuid::Uuid;

use super::{Core, CoreError, CoreState, EventData, OperationErrorCode, unix_millis, unix_seconds};
use crate::{
    protocol::{Packet, PairingBody, verification_code},
    store::TrustedDevice,
    transport::tls::subject_public_key_info,
};

/// How long a pairing session may remain non-terminal before it expires.
pub const PAIRING_TIMEOUT: Duration = Duration::from_secs(30);

/// How far an incoming pair request's timestamp may be from our clock, in
/// seconds. Matches KDE Connect's `ALLOWED_TIMESTAMP_TIME_DIFFERENCE_SECONDS`:
/// ordinary clock drift between devices is far larger than the pairing
/// timeout, so the timeout can't double as the skew limit.
pub const PAIRING_TIMESTAMP_TOLERANCE_SECS: u64 = 1800;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairingDirection {
    Incoming,
    Outgoing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairingStatus {
    Requested,
    AwaitingConfirmation,
    Accepted,
    Rejected,
    Expired,
    Failed,
}

impl PairingStatus {
    /// Whether the pairing has ended, one way or another.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Accepted | Self::Rejected | Self::Expired | Self::Failed
        )
    }

    fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (
                Self::Requested,
                Self::AwaitingConfirmation | Self::Rejected | Self::Expired | Self::Failed
            ) | (
                Self::AwaitingConfirmation,
                Self::Accepted | Self::Rejected | Self::Expired | Self::Failed
            )
        )
    }
}

/// Immutable view of an incoming or outgoing pairing operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairingSnapshot {
    pub id: Uuid,
    pub device_id: String,
    pub device_name: String,
    pub direction: PairingDirection,
    pub status: PairingStatus,
    pub verification_code: Option<String>,
    pub created_at: u64,
    pub expires_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<OperationErrorCode>,
}

/// Mutable core representation of a pairing operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pairing {
    snapshot: PairingSnapshot,
}

impl Pairing {
    pub fn new(snapshot: PairingSnapshot) -> Self {
        Self { snapshot }
    }

    pub fn snapshot(&self) -> PairingSnapshot {
        self.snapshot.clone()
    }

    pub fn transition(
        &mut self,
        next: PairingStatus,
        error_code: Option<OperationErrorCode>,
    ) -> Result<PairingSnapshot, PairingTransitionError> {
        let current = self.snapshot.status;
        if !current.can_transition_to(next) {
            return Err(PairingTransitionError { current, next });
        }
        self.snapshot.status = next;
        self.snapshot.error_code = error_code;
        Ok(self.snapshot())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[error("invalid pairing transition from {current:?} to {next:?}")]
pub struct PairingTransitionError {
    pub current: PairingStatus,
    pub next: PairingStatus,
}

pub(super) struct PairingRuntime {
    pairing: Pairing,
    /// The protocol-level pairing timestamp (Unix seconds) used to compute
    /// the verification code. Distinct from the API-facing millisecond
    /// `createdAt`/`expiresAt` bookkeeping fields.
    #[allow(dead_code)]
    protocol_timestamp: i64,
    timer: Option<JoinHandle<()>>,
}

impl Core {
    /// Every pairing this daemon process knows about, including finished
    /// ones.
    pub fn pairings(&self) -> Result<Vec<PairingSnapshot>, CoreError> {
        Ok(self
            .read_state()?
            .pairings
            .values()
            .map(|runtime| runtime.pairing.snapshot())
            .collect())
    }

    /// A pairing this daemon process knows about.
    pub fn pairing(&self, pairing_id: Uuid) -> Option<PairingSnapshot> {
        self.pairing_snapshot(pairing_id).ok()
    }

    /// Start an outgoing pairing session with a connected device.
    pub async fn start_outgoing_pairing(
        &self,
        device_id: &str,
    ) -> Result<PairingSnapshot, CoreError> {
        let operation = self.device_operation(device_id);
        let _operation = operation.lock().await;
        let timestamp = unix_seconds();
        let now = unix_millis();
        let (pairing_id, sender, packet) = {
            let mut state = self
                .state
                .write()
                .map_err(|_| CoreError::StateUnavailable)?;
            let device = state
                .devices
                .get(device_id)
                .ok_or(CoreError::UnknownDevice)?;
            if device.paired {
                return Err(CoreError::AlreadyPaired);
            }
            if state.pairing_by_device.contains_key(device_id) {
                return Err(CoreError::PairingInProgress);
            }
            let connection = state
                .connections
                .get(device_id)
                .cloned()
                .ok_or(CoreError::DeviceNotConnected)?;
            let peer_spki = subject_public_key_info(&connection.certificate_der)
                .map_err(|_| CoreError::InvalidPeerCertificate)?;
            let code = verification_code(&self.local_public_key_der, &peer_spki, timestamp);

            let pairing_id = Uuid::new_v4();
            let snapshot = PairingSnapshot {
                id: pairing_id,
                device_id: device_id.to_owned(),
                device_name: device.device_name.clone(),
                direction: PairingDirection::Outgoing,
                status: PairingStatus::Requested,
                verification_code: Some(code),
                created_at: now,
                expires_at: now.saturating_add(PAIRING_TIMEOUT.as_millis() as u64),
                error_code: None,
            };
            let mut pairing = Pairing::new(snapshot);
            pairing
                .transition(PairingStatus::AwaitingConfirmation, None)
                .expect("requested always allows awaiting_confirmation");

            state
                .devices
                .set_pairing(device_id, true)
                .map_err(|_| CoreError::StateUnavailable)?;
            state
                .pairing_by_device
                .insert(device_id.to_owned(), pairing_id);
            let body = PairingBody {
                pair: true,
                timestamp: Some(timestamp),
                extra: Default::default(),
            };
            let packet = Packet::from_body(now, "kdeconnect.pair", &body)
                .map_err(|_| CoreError::Internal)?;
            state.pairings.insert(
                pairing_id,
                PairingRuntime {
                    pairing,
                    protocol_timestamp: timestamp,
                    timer: None,
                },
            );
            (pairing_id, connection.packets, packet)
        };

        if sender.try_send(packet).is_err() {
            return Ok(self.fail_pairing(pairing_id, OperationErrorCode::ConnectionFailed));
        }
        self.schedule_pairing_timeout(pairing_id);
        let snapshot = self.pairing_snapshot(pairing_id)?;
        self.events
            .publish(EventData::PairingRequested(snapshot.clone()))?;
        Ok(snapshot)
    }

    /// Confirm a locally displayed verification code for an incoming
    /// pairing request. Trust is pinned before the pairing is marked
    /// accepted, and never before this explicit local confirmation.
    pub async fn accept_pairing(&self, pairing_id: Uuid) -> Result<PairingSnapshot, CoreError> {
        let core = self.clone();
        self.mutations
            .spawn(async move { core.accept_pairing_inner(pairing_id).await })
            .await
            .map_err(|_| CoreError::StateUnavailable)?
    }

    async fn accept_pairing_inner(&self, pairing_id: Uuid) -> Result<PairingSnapshot, CoreError> {
        let device_id = self.pairing_snapshot(pairing_id)?.device_id;
        let operation = self.device_operation(&device_id);
        let _operation = operation.lock().await;
        let (device_id, connection) = {
            let state = self.state.read().map_err(|_| CoreError::StateUnavailable)?;
            let runtime = state
                .pairings
                .get(&pairing_id)
                .ok_or(CoreError::UnknownPairing)?;
            let snapshot = runtime.pairing.snapshot();
            if snapshot.direction != PairingDirection::Incoming {
                return Err(CoreError::InvalidPairingDirection);
            }
            if snapshot.status != PairingStatus::AwaitingConfirmation {
                return Err(CoreError::InvalidPairingState);
            }
            let connection = state
                .connections
                .get(&snapshot.device_id)
                .cloned()
                .ok_or(CoreError::DeviceNotConnected)?;
            (snapshot.device_id, connection)
        };

        self.store
            .put_device(&TrustedDevice {
                device_id: device_id.clone(),
                certificate_der: connection.certificate_der.clone(),
                last_trusted_protocol_version: connection.protocol_version,
                last_identity: self.known_identity(&device_id),
            })
            .await
            .map_err(CoreError::Store)?;

        let snapshot = {
            let mut state = self
                .state
                .write()
                .map_err(|_| CoreError::StateUnavailable)?;
            let runtime = state
                .pairings
                .get_mut(&pairing_id)
                .ok_or(CoreError::UnknownPairing)?;
            let snapshot = runtime
                .pairing
                .transition(PairingStatus::Accepted, None)
                .map_err(CoreError::InvalidTransition)?;
            if let Some(timer) = runtime.timer.take() {
                timer.abort();
            }
            state.pairing_by_device.remove(&device_id);
            let _ = state.devices.set_paired(&device_id, true);
            let _ = state.devices.set_pairing(&device_id, false);
            snapshot
        };

        let body = PairingBody {
            pair: true,
            timestamp: None,
            extra: Default::default(),
        };
        if let Ok(packet) = Packet::from_body(unix_millis(), "kdeconnect.pair", &body) {
            let _ = connection.packets.try_send(packet);
        }

        self.events
            .publish(EventData::PairingUpdated(snapshot.clone()))?;
        self.publish_device_update(&device_id);
        self.run_paired_hooks(&device_id).await;
        Ok(snapshot)
    }

    /// Reject an incoming pairing or cancel an outgoing one.
    pub async fn cancel_pairing(&self, pairing_id: Uuid) -> Result<PairingSnapshot, CoreError> {
        let device_id = self.pairing_snapshot(pairing_id)?.device_id;
        let operation = self.device_operation(&device_id);
        let _operation = operation.lock().await;
        let (_device_id, sender, snapshot) = {
            let mut state = self
                .state
                .write()
                .map_err(|_| CoreError::StateUnavailable)?;
            let runtime = state
                .pairings
                .get_mut(&pairing_id)
                .ok_or(CoreError::UnknownPairing)?;
            let status = runtime.pairing.snapshot().status;
            if !matches!(
                status,
                PairingStatus::Requested | PairingStatus::AwaitingConfirmation
            ) {
                return Err(CoreError::InvalidPairingState);
            }
            let device_id = runtime.pairing.snapshot().device_id;
            let snapshot = runtime
                .pairing
                .transition(PairingStatus::Rejected, None)
                .map_err(CoreError::InvalidTransition)?;
            if let Some(timer) = runtime.timer.take() {
                timer.abort();
            }
            state.pairing_by_device.remove(&device_id);
            let _ = state.devices.set_pairing(&device_id, false);
            let sender = state.connections.get(&device_id).map(|c| c.packets.clone());
            (device_id, sender, snapshot)
        };

        if let Some(sender) = sender {
            let body = PairingBody {
                pair: false,
                timestamp: None,
                extra: Default::default(),
            };
            if let Ok(packet) = Packet::from_body(unix_millis(), "kdeconnect.pair", &body) {
                let _ = sender.try_send(packet);
            }
        }
        self.events
            .publish(EventData::PairingUpdated(snapshot.clone()))?;
        Ok(snapshot)
    }

    pub(super) async fn handle_pair_body(
        &self,
        device_id: &str,
        body: PairingBody,
        received_at: i64,
    ) {
        if !body.pair {
            let has_pairing = self
                .state
                .read()
                .ok()
                .is_some_and(|state| state.pairing_by_device.contains_key(device_id));
            if !has_pairing {
                self.handle_peer_unpair(device_id).await;
                return;
            }
            let snapshot = {
                let Ok(mut state) = self.state.write() else {
                    return;
                };
                let Some(&pairing_id) = state.pairing_by_device.get(device_id) else {
                    return;
                };
                let Some(runtime) = state.pairings.get_mut(&pairing_id) else {
                    return;
                };
                let Ok(snapshot) = runtime.pairing.transition(PairingStatus::Rejected, None) else {
                    return;
                };
                if let Some(timer) = runtime.timer.take() {
                    timer.abort();
                }
                state.pairing_by_device.remove(device_id);
                let _ = state.devices.set_pairing(device_id, false);
                snapshot
            };
            let _ = self.events.publish(EventData::PairingUpdated(snapshot));
            return;
        }

        enum Outcome {
            ConfirmOutgoing(Uuid),
            NewIncoming,
            Ignore,
        }

        let outcome = {
            let Ok(state) = self.state.read() else {
                return;
            };
            match state.pairing_by_device.get(device_id) {
                Some(&pairing_id) => {
                    let runtime = state.pairings.get(&pairing_id);
                    match runtime.map(|runtime| runtime.pairing.snapshot()) {
                        Some(snapshot)
                            if snapshot.direction == PairingDirection::Outgoing
                                && snapshot.status == PairingStatus::AwaitingConfirmation =>
                        {
                            Outcome::ConfirmOutgoing(pairing_id)
                        }
                        _ => Outcome::Ignore,
                    }
                }
                None => Outcome::NewIncoming,
            }
        };

        match outcome {
            Outcome::ConfirmOutgoing(pairing_id) => self.confirm_outgoing_pairing(pairing_id).await,
            Outcome::NewIncoming => self.begin_incoming_pairing(device_id, body, received_at),
            Outcome::Ignore => {}
        }
    }

    /// Handle `kdeconnect.pair {"pair": false}` outside a pairing session:
    /// the peer has unpaired us. Remove its trust and mark it unpaired, but
    /// keep the connection open, as KDE Connect does, so the device stays
    /// reachable and can be paired again.
    async fn handle_peer_unpair(&self, device_id: &str) {
        let was_paired = self
            .state
            .read()
            .ok()
            .and_then(|state| state.devices.get(device_id))
            .is_some_and(|device| device.paired);
        let was_trusted = match self.store.remove_device(device_id).await {
            Ok(removed) => removed,
            Err(error) => {
                tracing::warn!(device_id, %error, "could not remove peer trust");
                return;
            }
        };
        if !was_paired && !was_trusted {
            return;
        }
        if let Ok(mut state) = self.state.write() {
            let _ = state.devices.set_paired(device_id, false);
        }
        self.plugins
            .unpaired(&self.plugin_context(), device_id)
            .await;
        self.publish_device_update(device_id);
    }

    async fn confirm_outgoing_pairing(&self, pairing_id: Uuid) {
        let Some((device_id, certificate_der, protocol_version)) = ({
            let Ok(state) = self.state.read() else {
                return;
            };
            state.pairings.get(&pairing_id).map(|runtime| {
                let snapshot = runtime.pairing.snapshot();
                let connection = state.connections.get(&snapshot.device_id);
                (
                    snapshot.device_id,
                    connection
                        .map(|c| c.certificate_der.clone())
                        .unwrap_or_default(),
                    connection.map(|c| c.protocol_version).unwrap_or(0),
                )
            })
        }) else {
            return;
        };
        if certificate_der.is_empty() {
            return;
        }

        if self
            .store
            .put_device(&TrustedDevice {
                device_id: device_id.clone(),
                last_identity: self.known_identity(&device_id),
                certificate_der,
                last_trusted_protocol_version: protocol_version,
            })
            .await
            .is_err()
        {
            let snapshot = self.fail_pairing(pairing_id, OperationErrorCode::Internal);
            let _ = self.events.publish(EventData::PairingUpdated(snapshot));
            return;
        }

        let snapshot = {
            let Ok(mut state) = self.state.write() else {
                return;
            };
            let Some(runtime) = state.pairings.get_mut(&pairing_id) else {
                return;
            };
            let Ok(snapshot) = runtime.pairing.transition(PairingStatus::Accepted, None) else {
                return;
            };
            if let Some(timer) = runtime.timer.take() {
                timer.abort();
            }
            state.pairing_by_device.remove(&device_id);
            let _ = state.devices.set_paired(&device_id, true);
            let _ = state.devices.set_pairing(&device_id, false);
            snapshot
        };
        let _ = self.events.publish(EventData::PairingUpdated(snapshot));
        self.publish_device_update(&device_id);
        self.run_paired_hooks(&device_id).await;
    }

    /// Tell the plugins `device_id` is now paired, as it is connected.
    async fn run_paired_hooks(&self, device_id: &str) {
        self.commit_pending_address(device_id).await;
        if let Some(device) = self.device(device_id) {
            self.plugins.paired(&self.plugin_context(), &device).await;
        }
    }

    fn begin_incoming_pairing(&self, device_id: &str, body: PairingBody, received_at: i64) {
        let Some(timestamp) = body.timestamp else {
            tracing::debug!(device_id, "dropping pair request without a timestamp");
            return;
        };
        // Reject stale requests and implausible clock skew alike, as KDE
        // Connect does ("Device clocks are out of sync").
        if (received_at - timestamp).unsigned_abs() > PAIRING_TIMESTAMP_TOLERANCE_SECS {
            tracing::debug!(
                device_id,
                skew_secs = received_at - timestamp,
                "dropping pair request: device clocks are out of sync"
            );
            return;
        }

        let snapshot = {
            let Ok(mut state) = self.state.write() else {
                return;
            };
            if state.pairing_by_device.contains_key(device_id) {
                return;
            }
            let Some(device) = state.devices.get(device_id) else {
                tracing::debug!(device_id, "dropping pair request from an unknown device");
                return;
            };
            let Some(connection) = state.connections.get(device_id).cloned() else {
                tracing::debug!(device_id, "dropping pair request without a connection");
                return;
            };
            let Ok(peer_spki) = subject_public_key_info(&connection.certificate_der) else {
                return;
            };
            let code = verification_code(&self.local_public_key_der, &peer_spki, timestamp);
            let now = unix_millis();
            let pairing_id = Uuid::new_v4();
            let pairing_snapshot = PairingSnapshot {
                id: pairing_id,
                device_id: device_id.to_owned(),
                device_name: device.device_name.clone(),
                direction: PairingDirection::Incoming,
                status: PairingStatus::Requested,
                verification_code: Some(code),
                created_at: now,
                expires_at: now.saturating_add(PAIRING_TIMEOUT.as_millis() as u64),
                error_code: None,
            };
            let mut pairing = Pairing::new(pairing_snapshot);
            let awaiting_snapshot = pairing
                .transition(PairingStatus::AwaitingConfirmation, None)
                .expect("requested always allows awaiting_confirmation");
            let _ = state.devices.set_pairing(device_id, true);
            state
                .pairing_by_device
                .insert(device_id.to_owned(), pairing_id);
            state.pairings.insert(
                pairing_id,
                PairingRuntime {
                    pairing,
                    protocol_timestamp: timestamp,
                    timer: None,
                },
            );
            (pairing_id, awaiting_snapshot)
        };
        let (pairing_id, pairing_snapshot) = snapshot;
        self.schedule_pairing_timeout(pairing_id);
        let _ = self
            .events
            .publish(EventData::PairingRequested(pairing_snapshot));
    }

    fn fail_pairing(&self, pairing_id: Uuid, error_code: OperationErrorCode) -> PairingSnapshot {
        let outcome = (|| {
            let mut state = self.state.write().ok()?;
            let runtime = state.pairings.get_mut(&pairing_id)?;
            let device_id = runtime.pairing.snapshot().device_id;
            let snapshot = runtime
                .pairing
                .transition(PairingStatus::Failed, Some(error_code))
                .ok()?;
            if let Some(timer) = runtime.timer.take() {
                timer.abort();
            }
            state.pairing_by_device.remove(&device_id);
            let _ = state.devices.set_pairing(&device_id, false);
            Some(snapshot)
        })();
        outcome.unwrap_or_else(|| {
            self.pairing_snapshot(pairing_id)
                .unwrap_or(PairingSnapshot {
                    id: pairing_id,
                    device_id: String::new(),
                    device_name: String::new(),
                    direction: PairingDirection::Outgoing,
                    status: PairingStatus::Failed,
                    verification_code: None,
                    created_at: 0,
                    expires_at: 0,
                    error_code: Some(error_code),
                })
        })
    }

    fn pairing_snapshot(&self, pairing_id: Uuid) -> Result<PairingSnapshot, CoreError> {
        self.state
            .read()
            .map_err(|_| CoreError::StateUnavailable)?
            .pairings
            .get(&pairing_id)
            .map(|runtime| runtime.pairing.snapshot())
            .ok_or(CoreError::UnknownPairing)
    }

    fn schedule_pairing_timeout(&self, pairing_id: Uuid) {
        let handle = self.clone();
        let task = tokio::spawn(async move {
            sleep(PAIRING_TIMEOUT).await;
            handle.expire_pairing(pairing_id).await;
        });
        let Ok(mut state) = self.state.write() else {
            task.abort();
            return;
        };
        match state.pairings.get_mut(&pairing_id) {
            Some(runtime) => runtime.timer = Some(task),
            None => task.abort(),
        }
    }

    async fn expire_pairing(&self, pairing_id: Uuid) {
        let Ok(pairing) = self.pairing_snapshot(pairing_id) else {
            return;
        };
        let operation = self.device_operation(&pairing.device_id);
        let _operation = operation.lock().await;
        let snapshot = {
            let Ok(mut state) = self.state.write() else {
                return;
            };
            let Some(runtime) = state.pairings.get_mut(&pairing_id) else {
                return;
            };
            let status = runtime.pairing.snapshot().status;
            if !matches!(
                status,
                PairingStatus::Requested | PairingStatus::AwaitingConfirmation
            ) {
                return;
            }
            let device_id = runtime.pairing.snapshot().device_id;
            let Ok(snapshot) = runtime
                .pairing
                .transition(PairingStatus::Expired, Some(OperationErrorCode::TimedOut))
            else {
                return;
            };
            // This task is the timer being cleared; nothing left to abort.
            runtime.timer = None;
            state.pairing_by_device.remove(&device_id);
            let _ = state.devices.set_pairing(&device_id, false);
            snapshot
        };
        let _ = self.events.publish(EventData::PairingUpdated(snapshot));
    }
}

pub(super) fn fail_active_pairing(
    state: &mut CoreState,
    device_id: &str,
    error_code: OperationErrorCode,
) -> Option<PairingSnapshot> {
    let pairing_id = state.pairing_by_device.remove(device_id)?;
    let runtime = state.pairings.get_mut(&pairing_id)?;
    let snapshot = runtime
        .pairing
        .transition(PairingStatus::Failed, Some(error_code))
        .ok()?;
    if let Some(timer) = runtime.timer.take() {
        timer.abort();
    }
    let _ = state.devices.set_pairing(device_id, false);
    Some(snapshot)
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        core::testing::{handle, make_identity},
        protocol::Packet,
    };

    fn pairing(status: PairingStatus) -> Pairing {
        Pairing::new(PairingSnapshot {
            id: Uuid::nil(),
            device_id: "740bd4b9b4184ee497d6caf1da8151be".into(),
            device_name: "FOSS Phone".into(),
            direction: PairingDirection::Outgoing,
            status,
            verification_code: Some("ABCDEF12".into()),
            created_at: 100,
            expires_at: 130,
            error_code: None,
        })
    }

    #[test]
    fn pairing_rejects_invalid_transitions() {
        let mut pairing = pairing(PairingStatus::Requested);
        let error = pairing
            .transition(PairingStatus::Accepted, None)
            .unwrap_err();
        assert_eq!(error.current, PairingStatus::Requested);
        assert_eq!(error.next, PairingStatus::Accepted);
        assert_eq!(pairing.snapshot().status, PairingStatus::Requested);

        pairing
            .transition(PairingStatus::AwaitingConfirmation, None)
            .unwrap();
        pairing.transition(PairingStatus::Accepted, None).unwrap();
        assert!(pairing.transition(PairingStatus::Rejected, None).is_err());
    }

    #[tokio::test]
    async fn pairing_requires_a_connected_device() {
        let (handle, _commands) = handle().await;
        assert!(matches!(
            handle.start_outgoing_pairing("missing").await,
            Err(CoreError::UnknownDevice)
        ));
    }

    fn unpair_packet() -> Packet {
        let body = PairingBody {
            pair: false,
            timestamp: None,
            extra: Default::default(),
        };
        Packet::from_body(1, "kdeconnect.pair", &body).unwrap()
    }

    #[tokio::test]
    async fn unpair_from_a_paired_peer_removes_trust_and_keeps_the_connection() {
        let (handle, _commands) = handle().await;
        let device_id = "740bd4b9b4184ee497d6caf1da8151be";
        handle
            .store
            .put_device(&crate::store::testing::trusted_device(device_id))
            .await
            .unwrap();
        handle
            .discover_device(&make_identity(device_id, Vec::new()), true, 1)
            .unwrap();
        let (tx, _rx) = mpsc::channel(4);
        let cancellation = CancellationToken::new();
        handle
            .register_connection(device_id, vec![1, 2, 3], 8, tx, cancellation.clone(), 1)
            .await
            .unwrap();
        let mut events = handle.subscribe();

        handle.handle_peer_packet(device_id, unpair_packet()).await;

        assert!(handle.store.device(device_id).await.unwrap().is_none());
        match events.try_recv().unwrap().event {
            super::EventData::DeviceUpdated(device) => {
                assert!(!device.paired);
                assert_eq!(
                    device.reachability,
                    crate::core::DeviceReachability::Connected
                );
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(!cancellation.is_cancelled());
    }

    #[tokio::test]
    async fn unpair_from_an_unpaired_peer_is_ignored() {
        let (handle, _commands) = handle().await;
        let device_id = "740bd4b9b4184ee497d6caf1da8151be";
        handle
            .discover_device(&make_identity(device_id, Vec::new()), false, 1)
            .unwrap();
        let mut events = handle.subscribe();

        handle.handle_peer_packet(device_id, unpair_packet()).await;

        assert!(events.try_recv().is_err());
    }

    #[test]
    fn pairing_snapshots_use_stable_camel_case_json_names() {
        let pairing_json =
            serde_json::to_value(pairing(PairingStatus::AwaitingConfirmation).snapshot()).unwrap();
        assert_eq!(pairing_json["verificationCode"], "ABCDEF12");
        assert_eq!(pairing_json["createdAt"], 100);
        assert_eq!(pairing_json["expiresAt"], 130);
    }
}
