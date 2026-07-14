//! Optional firmware registry: maps the sha256 of a received SB2.1 image to a
//! firmware version, using the host project's `firmware-registry.json` schema.
//!
//! Caveat: the vendor app zeroes a 2-byte marker at `size − 4` before putting
//! the image on the wire, so the sha256 of what the virtual bootloader
//! receives usually does NOT match the registry's embedded-image hashes. The
//! lookup is still useful for images extracted straight from a capture (wire
//! images) or user-maintained maps; otherwise `--post-flash-version` applies.

use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Deserialize)]
struct RegistryFile {
    releases: Vec<Release>,
}

#[derive(Debug, Deserialize)]
struct Release {
    #[serde(default)]
    firmware_version: Option<String>,
    #[serde(default)]
    images: Vec<Image>,
}

#[derive(Debug, Deserialize)]
struct Image {
    sha256: String,
}

/// sha256 (lowercase hex) → firmware version.
#[derive(Debug, Clone, Default)]
pub struct FirmwareRegistry {
    by_sha: HashMap<String, u32>,
}

impl FirmwareRegistry {
    pub fn load(path: &Path) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        let parsed: RegistryFile = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
        let mut by_sha = HashMap::new();
        for rel in parsed.releases {
            let Some(version) = rel.firmware_version.as_deref().and_then(|v| v.parse().ok()) else {
                continue;
            };
            for img in rel.images {
                by_sha.insert(img.sha256.to_lowercase(), version);
            }
        }
        Ok(Self { by_sha })
    }

    pub fn len(&self) -> usize {
        self.by_sha.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_sha.is_empty()
    }

    /// Look up an image by content.
    pub fn version_of(&self, image: &[u8]) -> Option<u32> {
        let sha = Sha256::digest(image);
        let hex: String = sha.iter().map(|b| format!("{b:02x}")).collect();
        self.by_sha.get(&hex).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest;

    /// Parse the host project's firmware-registry.json schema and resolve an
    /// image by sha256.
    #[test]
    fn parses_registry_schema_and_resolves() {
        let image = b"fake sb2 image".to_vec();
        let sha: String = Sha256::digest(&image)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let json = format!(
            r#"{{
              "schema": "qa40x-firmware-registry/1",
              "releases": [
                {{"app_version": "1.223", "firmware_version": "60",
                  "images": [{{"device": "QA402", "size": 14, "sha256": "{sha}"}}]}},
                {{"app_version": "0.983", "firmware_version": null, "images": []}}
              ]
            }}"#
        );
        let dir = std::env::temp_dir().join("vqa40x-registry-test.json");
        std::fs::write(&dir, json).unwrap();
        let reg = FirmwareRegistry::load(&dir).unwrap();
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.version_of(&image), Some(60));
        assert_eq!(reg.version_of(b"other"), None);
        let _ = std::fs::remove_file(&dir);
    }
}
