//! A FIPS build's registry client refuses TLS that a FIPS-approved cipher
//! suite cannot carry.
#![cfg(feature = "fips")]

use std::io;
use std::net::TcpListener;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use ocync_distribution::RegistryClientBuilder;
use rustls::crypto::CryptoProvider;
use rustls::crypto::aws_lc_rs::{self, cipher_suite};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection, SupportedCipherSuite};
use url::Url;

/// Accept one TLS 1.3 connection offering only `suite`. The handle yields
/// whether the handshake completed.
fn serve_once(suite: SupportedCipherSuite) -> (Url, JoinHandle<io::Result<()>>) {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("self-signed certificate");
    let cert = CertificateDer::from(certified.cert.der().to_vec());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        certified.signing_key.serialize_der(),
    ));
    let provider = CryptoProvider {
        cipher_suites: vec![suite],
        ..aws_lc_rs::default_provider()
    };
    let config = ServerConfig::builder_with_provider(Arc::new(provider))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("TLS 1.3 server config")
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .expect("server certificate");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut conn = ServerConnection::new(Arc::new(config)).map_err(io::Error::other)?;
        while conn.is_handshaking() {
            conn.complete_io(&mut stream)?;
        }
        Ok(())
    });
    let url = Url::parse(&format!("https://localhost:{port}")).expect("url");
    (url, handle)
}

async fn ping(url: Url) {
    let client = RegistryClientBuilder::new(url)
        .allow_invalid_certs(true)
        .build()
        .expect("registry client");
    // The servers below close after the handshake, so the request fails
    // either way. The server side says whether TLS was established.
    let _ = client.ping().await;
}

#[tokio::test(flavor = "current_thread")]
async fn handshake_fails_when_the_server_offers_only_chacha20() {
    let (url, server) = serve_once(cipher_suite::TLS13_CHACHA20_POLY1305_SHA256);
    ping(url).await;
    let result = server.join().expect("server thread");
    assert!(
        result.is_err(),
        "a FIPS client negotiated ChaCha20-Poly1305: {result:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn handshake_succeeds_when_the_server_offers_aes_gcm() {
    let (url, server) = serve_once(cipher_suite::TLS13_AES_128_GCM_SHA256);
    ping(url).await;
    server
        .join()
        .expect("server thread")
        .expect("AES-128-GCM handshake");
}
