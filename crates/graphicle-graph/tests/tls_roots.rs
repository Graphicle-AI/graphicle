//! The HTTPS clients of `graphicle::net` trust the operating system's
//! certificate authorities, which `SSL_CERT_FILE` and `SSL_CERT_DIR` replace
//! when set, and a refused certificate fails with the causes and a hint.
//!
//! An HTTPS server on 127.0.0.1 presents a certificate from a certificate
//! authority made for this test, which neither the bundled roots nor the
//! system's hold. The test sets environment variables, so this file holds a
//! single test: its binary runs nothing else at the same time.
//!
//! On Windows with GRAPHICLE_TEST_ROOT_STORE=windows (the Windows CI job), the
//! test also adds its CA to the computer's Trusted Root store (certutil, which
//! needs an administrator), connects without SSL_CERT_FILE and removes the CA
//! again.
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::Arc;

use graphicle::net;

const BODY: &str = "{\"ok\":true}";

struct Authority {
    pem: String,
    der: Vec<u8>,
    server: rustls::ServerConfig,
}

fn authority() -> Authority {
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "Graphicle test CA");
    params.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::CrlSign,
    ];
    let ca =
        rcgen::CertifiedIssuer::self_signed(params, rcgen::KeyPair::generate().unwrap()).unwrap();
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params =
        rcgen::CertificateParams::new(vec!["127.0.0.1".to_string(), "localhost".to_string()])
            .unwrap();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "127.0.0.1");
    let certificate = params.signed_by(&key, &ca).unwrap();
    let server = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![certificate.der().clone()],
        rustls::pki_types::PrivateKeyDer::Pkcs8(key.serialize_der().into()),
    )
    .unwrap();
    Authority {
        pem: pem(ca.der()),
        der: ca.der().to_vec(),
        server,
    }
}

/// A certificate in PEM.
fn pem(der: &[u8]) -> String {
    use base64::Engine;
    let text = base64::engine::general_purpose::STANDARD.encode(der);
    let lines: Vec<&str> = text
        .as_bytes()
        .chunks(64)
        .map(|line| std::str::from_utf8(line).unwrap())
        .collect();
    format!(
        "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
        lines.join("\n")
    )
}

/// An HTTPS server on 127.0.0.1 that answers every request with [`BODY`];
/// its address.
fn serve(config: rustls::ServerConfig) -> String {
    let config = Arc::new(config);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let config = config.clone();
            std::thread::spawn(move || {
                let Ok(connection) = rustls::ServerConnection::new(config) else {
                    return;
                };
                let mut tls = rustls::StreamOwned::new(connection, stream);
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    match tls.read(&mut buffer) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => request.extend_from_slice(&buffer[..n]),
                    }
                }
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{BODY}",
                    BODY.len()
                );
                let _ = tls.write_all(reply.as_bytes());
                tls.conn.send_close_notify();
                let _ = tls.flush();
            });
        }
    });
    format!("https://127.0.0.1:{port}/v1/models")
}

/// Sets or removes SSL_CERT_FILE and SSL_CERT_DIR.
fn roots_from(file: Option<&Path>, dir: Option<&Path>) {
    // SAFETY: this test binary runs one test, and no other thread reads the
    // environment while it changes.
    unsafe {
        match file {
            Some(file) => std::env::set_var("SSL_CERT_FILE", file),
            None => std::env::remove_var("SSL_CERT_FILE"),
        }
        match dir {
            Some(dir) => std::env::set_var("SSL_CERT_DIR", dir),
            None => std::env::remove_var("SSL_CERT_DIR"),
        }
    }
}

/// The body from `url` with both clients, or the error text: the blocking
/// client's [`net::describe`], and the async client's causes with the hint.
fn fetch(url: &str) -> Result<String, String> {
    let blocking = net::blocking_client(|builder| builder.no_proxy())
        .expect("the blocking client builds")
        .get(url)
        .send()
        .and_then(|response| response.error_for_status()?.text())
        .map_err(net::describe);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let asynchronous = runtime.block_on(async {
        let client = net::client(|builder| builder.no_proxy()).expect("the async client builds");
        match client.get(url).send().await {
            Ok(response) => response.text().await.map_err(|error| error.to_string()),
            Err(error) => Err(net::with_hint(net::causes(&error))),
        }
    });
    match (blocking, asynchronous) {
        (Ok(a), Ok(b)) => {
            assert_eq!(a, b);
            Ok(a)
        }
        (Err(a), Err(b)) => {
            assert!(net::certificate_refused(&b), "async client: {b}");
            Err(a)
        }
        (a, b) => panic!("the clients disagree: blocking {a:?}, async {b:?}"),
    }
}

fn assert_refused(result: Result<String, String>, case: &str) {
    let error = result.expect_err(case);
    assert!(
        error.contains("invalid peer certificate: UnknownIssuer"),
        "{case}: {error}"
    );
    assert!(error.ends_with(net::CERTIFICATE_HINT), "{case}: {error}");
}

#[test]
fn https_clients_trust_the_system_roots_and_ssl_cert_file() {
    let authority = authority();
    let url = serve(authority.server);
    let folder = tempfile::tempdir().unwrap();
    let ca = folder.path().join("ca.pem");
    std::fs::write(&ca, &authority.pem).unwrap();
    let not_a_certificate = folder.path().join("broken.pem");
    std::fs::write(
        &not_a_certificate,
        "-----BEGIN CERTIFICATE-----\nMAA=\n-----END CERTIFICATE-----\n",
    )
    .unwrap();
    let directory = folder.path().join("certs");
    std::fs::create_dir(&directory).unwrap();
    std::fs::write(directory.join("ca.pem"), &authority.pem).unwrap();

    // Neither the bundled roots nor the system's hold the test CA.
    roots_from(None, None);
    assert_refused(fetch(&url), "without SSL_CERT_FILE");

    // SSL_CERT_FILE or SSL_CERT_DIR naming the CA: trusted.
    roots_from(Some(&ca), None);
    assert_eq!(fetch(&url).as_deref(), Ok(BODY));
    roots_from(None, Some(&directory));
    assert_eq!(fetch(&url).as_deref(), Ok(BODY));

    // A file with no usable certificate, which reqwest refuses to build a
    // client with: the clients still build, with the bundled roots alone.
    roots_from(Some(&not_a_certificate), None);
    assert!(reqwest::blocking::Client::builder().build().is_err());
    assert_refused(fetch(&url), "with a broken SSL_CERT_FILE");
    roots_from(None, None);

    if cfg!(windows) && std::env::var("GRAPHICLE_TEST_ROOT_STORE").as_deref() == Ok("windows") {
        windows_root_store(&url, &authority.der, folder.path());
    }
}

/// The test CA in the computer's Trusted Root store: trusted without
/// SSL_CERT_FILE. The CA is removed again, also when the check fails.
fn windows_root_store(url: &str, der: &[u8], folder: &Path) {
    use sha1::Digest;
    let file = folder.join("ca.cer");
    std::fs::write(&file, der).unwrap();
    let thumbprint: String = sha1::Sha1::digest(der)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let certutil = |args: &[&std::ffi::OsStr]| {
        let status = std::process::Command::new("certutil")
            .args(args)
            .status()
            .expect("certutil runs");
        assert!(status.success(), "certutil {args:?}: {status}");
    };
    struct Remove<F: Fn()>(F);
    impl<F: Fn()> Drop for Remove<F> {
        fn drop(&mut self) {
            (self.0)();
        }
    }
    certutil(&[
        "-f".as_ref(),
        "-addstore".as_ref(),
        "Root".as_ref(),
        file.as_os_str(),
    ]);
    let _remove = Remove(|| {
        let _ = std::process::Command::new("certutil")
            .args(["-delstore", "Root", &thumbprint])
            .status();
    });
    assert_eq!(fetch(url).as_deref(), Ok(BODY));
}
