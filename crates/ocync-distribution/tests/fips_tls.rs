//! A FIPS build's registry client refuses TLS that only non-approved
//! algorithms can carry.
#![cfg(feature = "fips")]

use std::io;
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ocync_distribution::RegistryClientBuilder;
use rustls::crypto::CryptoProvider;
use rustls::crypto::aws_lc_rs::{self, cipher_suite, kx_group};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::version::{TLS12, TLS13};
use rustls::{PeerIncompatible, ServerConfig, ServerConnection, SupportedProtocolVersion};
use url::Url;

/// Bounds every wait on the client, so a client that never connects fails
/// the test instead of hanging it.
const DEADLINE: Duration = Duration::from_secs(10);

/// Accept one connection with `provider` and `version`. The handle yields
/// the server's handshake result.
fn serve_once(
    provider: CryptoProvider,
    version: &'static SupportedProtocolVersion,
) -> (Url, JoinHandle<Result<(), rustls::Error>>) {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("self-signed certificate");
    let cert = CertificateDer::from(certified.cert.der().to_vec());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        certified.signing_key.serialize_der(),
    ));
    let config = ServerConfig::builder_with_provider(Arc::new(provider))
        .with_protocol_versions(&[version])
        .expect("server protocol versions")
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .expect("server certificate");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    let handle = thread::spawn(move || {
        let mut stream = accept_before_deadline(&listener)?;
        let mut conn = ServerConnection::new(Arc::new(config))?;
        while conn.is_handshaking() {
            conn.complete_io(&mut stream).map_err(rustls_error)?;
        }
        Ok(())
    });
    let url = Url::parse(&format!("https://127.0.0.1:{port}")).expect("url");
    (url, handle)
}

fn accept_before_deadline(listener: &TcpListener) -> Result<TcpStream, rustls::Error> {
    listener.set_nonblocking(true).map_err(rustls_error)?;
    let start = Instant::now();
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).map_err(rustls_error)?;
                stream
                    .set_read_timeout(Some(DEADLINE))
                    .map_err(rustls_error)?;
                stream
                    .set_write_timeout(Some(DEADLINE))
                    .map_err(rustls_error)?;
                return Ok(stream);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock && start.elapsed() < DEADLINE => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(rustls_error(e)),
        }
    }
}

/// Recover the TLS error `complete_io` wraps, keeping plain I/O errors
/// distinguishable from a protocol refusal.
fn rustls_error(e: io::Error) -> rustls::Error {
    let message = e.to_string();
    e.into_inner()
        .and_then(|inner| inner.downcast::<rustls::Error>().ok())
        .map_or(rustls::Error::General(message), |inner| *inner)
}

/// Drive the production registry client at `url`. The servers close after
/// the handshake, so the request fails either way; the server side says
/// whether TLS was established.
async fn connect(url: Url) {
    let client = RegistryClientBuilder::new(url)
        .allow_invalid_certs(true)
        .build()
        .expect("registry client");
    let _ = client.ping().await;
}

async fn handshake(
    provider: CryptoProvider,
    version: &'static SupportedProtocolVersion,
) -> Result<(), rustls::Error> {
    let (url, server) = serve_once(provider, version);
    connect(url).await;
    server.join().expect("server thread")
}

fn offering_suite(suite: rustls::SupportedCipherSuite) -> CryptoProvider {
    CryptoProvider {
        cipher_suites: vec![suite],
        ..aws_lc_rs::default_provider()
    }
}

#[tokio::test(flavor = "current_thread")]
async fn refuses_a_server_offering_only_tls13_chacha20() {
    let result = handshake(
        offering_suite(cipher_suite::TLS13_CHACHA20_POLY1305_SHA256),
        &TLS13,
    )
    .await;
    assert_eq!(
        result,
        Err(rustls::Error::PeerIncompatible(
            PeerIncompatible::NoCipherSuitesInCommon
        ))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn refuses_a_server_offering_only_tls12_chacha20() {
    let result = handshake(
        offering_suite(cipher_suite::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256),
        &TLS12,
    )
    .await;
    assert_eq!(
        result,
        Err(rustls::Error::PeerIncompatible(
            PeerIncompatible::NoCipherSuitesInCommon
        ))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn refuses_a_server_offering_only_x25519() {
    let provider = CryptoProvider {
        kx_groups: vec![kx_group::X25519],
        ..offering_suite(cipher_suite::TLS13_AES_128_GCM_SHA256)
    };
    let result = handshake(provider, &TLS13).await;
    assert_eq!(
        result,
        Err(rustls::Error::PeerIncompatible(
            PeerIncompatible::NoKxGroupsInCommon
        ))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn accepts_a_server_offering_aes_gcm() {
    let result = handshake(
        offering_suite(cipher_suite::TLS13_AES_128_GCM_SHA256),
        &TLS13,
    )
    .await;
    assert_eq!(result, Ok(()));
}
