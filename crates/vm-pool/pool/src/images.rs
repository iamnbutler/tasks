//! Image management: types, versioning, local storage.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ImageError {
    #[error("image not found: {0}")]
    NotFound(String),
    #[error("build failed: {0}")]
    BuildFailed(String),
    #[error("invalid image reference: {0}")]
    InvalidRef(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Known image types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageType {
    /// Base image with common tooling.
    Base,
    /// Agent image with Claude Code and dev tools.
    Agent,
    /// Automation image for headless tasks.
    Automation,
}

impl ImageType {
    pub fn name(&self) -> &'static str {
        match self {
            ImageType::Base => "base",
            ImageType::Agent => "agent",
            ImageType::Automation => "automation",
        }
    }

    /// Try to parse an image type from a string.
    pub fn from_name(s: &str) -> Option<Self> {
        match s {
            "base" => Some(ImageType::Base),
            "agent" => Some(ImageType::Agent),
            "automation" => Some(ImageType::Automation),
            _ => None,
        }
    }
}

impl fmt::Display for ImageType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Reference to an image (name + version).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ImageRef {
    /// Image name (e.g., "agent").
    pub name: String,
    /// Version tag (e.g., "v1.2.3") or digest.
    pub version: String,
}

impl ImageRef {
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
        }
    }

    /// Parse from "name:version" format.
    pub fn parse(s: &str) -> Option<Self> {
        let (name, version) = s.split_once(':')?;
        if name.is_empty() || version.is_empty() {
            return None;
        }
        Some(Self::new(name, version))
    }

    /// Infer image type from the name, if it matches a known type.
    pub fn image_type(&self) -> Option<ImageType> {
        ImageType::from_name(&self.name)
    }
}

impl fmt::Display for ImageRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.name, self.version)
    }
}

/// Image metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageMetadata {
    pub image_ref: ImageRef,
    pub image_type: ImageType,
    /// Content digest (sha256).
    pub digest: String,
    /// Size in bytes.
    pub size: u64,
    /// Build timestamp (unix ms).
    pub created_at: u64,
}

/// Image store for managing local images.
pub struct ImageStore {
    root: PathBuf,
}

impl ImageStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn metadata_dir(&self) -> PathBuf {
        self.root.join("metadata")
    }

    fn metadata_path(&self, image_ref: &ImageRef) -> PathBuf {
        self.metadata_dir()
            .join(format!("{}_{}.json", image_ref.name, image_ref.version))
    }

    /// Ensure the storage directories exist.
    pub async fn init(&self) -> Result<(), ImageError> {
        tokio::fs::create_dir_all(self.metadata_dir()).await?;
        Ok(())
    }

    /// List all available images.
    pub async fn list(&self) -> Result<Vec<ImageMetadata>, ImageError> {
        let dir = self.metadata_dir();
        if !dir.exists() {
            return Ok(vec![]);
        }

        let mut entries = tokio::fs::read_dir(&dir).await?;
        let mut images = Vec::new();

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json") {
                let data = tokio::fs::read_to_string(&path).await?;
                if let Ok(meta) = serde_json::from_str::<ImageMetadata>(&data) {
                    images.push(meta);
                }
            }
        }

        images.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(images)
    }

    /// Get image metadata by reference.
    pub async fn get(&self, image_ref: &ImageRef) -> Result<ImageMetadata, ImageError> {
        let path = self.metadata_path(image_ref);
        if !path.exists() {
            return Err(ImageError::NotFound(image_ref.to_string()));
        }
        let data = tokio::fs::read_to_string(&path).await?;
        let meta: ImageMetadata =
            serde_json::from_str(&data).map_err(|e| ImageError::NotFound(e.to_string()))?;
        Ok(meta)
    }

    /// Save image metadata (used after building).
    pub async fn save_metadata(&self, metadata: &ImageMetadata) -> Result<(), ImageError> {
        self.init().await?;
        let path = self.metadata_path(&metadata.image_ref);
        let data = serde_json::to_string_pretty(metadata)
            .map_err(|e| ImageError::Io(std::io::Error::other(e)))?;
        tokio::fs::write(&path, data).await?;
        Ok(())
    }

    /// Build an image from a Dockerfile using `container build`.
    pub async fn build(
        &self,
        image_type: ImageType,
        version: &str,
        dockerfile_dir: &Path,
    ) -> Result<ImageMetadata, ImageError> {
        let image_ref = ImageRef::new(image_type.name(), version);
        let tag = format!("vm-pool-{}:{}", image_type.name(), version);

        let output = tokio::process::Command::new("container")
            .args(["build", "--tag", &tag, "."])
            .current_dir(dockerfile_dir)
            .output()
            .await
            .map_err(|e| ImageError::BuildFailed(format!("failed to run container build: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ImageError::BuildFailed(format!(
                "container build failed: {stderr}"
            )));
        }

        let metadata = ImageMetadata {
            image_ref,
            image_type,
            digest: format!("sha256:{}", hex_digest(&tag)),
            size: 0, // Would need to query the container runtime
            created_at: now_ms(),
        };

        self.save_metadata(&metadata).await?;
        Ok(metadata)
    }

    /// Delete an image.
    pub async fn delete(&self, image_ref: &ImageRef) -> Result<(), ImageError> {
        let path = self.metadata_path(image_ref);
        if path.exists() {
            tokio::fs::remove_file(&path).await?;
        }
        Ok(())
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn hex_digest(input: &str) -> String {
    // Simple hash for tagging — not cryptographic, just for identification.
    // In production, this would come from the container runtime.
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in input.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{:016x}", hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_ref_parse_valid() {
        let r = ImageRef::parse("agent:v1.0.0").unwrap();
        assert_eq!(r.name, "agent");
        assert_eq!(r.version, "v1.0.0");
    }

    #[test]
    fn image_ref_parse_no_colon() {
        assert!(ImageRef::parse("agent").is_none());
    }

    #[test]
    fn image_ref_parse_empty_parts() {
        assert!(ImageRef::parse(":v1").is_none());
        assert!(ImageRef::parse("agent:").is_none());
    }

    #[test]
    fn image_ref_display() {
        let r = ImageRef::new("agent", "v1.0.0");
        assert_eq!(r.to_string(), "agent:v1.0.0");
    }

    #[test]
    fn image_ref_image_type() {
        assert_eq!(
            ImageRef::new("agent", "v1").image_type(),
            Some(ImageType::Agent)
        );
        assert_eq!(
            ImageRef::new("base", "v1").image_type(),
            Some(ImageType::Base)
        );
        assert_eq!(
            ImageRef::new("automation", "v1").image_type(),
            Some(ImageType::Automation)
        );
        assert_eq!(ImageRef::new("custom", "v1").image_type(), None);
    }

    #[test]
    fn image_type_roundtrip() {
        for ty in [ImageType::Base, ImageType::Agent, ImageType::Automation] {
            let name = ty.name();
            let parsed = ImageType::from_name(name).unwrap();
            assert_eq!(parsed, ty);
        }
    }

    #[test]
    fn image_type_display() {
        assert_eq!(ImageType::Agent.to_string(), "agent");
        assert_eq!(ImageType::Base.to_string(), "base");
        assert_eq!(ImageType::Automation.to_string(), "automation");
    }

    #[test]
    fn image_metadata_serde() {
        let meta = ImageMetadata {
            image_ref: ImageRef::new("agent", "v1.0.0"),
            image_type: ImageType::Agent,
            digest: "sha256:abc123".into(),
            size: 1024,
            created_at: 1234567890,
        };
        let json = serde_json::to_string(&meta).unwrap();
        let parsed: ImageMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.image_ref, meta.image_ref);
        assert_eq!(parsed.image_type, meta.image_type);
        assert_eq!(parsed.digest, meta.digest);
    }

    #[tokio::test]
    async fn image_store_init_and_list_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = ImageStore::new(dir.path());
        store.init().await.unwrap();
        let images = store.list().await.unwrap();
        assert!(images.is_empty());
    }

    #[tokio::test]
    async fn image_store_save_and_get() {
        let dir = tempfile::tempdir().unwrap();
        let store = ImageStore::new(dir.path());
        store.init().await.unwrap();

        let meta = ImageMetadata {
            image_ref: ImageRef::new("agent", "v1.0.0"),
            image_type: ImageType::Agent,
            digest: "sha256:abc".into(),
            size: 512,
            created_at: 100,
        };

        store.save_metadata(&meta).await.unwrap();

        let loaded = store.get(&ImageRef::new("agent", "v1.0.0")).await.unwrap();
        assert_eq!(loaded.image_ref, meta.image_ref);
        assert_eq!(loaded.size, 512);
    }

    #[tokio::test]
    async fn image_store_list_multiple() {
        let dir = tempfile::tempdir().unwrap();
        let store = ImageStore::new(dir.path());
        store.init().await.unwrap();

        for (name, version, ts) in [("agent", "v1", 200), ("auto", "v2", 100)] {
            let meta = ImageMetadata {
                image_ref: ImageRef::new(name, version),
                image_type: ImageType::Agent,
                digest: "sha256:x".into(),
                size: 0,
                created_at: ts,
            };
            store.save_metadata(&meta).await.unwrap();
        }

        let list = store.list().await.unwrap();
        assert_eq!(list.len(), 2);
        // Sorted by created_at
        assert_eq!(list[0].created_at, 100);
        assert_eq!(list[1].created_at, 200);
    }

    #[tokio::test]
    async fn image_store_delete() {
        let dir = tempfile::tempdir().unwrap();
        let store = ImageStore::new(dir.path());
        store.init().await.unwrap();

        let image_ref = ImageRef::new("agent", "v1");
        let meta = ImageMetadata {
            image_ref: image_ref.clone(),
            image_type: ImageType::Agent,
            digest: "sha256:x".into(),
            size: 0,
            created_at: 0,
        };
        store.save_metadata(&meta).await.unwrap();
        assert!(store.get(&image_ref).await.is_ok());

        store.delete(&image_ref).await.unwrap();
        assert!(store.get(&image_ref).await.is_err());
    }

    #[tokio::test]
    async fn image_store_get_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let store = ImageStore::new(dir.path());
        store.init().await.unwrap();

        let result = store.get(&ImageRef::new("nope", "v1")).await;
        assert!(matches!(result, Err(ImageError::NotFound(_))));
    }
}
