use anyhow::{ensure, Context, Result};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub const REPOSITORY: &str = "danieldunderfelt/splice";
pub const MAX_ARCHIVE: u64 = 256 * 1024 * 1024;
pub const MAX_MANIFEST: usize = 64 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Asset {
    pub name: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema: u16,
    pub version: String,
    pub commit: String,
    pub protocol: u16,
    pub assets: BTreeMap<String, Asset>,
}

pub fn version(value: &str) -> Result<semver::Version> {
    let version = semver::Version::parse(value).context("invalid release version")?;
    ensure!(
        version.pre.is_empty() && version.build.is_empty(),
        "only stable release versions are supported"
    );
    Ok(version)
}

pub fn verify(bytes: &[u8], signature: &[u8], key: &[u8; 32]) -> Result<Manifest> {
    ensure!(
        bytes.len() <= MAX_MANIFEST,
        "release manifest exceeds its size limit"
    );
    VerifyingKey::from_bytes(key)?
        .verify_strict(bytes, &Signature::from_slice(signature)?)
        .context("release signature is invalid")?;
    let manifest: Manifest = serde_json::from_slice(bytes)?;
    ensure!(manifest.schema == 1, "unsupported update manifest schema");
    version(&manifest.version)?;
    ensure!(
        manifest.commit.len() == 40 && manifest.commit.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid release commit"
    );
    ensure!(
        manifest.protocol > 0 && !manifest.assets.is_empty(),
        "incomplete release manifest"
    );
    for (target, asset) in &manifest.assets {
        ensure!(
            asset.name == format!("splice-{target}.tar.gz"),
            "unexpected release asset name"
        );
        ensure!(
            target
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "invalid release target"
        );
        ensure!(
            (1..=MAX_ARCHIVE).contains(&asset.size),
            "invalid release asset size"
        );
        ensure!(
            hex::decode(&asset.sha256)?.len() == 32,
            "invalid release asset checksum"
        );
    }
    Ok(manifest)
}

pub fn verify_archive(bytes: &[u8], asset: &Asset) -> Result<()> {
    ensure!(
        bytes.len() as u64 == asset.size,
        "downloaded archive size does not match its signed manifest"
    );
    ensure!(
        hex::encode(Sha256::digest(bytes)) == asset.sha256,
        "downloaded archive checksum does not match its signed manifest"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn fixture() -> (Vec<u8>, SigningKey) {
        let manifest = Manifest {
            schema: 1,
            version: "1.2.0".into(),
            commit: "a".repeat(40),
            protocol: 3,
            assets: BTreeMap::from([(
                "x86_64-unknown-linux-gnu".into(),
                Asset {
                    name: "splice-x86_64-unknown-linux-gnu.tar.gz".into(),
                    sha256: "ab".repeat(32),
                    size: 42,
                },
            )]),
        };
        (
            serde_json::to_vec(&manifest).unwrap(),
            SigningKey::from_bytes(&[7; 32]),
        )
    }

    #[test]
    fn signed_manifest_rejects_tampering_and_wrong_keys() {
        let (mut bytes, key) = fixture();
        let signature = key.sign(&bytes).to_bytes();
        assert_eq!(
            verify(&bytes, &signature, key.verifying_key().as_bytes())
                .unwrap()
                .version,
            "1.2.0"
        );
        assert!(verify(
            &bytes,
            &signature,
            SigningKey::from_bytes(&[8; 32]).verifying_key().as_bytes()
        )
        .is_err());
        bytes[0] ^= 1;
        assert!(verify(&bytes, &signature, key.verifying_key().as_bytes()).is_err());
    }

    #[test]
    fn signed_manifest_still_rejects_unsafe_names_and_unknown_schema() {
        let (bytes, key) = fixture();
        for (field, value) in [
            ("schema", serde_json::json!(2)),
            ("version", serde_json::json!("../../evil")),
        ] {
            let mut manifest: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            manifest[field] = value;
            let bytes = serde_json::to_vec(&manifest).unwrap();
            assert!(verify(
                &bytes,
                &key.sign(&bytes).to_bytes(),
                key.verifying_key().as_bytes()
            )
            .is_err());
        }
    }

    #[test]
    fn archive_requires_both_exact_size_and_digest() {
        let asset = Asset {
            name: "test".into(),
            size: 3,
            sha256: hex::encode(Sha256::digest(b"abc")),
        };
        assert!(verify_archive(b"abc", &asset).is_ok());
        assert!(verify_archive(b"abd", &asset).is_err());
        assert!(verify_archive(b"ab", &asset).is_err());
    }
}
