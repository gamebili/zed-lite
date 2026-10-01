use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use serde::Deserialize;

include!(concat!(env!("OUT_DIR"), "/key_wrap.rs"));

/// Layout of `key.enc`: magic, 12-byte IV, 16-byte GCM tag, then the AES-256-GCM ciphertext of
/// `{"key": ..., "url": ...}`.
const MAGIC: &[u8] = b"DSHK1";
const TAG_LEN: usize = 16;
pub const KEY_FILE_NAME: &str = "key.enc";

#[derive(Clone, Deserialize)]
pub struct BundledCredentials {
    pub key: String,
    pub url: String,
}

// The API key must never reach logs, so only the endpoint is printable.
impl std::fmt::Debug for BundledCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BundledCredentials")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

pub fn load() -> Result<BundledCredentials> {
    let wrap_key = KEY_WRAP.context("this build was compiled without secrets/key-wrap.hex")?;
    let candidates = candidate_paths();
    let path = candidates
        .iter()
        .find(|path| path.is_file())
        .with_context(|| {
            let searched = candidates
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            format!("{KEY_FILE_NAME} not found (searched: {searched})")
        })?;
    let blob = std::fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    decrypt(&blob, &wrap_key).with_context(|| format!("failed to decrypt {}", path.display()))
}

fn candidate_paths() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(executable) = std::env::current_exe()
        && let Some(directory) = executable.parent()
    {
        candidates.push(directory.join(KEY_FILE_NAME));
    }
    // Development builds run out of target/<profile>, so fall back to the checkout's secrets dir.
    candidates.push(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("secrets")
            .join(KEY_FILE_NAME),
    );
    candidates
}

fn decrypt(blob: &[u8], wrap_key: &[u8; 32]) -> Result<BundledCredentials> {
    let Some(payload) = blob.strip_prefix(MAGIC) else {
        bail!("unrecognized key file format");
    };
    if payload.len() < NONCE_LEN + TAG_LEN {
        bail!("key file is truncated");
    }
    let (nonce, rest) = payload.split_at(NONCE_LEN);
    let (tag, ciphertext) = rest.split_at(TAG_LEN);

    let key = LessSafeKey::new(
        UnboundKey::new(&AES_256_GCM, wrap_key).map_err(|_| anyhow!("invalid wrap key"))?,
    );
    let nonce = Nonce::try_assume_unique_for_key(nonce).map_err(|_| anyhow!("invalid nonce"))?;
    // ring expects the tag after the ciphertext, whereas the file stores it in front.
    let mut in_out = Vec::with_capacity(ciphertext.len() + TAG_LEN);
    in_out.extend_from_slice(ciphertext);
    in_out.extend_from_slice(tag);
    let plaintext = key
        .open_in_place(nonce, Aad::empty(), &mut in_out)
        .map_err(|_| anyhow!("authentication failed: wrong wrap key or corrupted file"))?;

    let credentials: BundledCredentials =
        serde_json::from_slice(plaintext).context("decrypted key file is not valid JSON")?;
    let credentials = BundledCredentials {
        key: credentials.key.trim().to_string(),
        url: credentials.url.trim().trim_end_matches('/').to_string(),
    };
    if credentials.key.is_empty() {
        bail!("decrypted key file has an empty key");
    }
    if !credentials.url.starts_with("http://") && !credentials.url.starts_with("https://") {
        bail!("decrypted key file has an invalid url");
    }
    Ok(credentials)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_WRAP_KEY: [u8; 32] = [7; 32];

    fn encrypt(plaintext: &[u8], wrap_key: &[u8; 32]) -> Vec<u8> {
        let nonce_bytes = [3u8; NONCE_LEN];
        let key = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, wrap_key).expect("key"));
        let mut ciphertext = plaintext.to_vec();
        let tag = key
            .seal_in_place_separate_tag(
                Nonce::assume_unique_for_key(nonce_bytes),
                Aad::empty(),
                &mut ciphertext,
            )
            .expect("seal");
        let mut blob = MAGIC.to_vec();
        blob.extend_from_slice(&nonce_bytes);
        blob.extend_from_slice(tag.as_ref());
        blob.extend_from_slice(&ciphertext);
        blob
    }

    #[test]
    fn decrypts_key_file_layout() {
        let blob = encrypt(
            br#"{"key":" sk-test ","url":"https://example.com/v1/"}"#,
            &TEST_WRAP_KEY,
        );
        let credentials = decrypt(&blob, &TEST_WRAP_KEY).expect("decrypt");
        assert_eq!(credentials.key, "sk-test");
        assert_eq!(credentials.url, "https://example.com/v1");
        assert!(!format!("{credentials:?}").contains("sk-test"));
    }

    #[test]
    fn rejects_wrong_wrap_key_and_bad_input() {
        let blob = encrypt(br#"{"key":"k","url":"https://e"}"#, &TEST_WRAP_KEY);
        assert!(decrypt(&blob, &[8; 32]).is_err());
        assert!(decrypt(b"DSHK1short", &TEST_WRAP_KEY).is_err());
        assert!(decrypt(b"nope", &TEST_WRAP_KEY).is_err());

        let blob = encrypt(br#"{"key":"k","url":"ftp://e"}"#, &TEST_WRAP_KEY);
        assert!(decrypt(&blob, &TEST_WRAP_KEY).is_err());
    }
}
