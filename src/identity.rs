use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey, SECRET_KEY_LENGTH};
use rand_core::OsRng;

const KEY_FILE: &str = "identity.key";
const PUB_FILE: &str = "identity.pub";

pub struct Identity {
    signing: SigningKey,
    verifying: VerifyingKey,
}

impl Identity {
    pub fn load_or_create() -> Result<Self> {
        let dir = data_dir().context("resolve data dir")?;
        Self::load_or_create_in(&dir)
    }

    pub fn load_or_create_in(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let key_path = dir.join(KEY_FILE);
        let signing = if key_path.exists() {
            let bytes =
                fs::read(&key_path).with_context(|| format!("read {}", key_path.display()))?;
            if bytes.len() != SECRET_KEY_LENGTH {
                return Err(anyhow!(
                    "{} has wrong length: {} (expected {})",
                    key_path.display(),
                    bytes.len(),
                    SECRET_KEY_LENGTH
                ));
            }
            let mut seed = [0u8; SECRET_KEY_LENGTH];
            seed.copy_from_slice(&bytes);
            SigningKey::from_bytes(&seed)
        } else {
            let sk = SigningKey::generate(&mut OsRng);
            write_secret(&key_path, sk.as_bytes())
                .with_context(|| format!("write {}", key_path.display()))?;
            let pub_path = dir.join(PUB_FILE);
            fs::write(&pub_path, sk.verifying_key().as_bytes())
                .with_context(|| format!("write {}", pub_path.display()))?;
            sk
        };
        let verifying = signing.verifying_key();
        Ok(Self { signing, verifying })
    }

    pub fn pubkey_bytes(&self) -> [u8; 32] {
        self.verifying.to_bytes()
    }

    pub fn pubkey_hex(&self) -> String {
        hex::encode(self.pubkey_bytes())
    }

    pub fn sign(&self, msg: &[u8]) -> Signature {
        self.signing.sign(msg)
    }
}

pub fn data_dir() -> Result<PathBuf> {
    let base = dirs::data_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("share")))
        .ok_or_else(|| anyhow!("cannot resolve data dir"))?;
    Ok(base.join("cweldrop"))
}

pub fn verify(pubkey_bytes: &[u8; 32], msg: &[u8], sig_bytes: &[u8; 64]) -> bool {
    let Ok(vk) = VerifyingKey::from_bytes(pubkey_bytes) else {
        return false;
    };
    let sig = Signature::from_bytes(sig_bytes);
    vk.verify(msg, &sig).is_ok()
}

#[cfg(unix)]
fn write_secret(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    Ok(())
}

#[cfg(not(unix))]
fn write_secret(path: &Path, bytes: &[u8]) -> io::Result<()> {
    fs::write(path, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_dir() -> PathBuf {
        let nonce: u64 = rand::random();
        let p = std::env::temp_dir().join(format!("cweldrop-idtest-{nonce}"));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn idempotent_load() {
        let dir = fresh_dir();
        let a = Identity::load_or_create_in(&dir).unwrap();
        let b = Identity::load_or_create_in(&dir).unwrap();
        assert_eq!(a.pubkey_hex(), b.pubkey_hex());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sign_verify_roundtrip() {
        let dir = fresh_dir();
        let id = Identity::load_or_create_in(&dir).unwrap();
        let msg = b"hello world";
        let sig = id.sign(msg);
        let pk = id.pubkey_bytes();
        assert!(verify(&pk, msg, &sig.to_bytes()));
        let mut bad = sig.to_bytes();
        bad[0] ^= 1;
        assert!(!verify(&pk, msg, &bad));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
