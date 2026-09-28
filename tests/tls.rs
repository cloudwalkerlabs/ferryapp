//! Direct tests of the protocol-v8 TLS transport: real handshake-signature
//! verification, certificate pinning, and identity binding.

use std::time::Duration;

use ferry::{
    config::LocalIdentity,
    store::Store,
    transport::tls::{self, PeerPin, TlsMaterial, subject_public_key_info},
};
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};
use tokio::{net::TcpListener, time::timeout};

async fn identity() -> LocalIdentity {
    LocalIdentity::load_or_create(&Store::open_in_memory().await.unwrap())
        .await
        .unwrap()
}

async fn loopback_pair() -> (tokio::net::TcpStream, tokio::net::TcpStream) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let connect = tokio::net::TcpStream::connect(addr);
    let accept = listener.accept();
    let (client, accepted) = tokio::join!(connect, accept);
    (client.unwrap(), accepted.unwrap().0)
}

#[tokio::test]
async fn valid_peers_complete_a_real_mutually_authenticated_handshake() {
    let a = identity().await;
    let b = identity().await;
    let a_material = TlsMaterial::new(a.certificate_der(), a.private_key_der());
    let b_material = TlsMaterial::new(b.certificate_der(), b.private_key_der());
    let (client_stream, server_stream) = loopback_pair().await;

    let (client_result, server_result) = tokio::join!(
        tls::connect(client_stream, &a_material, b.device_id(), PeerPin::Unpinned),
        tls::accept(server_stream, &b_material, a.device_id(), PeerPin::Unpinned),
    );

    let client_stream = client_result.expect("client handshake succeeds");
    let server_stream = server_result.expect("server handshake succeeds");
    assert_eq!(
        tls::client_peer_certificate(&client_stream).unwrap(),
        b.certificate_der()
    );
    assert_eq!(
        tls::server_peer_certificate(&server_stream).unwrap(),
        a.certificate_der()
    );
}

#[tokio::test]
async fn pinned_certificate_change_is_rejected() {
    let a = identity().await;
    let b = identity().await;
    let impostor = identity().await; // different key and certificate, same role
    let a_material = TlsMaterial::new(a.certificate_der(), a.private_key_der());
    let impostor_material =
        TlsMaterial::new(impostor.certificate_der(), impostor.private_key_der());
    let (client_stream, server_stream) = loopback_pair().await;

    // The client pins the certificate it originally trusted for `b`, but the
    // server now presents the impostor's certificate under the same role.
    let (client_result, _server_result) = tokio::join!(
        tls::connect(
            client_stream,
            &a_material,
            b.device_id(),
            PeerPin::Pinned(b.certificate_der().to_vec()),
        ),
        tls::accept(
            server_stream,
            &impostor_material,
            a.device_id(),
            PeerPin::Unpinned
        ),
    );

    assert!(
        client_result.is_err(),
        "a changed certificate for a pinned device must fail closed"
    );
}

#[tokio::test]
async fn certificate_not_matching_the_expected_device_id_is_rejected() {
    let a = identity().await;
    let b = identity().await;
    let (client_stream, server_stream) = loopback_pair().await;
    let a_material = TlsMaterial::new(a.certificate_der(), a.private_key_der());
    let b_material = TlsMaterial::new(b.certificate_der(), b.private_key_der());

    // The client expects to be talking to some unrelated device ID, not `b`.
    let (client_result, _server_result) = tokio::join!(
        tls::connect(
            client_stream,
            &a_material,
            "00000000000000000000000000000000",
            PeerPin::Unpinned,
        ),
        tls::accept(server_stream, &b_material, a.device_id(), PeerPin::Unpinned),
    );

    assert!(client_result.is_err());
}

#[tokio::test]
async fn signature_from_a_mismatched_private_key_is_rejected() {
    // Never trust a verifier that skips signature verification: build TLS
    // material whose certificate and private key do not correspond to the
    // same key pair. A correct verifier must reject the handshake because
    // the handshake signature cannot validate against the certificate's
    // public key, even though the certificate's Common Name is exactly
    // right and would otherwise be trusted.
    let legitimate = identity().await;
    let other_key_source = identity().await;
    let mismatched_material = TlsMaterial::new(
        legitimate.certificate_der(),
        other_key_source.private_key_der(),
    );
    let peer = identity().await;
    let peer_material = TlsMaterial::new(peer.certificate_der(), peer.private_key_der());
    let (client_stream, server_stream) = loopback_pair().await;

    let (client_result, server_result) = tokio::join!(
        tls::connect(
            client_stream,
            &mismatched_material,
            peer.device_id(),
            PeerPin::Unpinned,
        ),
        tls::accept(
            server_stream,
            &peer_material,
            legitimate.device_id(),
            PeerPin::Unpinned,
        ),
    );

    assert!(
        client_result.is_err() || server_result.is_err(),
        "a handshake signed with the wrong private key must never succeed"
    );
}

#[tokio::test]
async fn arbitrary_certificates_without_a_pin_still_require_a_matching_common_name() {
    // Unpinned (trust-on-first-use for pairing) handshakes must still bind
    // the presented certificate to the device ID claimed before TLS, not
    // accept literally anything.
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::default();
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, "not-the-expected-device-id");
    let certificate = params.self_signed(&key).unwrap();
    let forged_material = TlsMaterial::new(certificate.der(), &key.serialize_der());
    let peer = identity().await;
    let peer_material = TlsMaterial::new(peer.certificate_der(), peer.private_key_der());
    let (client_stream, server_stream) = loopback_pair().await;

    let (client_result, server_result) = tokio::join!(
        tls::connect(
            client_stream,
            &forged_material,
            peer.device_id(),
            PeerPin::Unpinned
        ),
        tls::accept(
            server_stream,
            &peer_material,
            "expected-but-different-id",
            PeerPin::Unpinned,
        ),
    );

    assert!(
        client_result.is_err() || server_result.is_err(),
        "a client certificate whose Common Name does not match the claimed device ID must be rejected"
    );
}

#[tokio::test]
async fn subject_public_key_info_round_trips_for_a_real_certificate() {
    let device = identity().await;
    let spki = subject_public_key_info(device.certificate_der()).unwrap();
    assert!(!spki.is_empty());
    // Re-parsing the same certificate must be deterministic.
    assert_eq!(
        spki,
        subject_public_key_info(device.certificate_der()).unwrap()
    );
}

#[tokio::test]
async fn handshake_does_not_hang_when_peer_never_speaks_tls() {
    let identity = identity().await;
    let material = TlsMaterial::new(identity.certificate_der(), identity.private_key_der());
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let plain_client = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        use tokio::io::AsyncWriteExt;
        let _ = stream.write_all(b"not tls\n").await;
    });
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let result = timeout(
        Duration::from_secs(2),
        tls::connect(
            stream,
            &material,
            "00000000000000000000000000000000",
            PeerPin::Unpinned,
        ),
    )
    .await;
    let _ = plain_client.await;
    assert!(matches!(result, Ok(Err(_))));
}
