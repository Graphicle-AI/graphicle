//! Signed release discovery and bounded, verified installer downloads.
//! The trusted public key is supplied separately from the feed; nothing is executed.
//! Each installer in a feed names its edition ("core" or "pro"); a program
//! only takes the installer of its own edition, so an update never swaps one
//! edition for the other (both install as the same application).
use crate::{
    Error, Result, digest, model,
    store::{Store, one, s},
    string,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use rusqlite::params;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{io::Write, path::Path};

fn version(value: &str) -> Result<(u64, u64, u64)> {
    let parts = value
        .split('.')
        .map(str::parse::<u64>)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|_| Error::bad("Release version must be major.minor.patch"))?;
    if parts.len() != 3 {
        return Err(Error::bad("Invalid release version"));
    }
    Ok((parts[0], parts[1], parts[2]))
}
pub fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}
/// The edition an installer entry is built for. Feeds published before the
/// editions were split list no edition; their installers were the commercial build.
fn artifact_edition(artifact: &Value) -> &str {
    artifact["edition"].as_str().unwrap_or("pro")
}
pub fn verify(
    envelope: &Value,
    public_key: &str,
    target: &str,
    edition: &str,
    current: &str,
    now: i64,
) -> Result<Value> {
    let key = STANDARD
        .decode(public_key.trim())
        .map_err(|_| Error::bad("Invalid Ed25519 public key"))?;
    if key.len() != 32 {
        return Err(Error::bad("Expected a 32-byte Ed25519 public key"));
    }
    let payload = STANDARD
        .decode(string(envelope, "payload", 512 * 1024)?)
        .map_err(|_| Error::bad("Invalid release payload"))?;
    let sig = STANDARD
        .decode(string(envelope, "signature", 200)?)
        .map_err(|_| Error::bad("Invalid release signature"))?;
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &key)
        .verify(&payload, &sig)
        .map_err(|_| Error::bad("Release signature does not match the trusted publisher key"))?;
    let release: Value = serde_json::from_slice(&payload)?;
    if release["schema"] != 1
        || release["channel"] != "stable"
        || release["sequence"]
            .as_u64()
            .is_none_or(|n| n == 0 || n > i64::MAX as u64)
    {
        return Err(Error::bad("Unsupported release manifest"));
    }
    let published = release["published_at"]
        .as_i64()
        .ok_or_else(|| Error::bad("Missing publication time"))?;
    let expires = release["expires_at"]
        .as_i64()
        .ok_or_else(|| Error::bad("Missing release expiry"))?;
    if published > now.saturating_add(300)
        || expires <= now
        || expires <= published
        || expires.saturating_sub(published) > 90 * 86400
    {
        return Err(Error::bad(
            "Release manifest is expired or has invalid validity dates",
        ));
    }
    let next = string(&release, "version", 40)?;
    let next_version = version(next)?;
    let current_version = version(current)?;
    if next_version < current_version {
        return Err(Error::bad(
            "Publisher feed offers an older version; refusing downgrade",
        ));
    }
    let artifacts = release["artifacts"]
        .as_array()
        .filter(|v| !v.is_empty() && v.len() <= 20)
        .ok_or_else(|| Error::bad("Invalid release artifact list"))?;
    let matches = artifacts
        .iter()
        .filter(|v| v["platform"] == target && artifact_edition(v) == edition)
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return Err(Error::bad(
            "Release must contain exactly one installer for this platform and edition",
        ));
    }
    let artifact = matches[0];
    let name = string(artifact, "name", 160)?;
    if name.starts_with('.')
        || name.contains("..")
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err(Error::bad("Invalid installer filename"));
    }
    let url = model::checked_url(string(artifact, "url", 4000)?)?;
    if url.scheme() != "https" && !cfg!(test) {
        return Err(Error::bad("Installer downloads require HTTPS"));
    }
    if artifact["bytes"]
        .as_u64()
        .is_none_or(|n| n == 0 || n > 1024 * 1024 * 1024)
    {
        return Err(Error::bad("Installer exceeds the 1 GiB bound"));
    }
    let hash = string(artifact, "sha256", 64)?;
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(Error::bad("Invalid installer SHA-256"));
    }
    Ok(
        json!({"version":next,"current_version":current,"edition":edition,"update_available":next_version>current_version,"sequence":release["sequence"],"expires_at":expires,"artifact":artifact,"manifest_sha256":graphicle_memory::text::sha256(&payload),"publisher_key_id":graphicle_memory::text::sha256(&key),"signature_verified":true}),
    )
}
fn remember(store: &Store, feed: &str, value: &Value) -> Result<()> {
    store.transaction(|db| {
        db.execute_batch("CREATE TABLE IF NOT EXISTS am_release_seen(feed_key TEXT PRIMARY KEY,sequence INTEGER NOT NULL,manifest_sha256 TEXT NOT NULL)")?;
        let key=digest(&json!([feed,value["publisher_key_id"]]));let sequence=value["sequence"].as_i64().ok_or_else(||Error::bad("Invalid release sequence"))?;
        if let Some(old)=one(db,"SELECT sequence,manifest_sha256 FROM am_release_seen WHERE feed_key=?",[&key])?
            && (old["sequence"].as_i64().unwrap()>sequence || (old["sequence"]==sequence && old["manifest_sha256"]!=value["manifest_sha256"])){return Err(Error::conflict("Release feed moved backwards or reused its sequence with different content"));}
        db.execute("INSERT INTO am_release_seen VALUES(?,?,?) ON CONFLICT(feed_key) DO UPDATE SET sequence=excluded.sequence,manifest_sha256=excluded.manifest_sha256",params![key,sequence,s(value,"manifest_sha256")])?;Ok(())
    })
}
pub async fn check(
    store: &Store,
    client: &reqwest::Client,
    feed: &str,
    public_key: &str,
) -> Result<Value> {
    let url = model::checked_url(feed)?;
    if url.scheme() != "https" && !cfg!(test) {
        return Err(Error::bad("Release feeds require HTTPS"));
    }
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|_| Error::bad("Cannot reach release feed"))?;
    let envelope = model::bounded_json(response).await?;
    let value = verify(
        &envelope,
        public_key,
        &platform(),
        crate::core_capabilities::edition(),
        env!("CARGO_PKG_VERSION"),
        chrono::Utc::now().timestamp(),
    )?;
    remember(store, feed, &value)?;
    Ok(value)
}
pub async fn download(
    store: &Store,
    client: &reqwest::Client,
    feed: &str,
    public_key: &str,
    output: &Path,
) -> Result<Value> {
    let mut value = check(store, client, feed, public_key).await?;
    if value["update_available"] != true {
        return Err(Error::bad("This version is already current"));
    }
    std::fs::create_dir_all(output)?;
    let destination = output.join(string(&value["artifact"], "name", 160)?);
    if destination.exists() || destination.is_symlink() {
        return Err(Error::conflict(
            "Installer already exists; it was not overwritten",
        ));
    }
    let mut temporary = tempfile::NamedTempFile::new_in(output)?;
    let mut response = client
        .get(string(&value["artifact"], "url", 4000)?)
        .send()
        .await
        .map_err(|_| Error::bad("Installer download failed"))?;
    if !response.status().is_success() {
        return Err(Error::bad(format!(
            "Installer returned HTTP {}",
            response.status().as_u16()
        )));
    }
    let expected = value["artifact"]["bytes"].as_u64().unwrap();
    let mut bytes = 0u64;
    let mut sha = Sha256::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| Error::bad("Installer download was interrupted"))?
    {
        bytes = bytes.saturating_add(chunk.len() as u64);
        if bytes > expected {
            return Err(Error::bad("Installer exceeds its signed size"));
        }
        sha.update(&chunk);
        temporary.write_all(&chunk)?;
    }
    if bytes != expected || format!("{:x}", sha.finalize()) != s(&value["artifact"], "sha256") {
        return Err(Error::bad(
            "Installer does not match its signed size and hash",
        ));
    }
    // Recheck the monotonic marker after the network operation.
    remember(store, feed, &value)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist_noclobber(&destination)
        .map_err(|_| Error::conflict("Installer destination changed during download"))?;
    value["downloaded_to"] = json!(destination);
    value["executed"] = json!(false);
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{Ed25519KeyPair, KeyPair};
    fn envelope(value: Value, key: &Ed25519KeyPair) -> Value {
        let payload = serde_json::to_vec(&value).unwrap();
        json!({"payload":STANDARD.encode(&payload),"signature":STANDARD.encode(key.sign(&payload))})
    }
    fn fixture() -> (Ed25519KeyPair, Value) {
        let key = Ed25519KeyPair::from_seed_unchecked(&[7u8; 32]).unwrap();
        let value = json!({"schema":1,"channel":"stable","version":"0.4.1","sequence":1,"published_at":1000,"expires_at":3000,"artifacts":[{"platform":"linux-aarch64","edition":"core","name":"Graphicle-0.4.1.tar.gz","url":"https://downloads.example.test/Graphicle-0.4.1.tar.gz","sha256":"ab".repeat(32),"bytes":100}]});
        (key, value)
    }
    #[test]
    fn signatures_versions_expiry_and_filename_are_enforced() {
        let (key, mut value) = fixture();
        let pk = STANDARD.encode(key.public_key());
        let valid = verify(
            &envelope(value.clone(), &key),
            &pk,
            "linux-aarch64",
            "core",
            "0.4.0",
            2000,
        )
        .unwrap();
        assert_eq!(valid["signature_verified"], true);
        let wrong = Ed25519KeyPair::from_seed_unchecked(&[8u8; 32]).unwrap();
        assert!(
            verify(
                &envelope(value.clone(), &wrong),
                &pk,
                "linux-aarch64",
                "core",
                "0.4.0",
                2000
            )
            .is_err()
        );
        assert!(
            verify(
                &envelope(value.clone(), &key),
                &pk,
                "linux-aarch64",
                "core",
                "0.4.0",
                4000
            )
            .is_err()
        );
        assert!(
            verify(
                &envelope(value.clone(), &key),
                &pk,
                "linux-aarch64",
                "core",
                "0.5.0",
                2000
            )
            .is_err()
        );
        value["artifacts"][0]["name"] = json!("../escape");
        assert!(
            verify(
                &envelope(value, &key),
                &pk,
                "linux-aarch64",
                "core",
                "0.4.0",
                2000
            )
            .is_err()
        );
    }
    #[test]
    fn each_edition_takes_only_its_own_installer() {
        let (key, mut value) = fixture();
        let pk = STANDARD.encode(key.public_key());
        let core = value["artifacts"][0].clone();
        let mut pro = core.clone();
        pro["edition"] = json!("pro");
        pro["name"] = json!("Graphicle-Pro-0.4.1.tar.gz");
        pro["url"] = json!("https://downloads.example.test/Graphicle-Pro-0.4.1.tar.gz");
        value["artifacts"] = json!([core.clone(), pro]);
        for (edition, name) in [
            ("core", "Graphicle-0.4.1.tar.gz"),
            ("pro", "Graphicle-Pro-0.4.1.tar.gz"),
        ] {
            let valid = verify(
                &envelope(value.clone(), &key),
                &pk,
                "linux-aarch64",
                edition,
                "0.4.0",
                2000,
            )
            .unwrap();
            assert_eq!(valid["edition"], edition);
            assert_eq!(valid["artifact"]["name"], name);
        }
        // A feed published before the split lists the commercial build without an
        // edition: Core never takes it.
        let mut legacy = core.clone();
        legacy.as_object_mut().unwrap().remove("edition");
        value["artifacts"] = json!([legacy]);
        let feed = envelope(value.clone(), &key);
        assert!(verify(&feed, &pk, "linux-aarch64", "core", "0.4.0", 2000).is_err());
        assert_eq!(
            verify(&feed, &pk, "linux-aarch64", "pro", "0.4.0", 2000).unwrap()["artifact"]["name"],
            "Graphicle-0.4.1.tar.gz"
        );
        // Two installers of one edition for one platform are ambiguous.
        let mut second = core.clone();
        second["name"] = json!("Graphicle-0.4.1-other.tar.gz");
        value["artifacts"] = json!([core, second]);
        assert!(
            verify(
                &envelope(value, &key),
                &pk,
                "linux-aarch64",
                "core",
                "0.4.0",
                2000
            )
            .is_err()
        );
    }
    #[test]
    fn rollback_and_reused_sequences_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let (key, value) = fixture();
        let pk = STANDARD.encode(key.public_key());
        let valid = verify(
            &envelope(value, &key),
            &pk,
            "linux-aarch64",
            "core",
            "0.4.0",
            2000,
        )
        .unwrap();
        remember(&store, "https://example.test", &valid).unwrap();
        let mut changed = valid.clone();
        changed["manifest_sha256"] = json!("different");
        assert!(remember(&store, "https://example.test", &changed).is_err());
        changed["sequence"] = json!(0);
        assert!(remember(&store, "https://example.test", &changed).is_err());
    }
    #[tokio::test]
    async fn download_verifies_bytes_and_never_overwrites_an_existing_file() {
        use axum::{Json, Router, routing::get};
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("store")).unwrap();
        let (key, mut value) = fixture();
        let public = STANDARD.encode(key.public_key());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let bytes = b"public fixture installer";
        let now = chrono::Utc::now().timestamp();
        value["published_at"] = json!(now);
        value["expires_at"] = json!(now + 3600);
        value["version"] = json!("99.0.0");
        value["artifacts"][0] = json!({"platform":platform(),"edition":"core","name":"fixture-installer.bin","url":format!("{base}/installer"),"bytes":bytes.len(),"sha256":graphicle_memory::text::sha256(bytes)});
        let envelope = envelope(value, &key);
        let app = Router::new()
            .route(
                "/feed",
                get(move || {
                    let v = envelope.clone();
                    async move { Json(v) }
                }),
            )
            .route("/installer", get(|| async { "public fixture installer" }));
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let output = directory.path().join("downloads");
        let feed = format!("{base}/feed");
        let client = model::client().unwrap();
        let result = download(&store, &client, &feed, &public, &output)
            .await
            .unwrap();
        assert_eq!(result["executed"], false);
        assert_eq!(
            std::fs::read(output.join("fixture-installer.bin")).unwrap(),
            bytes
        );
        assert!(
            download(&store, &client, &feed, &public, &output)
                .await
                .is_err()
        );
        task.abort();
    }
}
