//! Workspace-bound authenticated encryption for provider secrets. The key is
//! the device key file `vault.key`, unless an installed extension holds it
//! ([`crate::extension::Extension::vault_key`]).
use crate::db::params;
use crate::{
    Error, Result, id, now,
    store::{Store, one, s},
    string,
};
use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use rand::{RngCore, rngs::OsRng};
use serde_json::{Value, json};
use std::io::{Read, Write};

fn local_vault_key(path: &std::path::Path) -> Result<Option<[u8; 32]>> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() != 32 {
        return Err(Error::bad("Invalid local vault key"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(Error::bad("Vault key permissions must be owner-only"));
        }
    }
    let mut key = [0u8; 32];
    file.read_exact(&mut key)?;
    Ok(Some(key))
}

impl Store {
    fn vault_key(&self) -> Result<[u8; 32]> {
        // An extension that holds the key answers for it, errors included.
        if let Some(key) = crate::extension::installed().vault_key(self) {
            return key;
        }
        let path = self.directory.join("vault.key");
        if let Some(key) = local_vault_key(&path)? {
            return Ok(key);
        }
        let mut temporary = tempfile::NamedTempFile::new_in(&*self.directory)?;
        let mut new_key = [0u8; 32];
        OsRng.fill_bytes(&mut new_key);
        temporary.write_all(&new_key)?;
        temporary.as_file().sync_all()?;
        match temporary.persist_noclobber(&path) {
            Ok(_) => Ok(new_key),
            Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
                local_vault_key(&path)?
                    .ok_or_else(|| Error::from(std::io::Error::from(std::io::ErrorKind::NotFound)))
            }
            Err(e) => Err(e.error.into()),
        }
    }
    pub fn save_secret(&self, workspace: &str, name: &str, value: &str) -> Result<String> {
        if value.is_empty() || value.len() > 32768 {
            return Err(Error::bad("Credential must contain 1–32768 bytes"));
        }
        let identifier = id();
        let key = self.vault_key()?;
        let cipher =
            Aes256Gcm::new_from_slice(&key).map_err(|_| Error::bad("Vault key invalid"))?;
        let mut nonce = [0u8; 12];
        OsRng.fill_bytes(&mut nonce);
        // Frozen AEAD associated data: stored credentials are sealed with it, so
        // changing it makes them undecryptable. `secret` opens with the same value.
        let aad = format!("Graphicle/v1/{workspace}/{identifier}/{name}");
        let ciphertext = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: value.as_bytes(),
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| Error::bad("Credential encryption failed"))?;
        self.transaction(|db| {
            db.execute(
                "INSERT INTO am_secrets VALUES(?,?,?,?,?,?)",
                params![
                    identifier,
                    workspace,
                    name,
                    ciphertext,
                    nonce.as_slice(),
                    now()
                ],
            )?;
            Ok(())
        })?;
        Ok(identifier)
    }
    pub fn secret(&self, workspace: &str, identifier: &str) -> Result<String> {
        let db = self.connect()?;
        let mut stmt = db.prepare(
            "SELECT name,ciphertext,nonce FROM am_secrets WHERE workspace_id=? AND id=?",
        )?;
        let mut rows = stmt.query(params![workspace, identifier])?;
        let row = rows
            .next()?
            .ok_or_else(|| Error::bad("Stored credential is unavailable"))?;
        let name: String = row.get(0)?;
        let bytes: Vec<u8> = row.get(1)?;
        let nonce: Vec<u8> = row.get(2)?;
        if nonce.len() != 12 {
            return Err(Error::bad("Credential integrity check failed"));
        }
        let cipher = Aes256Gcm::new_from_slice(&self.vault_key()?)
            .map_err(|_| Error::bad("Vault key invalid"))?;
        // Frozen AEAD associated data: the value the credential was sealed with
        // above; changing it makes stored credentials undecryptable.
        let aad = format!("Graphicle/v1/{workspace}/{identifier}/{name}");
        let plain = cipher
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &bytes,
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| Error::bad("Credential integrity check failed"))?;
        String::from_utf8(plain).map_err(|_| Error::bad("Credential encoding invalid"))
    }
    pub fn put_credential(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        self.authorized(workspace, token, "admin", |_, _| Ok(()))?;
        let identifier = self.save_secret(
            workspace,
            string(args, "name", 100)?,
            string(args, "value", 32768)?,
        )?;
        // Revalidate after writing; a concurrently revoked caller cannot use the result.
        if self
            .authorized(workspace, token, "admin", |_, _| Ok(()))
            .is_err()
        {
            self.connect()?
                .execute("DELETE FROM am_secrets WHERE id=?", [&identifier])?;
            return Err(Error::denied());
        }
        Ok(json!({"id":identifier,"credential_ref":format!("secret:{identifier}"),"stored":true}))
    }
    pub fn list_credentials(&self, workspace: &str, token: &str) -> Result<Value> {
        self.authorized(workspace,token,"admin",|db,_|Ok(json!(crate::store::rows(db,"SELECT id,name,updated_at FROM am_secrets WHERE workspace_id=? AND name NOT LIKE 'oauth-verifier:%' ORDER BY name",[workspace])?)))
    }
    pub fn remove_credential(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        self.authorized(workspace,token,"admin",|db,_|{
            let reference=string(args,"credential_ref",100)?;let identifier=reference.strip_prefix("secret:").ok_or_else(||Error::bad("Expected stored credential reference"))?;
            let projects=crate::store::rows(db,"SELECT DISTINCT repository_id FROM am_connectors WHERE repository_id IN (SELECT id FROM repo_projects WHERE workspace_id=?) AND (credential_env=? OR credential_env IN (SELECT 'oauth:'||id FROM am_oauth_accounts WHERE workspace_id=? AND token_secret=?))",params![workspace,reference,workspace,identifier])?;
            db.execute("UPDATE am_connectors SET state='paused',last_sync=NULL,generation=generation+1 WHERE repository_id IN (SELECT id FROM repo_projects WHERE workspace_id=?) AND credential_env IN (SELECT 'oauth:'||id FROM am_oauth_accounts WHERE workspace_id=? AND token_secret=?)",params![workspace,workspace,identifier])?;
            let n=db.execute("DELETE FROM am_secrets WHERE workspace_id=? AND id=?",params![workspace,identifier])?;
            // A removed secret immediately stops every connector referencing it.
            db.execute("UPDATE am_connectors SET state='paused',last_sync=NULL,generation=generation+1 WHERE repository_id IN (SELECT id FROM repo_projects WHERE workspace_id=?) AND credential_env=?",params![workspace,reference])?;
            for project in projects{db.execute("DELETE FROM am_model_cache WHERE repository_id=?",[s(&project,"repository_id")])?;crate::store::invalidate(db,s(&project,"repository_id"))?;}
            Ok(json!({"removed":n>0}))
        })
    }
    pub fn resolve_credential(&self, workspace: &str, reference: &str) -> Result<String> {
        if let Some(identifier) = reference.strip_prefix("secret:") {
            return self.secret(workspace, identifier);
        }
        if let Some(identifier) = reference.strip_prefix("oauth:") {
            let account = one(
                &self.connect()?,
                "SELECT * FROM am_oauth_accounts WHERE workspace_id=? AND id=? AND state='active'",
                params![workspace, identifier],
            )?
            .ok_or_else(|| Error::bad("Connected account is unavailable"))?;
            if account["expires_at"]
                .as_str()
                .is_some_and(|t| t < now().as_str())
            {
                return Err(Error::bad(
                    "Connected account token expired; refresh the account",
                ));
            }
            return self.secret(workspace, s(&account, "token_secret"));
        }
        if reference.is_empty()
            || reference.len() > 100
            || !reference
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_')
        {
            return Err(Error::bad("Invalid credential reference"));
        }
        std::env::var(reference)
            .ok()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| Error::bad("Credential is unavailable; add it in Connections"))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{
        collections::BTreeSet,
        ffi::OsString,
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
    };

    struct PermissionGuard {
        path: PathBuf,
        permissions: fs::Permissions,
    }

    impl PermissionGuard {
        fn make_read_only(path: &Path) -> Self {
            let permissions = fs::metadata(path).unwrap().permissions();
            fs::set_permissions(path, fs::Permissions::from_mode(0o500)).unwrap();
            Self {
                path: path.to_owned(),
                permissions,
            }
        }
    }

    impl Drop for PermissionGuard {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.path, self.permissions.clone());
        }
    }

    fn entries(path: &Path) -> BTreeSet<OsString> {
        fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect()
    }

    #[test]
    fn an_existing_vault_key_is_read_from_a_directory_that_cannot_be_written() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let workspace = store.create_workspace("Read-only vault").unwrap();
        let workspace = workspace["id"].as_str().unwrap();
        store
            .save_secret(workspace, "model key", "saved value")
            .unwrap();
        let saved = fs::read(directory.path().join("vault.key")).unwrap();
        let before = entries(directory.path());

        // Check that an existing vault.key is read without creating anything in
        // a data folder that cannot be written, as on read-only media served by
        // `--read-only`. This calls `vault_key` directly; it does not use
        // `Store::secret` or toggle read-only mode.
        let _permissions = PermissionGuard::make_read_only(directory.path());
        let error = fs::File::create(directory.path().join("write-probe"))
            .expect_err("the directory must be unwritable for this test; running as root?");
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::PermissionDenied,
            "the directory must be unwritable for this test; running as root?"
        );
        assert_eq!(store.vault_key().unwrap().as_slice(), saved.as_slice());
        assert_eq!(entries(directory.path()), before);
    }
}
