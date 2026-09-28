//! `kdeconnect.share.request` and `kdeconnect.share.request.update` packet
//! models.
//!
//! `kdeconnect.share.request` announces a single file: its body carries the
//! sanitized `filename` (and optionally `lastModified`), while the envelope's
//! `payloadSize` and `payloadTransferInfo` (a `port` on the same host as the
//! control connection) describe the auxiliary TLS payload stream the receiver
//! must dial to pull the bytes. `kdeconnect.share.request.update` carries
//! `numberOfFiles`/`totalPayloadSize` and is used upstream to announce the
//! total size of a multi-file batch before the individual `share.request`
//! packets; Ferry's MVP only ever transfers one file per transfer
//! resource; the type is modeled and round-trip tested for protocol
//! completeness and future batch support, but it is never sent by this build.
//!
//! The same `kdeconnect.share.request` type also carries shared text or a
//! link, with no payload: a body of just `text` or just `url` (KDE Connect's
//! `SharePlugin::shareText`/`shareUrl` on the desktop, `sendText`/`sendUrls`
//! on Android). A receiver tells them apart by which field is there,
//! checking `filename` first, then `text`, then `url`.
//!
//! Never log a `filename` value, transferred file contents, or shared text
//! or links.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};

use crate::protocol::{BodyError, Packet};

/// Packet type and capability identifier for a single-file share request.
pub const PACKET_TYPE: &str = "kdeconnect.share.request";
/// Packet type for a batch-size update. Modeled but not sent by this build.
pub const UPDATE_PACKET_TYPE: &str = "kdeconnect.share.request.update";

/// Body of a `kdeconnect.share.request` packet.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareRequestBody {
    pub filename: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<i64>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Body of a `kdeconnect.share.request` carrying text rather than a file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareTextBody {
    pub text: String,
}

/// Body of a `kdeconnect.share.request` carrying a link rather than a file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareUrlBody {
    pub url: String,
}

/// Body of a `kdeconnect.share.request.update` packet.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareRequestUpdateBody {
    pub number_of_files: u64,
    pub total_payload_size: u64,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Build the `payloadTransferInfo` envelope map advertising the auxiliary TLS
/// payload port. KDE Connect payload connections are on the same host as the
/// control connection, so only the port varies.
pub fn payload_transfer_info(port: u16) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert("port".into(), Value::from(port));
    map
}

/// Extract the payload port from a received `payloadTransferInfo` map.
pub fn payload_port(info: &Map<String, Value>) -> Option<u16> {
    info.get("port")?.as_u64()?.try_into().ok()
}

/// Build a `kdeconnect.share.request` packet, including the `payloadSize` and
/// `payloadTransferInfo` envelope fields the receiver needs to dial the
/// auxiliary payload connection.
pub fn build_request_packet(
    id: impl Into<Number>,
    filename: String,
    last_modified: Option<i64>,
    payload_size: u64,
    payload_port: u16,
) -> Result<Packet, BodyError> {
    let mut packet = Packet::from_body(
        id,
        PACKET_TYPE,
        &ShareRequestBody {
            filename,
            last_modified,
            extra: Map::new(),
        },
    )?;
    // Sizes beyond i64::MAX are not realistic for a file transfer; saturate
    // rather than fail to encode a share request over an implausible size.
    packet.payload_size = Some(payload_size.try_into().unwrap_or(i64::MAX));
    packet.payload_transfer_info = Some(payload_transfer_info(payload_port));
    Ok(packet)
}

/// Build a `kdeconnect.share.request` sharing `text`, with no payload.
pub fn build_text_packet(id: impl Into<Number>, text: String) -> Result<Packet, BodyError> {
    Packet::from_body(id, PACKET_TYPE, &ShareTextBody { text })
}

/// Build a `kdeconnect.share.request` sharing a link, with no payload.
pub fn build_url_packet(id: impl Into<Number>, url: String) -> Result<Packet, BodyError> {
    Packet::from_body(id, PACKET_TYPE, &ShareUrlBody { url })
}

/// Build a `kdeconnect.share.request.update` packet. Not sent by this build's
/// single-file transfer path; provided for protocol completeness.
pub fn build_update_packet(
    id: impl Into<Number>,
    number_of_files: u64,
    total_payload_size: u64,
) -> Result<Packet, BodyError> {
    Packet::from_body(
        id,
        UPDATE_PACKET_TYPE,
        &ShareRequestUpdateBody {
            number_of_files,
            total_payload_size,
            extra: Map::new(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_packet_round_trips_filename_and_payload_envelope() {
        let packet = build_request_packet(
            1_u64,
            "photo.jpg".into(),
            Some(1_700_000_000_000),
            4096,
            1741,
        )
        .unwrap();
        assert_eq!(packet.packet_type, PACKET_TYPE);
        assert_eq!(packet.payload_size, Some(4096));
        let info = packet.payload_transfer_info.clone().unwrap();
        assert_eq!(payload_port(&info), Some(1741));

        let body: ShareRequestBody = packet.body_as().unwrap();
        assert_eq!(body.filename, "photo.jpg");
        assert_eq!(body.last_modified, Some(1_700_000_000_000));
    }

    #[test]
    fn request_packet_omits_last_modified_when_absent() {
        let packet = build_request_packet(1_u64, "notes.txt".into(), None, 0, 1716).unwrap();
        assert!(!packet.body.contains_key("lastModified"));
    }

    #[test]
    fn text_and_url_packets_carry_only_their_field_and_no_payload() {
        let text = build_text_packet(1_u64, "hello".into()).unwrap();
        assert_eq!(text.packet_type, PACKET_TYPE);
        assert_eq!(
            serde_json::Value::Object(text.body.clone()),
            serde_json::json!({"text": "hello"})
        );
        assert_eq!(text.payload_size, None);
        assert_eq!(text.payload_transfer_info, None);

        let url = build_url_packet(1_u64, "https://kde.org".into()).unwrap();
        assert_eq!(url.packet_type, PACKET_TYPE);
        assert_eq!(
            serde_json::Value::Object(url.body.clone()),
            serde_json::json!({"url": "https://kde.org"})
        );
        assert_eq!(url.payload_size, None);
    }

    #[test]
    fn update_packet_round_trips_counts() {
        let packet = build_update_packet(1_u64, 3, 12_345).unwrap();
        assert_eq!(packet.packet_type, UPDATE_PACKET_TYPE);
        let body: ShareRequestUpdateBody = packet.body_as().unwrap();
        assert_eq!(body.number_of_files, 3);
        assert_eq!(body.total_payload_size, 12_345);
    }

    #[test]
    fn unknown_body_fields_survive_round_trip() {
        let mut packet = build_request_packet(1_u64, "a.bin".into(), None, 1, 1716).unwrap();
        packet
            .body
            .insert("creationTime".into(), serde_json::json!(1234));
        let body: ShareRequestBody = packet.body_as().unwrap();
        assert_eq!(body.extra["creationTime"], 1234);
    }
}
