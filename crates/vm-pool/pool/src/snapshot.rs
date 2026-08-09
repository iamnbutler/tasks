//! Save/restore VM state via Virtualization.framework.
//!
//! Snapshot metadata is persisted to disk as JSON alongside the .vmstate files.
//! The actual save/restore operations require Virtualization.framework integration
//! (via apple/container or direct Swift interop) and are not yet implemented.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::info;
use vm_pool_protocol::VmId;

#[derive(Debug, Error)]
pub enum SnapshotError {
    #[error("snapshot not found: {0}")]
    NotFound(String),
    #[error("save failed: {0}")]
    SaveFailed(String),
    #[error("restore failed: {0}")]
    RestoreFailed(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Snapshot metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotMetadata {
    /// Snapshot name (unique identifier).
    pub name: String,
    /// VM ID this snapshot was taken from.
    pub vm_id: VmId,
    /// Image reference at time of snapshot.
    pub image: String,
    /// Creation timestamp (unix ms).
    pub created_at: u64,
    /// Size in bytes (of the .vmstate file).
    pub size: u64,
}

/// Snapshot storage and management.
pub struct SnapshotStore {
    root: PathBuf,
}

impl SnapshotStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Ensure the snapshot directory exists.
    pub async fn init(&self) -> Result<(), SnapshotError> {
        tokio::fs::create_dir_all(&self.root).await?;
        Ok(())
    }

    fn metadata_path(&self, name: &str) -> PathBuf {
        self.root.join(format!("{}.json", name))
    }

    fn vmstate_path(&self, name: &str) -> PathBuf {
        self.root.join(format!("{}.vmstate", name))
    }

    /// List all snapshots.
    pub async fn list(&self) -> Result<Vec<SnapshotMetadata>, SnapshotError> {
        if !self.root.exists() {
            return Ok(vec![]);
        }

        let mut entries = tokio::fs::read_dir(&self.root).await?;
        let mut snapshots = Vec::new();

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json") {
                let data = tokio::fs::read_to_string(&path).await?;
                if let Ok(meta) = serde_json::from_str::<SnapshotMetadata>(&data) {
                    snapshots.push(meta);
                }
            }
        }

        snapshots.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(snapshots)
    }

    /// Get snapshot metadata by name.
    pub async fn get(&self, name: &str) -> Result<SnapshotMetadata, SnapshotError> {
        let path = self.metadata_path(name);
        if !path.exists() {
            return Err(SnapshotError::NotFound(name.to_string()));
        }
        let data = tokio::fs::read_to_string(&path).await?;
        let meta: SnapshotMetadata = serde_json::from_str(&data)?;
        Ok(meta)
    }

    /// Save snapshot metadata. The actual VM state save requires
    /// Virtualization.framework integration.
    ///
    /// In the full implementation, this would:
    /// 1. Pause the VM
    /// 2. Call saveMachineStateTo with the vmstate path
    /// 3. Store metadata
    pub async fn save(
        &self,
        vm_id: &VmId,
        name: &str,
        image: &str,
    ) -> Result<SnapshotMetadata, SnapshotError> {
        info!(%vm_id, name, "saving snapshot");

        self.init().await?;

        let vmstate_path = self.vmstate_path(name);

        // Check if vmstate file was created by the VM framework
        let size = if vmstate_path.exists() {
            tokio::fs::metadata(&vmstate_path).await?.len()
        } else {
            // In a real implementation, we'd call the Virtualization.framework
            // save API here. For now, we just save metadata.
            0
        };

        let metadata = SnapshotMetadata {
            name: name.to_string(),
            vm_id: vm_id.clone(),
            image: image.to_string(),
            created_at: now_ms(),
            size,
        };

        let json = serde_json::to_string_pretty(&metadata)?;
        tokio::fs::write(self.metadata_path(name), json).await?;

        Ok(metadata)
    }

    /// Restore a VM from a snapshot. Requires Virtualization.framework.
    ///
    /// In the full implementation, this would:
    /// 1. Stop the VM if running
    /// 2. Call restoreMachineState with the vmstate path
    /// 3. Resume the VM
    pub async fn restore(&self, vm_id: &VmId, name: &str) -> Result<PathBuf, SnapshotError> {
        info!(%vm_id, name, "restoring from snapshot");

        // Verify metadata exists
        let _meta = self.get(name).await?;

        let vmstate_path = self.vmstate_path(name);
        if !vmstate_path.exists() {
            return Err(SnapshotError::RestoreFailed(format!(
                "vmstate file not found: {}",
                vmstate_path.display()
            )));
        }

        // In a real implementation, we'd call the Virtualization.framework
        // restore API with this path.
        Ok(vmstate_path)
    }

    /// Delete a snapshot (both metadata and vmstate).
    pub async fn delete(&self, name: &str) -> Result<(), SnapshotError> {
        info!(name, "deleting snapshot");

        let meta_path = self.metadata_path(name);
        if meta_path.exists() {
            tokio::fs::remove_file(&meta_path).await?;
        }

        let vmstate_path = self.vmstate_path(name);
        if vmstate_path.exists() {
            tokio::fs::remove_file(&vmstate_path).await?;
        }

        Ok(())
    }

    /// Check if a snapshot exists.
    pub async fn exists(&self, name: &str) -> bool {
        self.metadata_path(name).exists()
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn init_creates_directory() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path().join("snapshots"));
        store.init().await.unwrap();
        assert!(dir.path().join("snapshots").exists());
    }

    #[tokio::test]
    async fn list_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path());
        let list = store.list().await.unwrap();
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn save_and_get() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path());
        let vm_id = VmId::new("vm-1");

        let meta = store
            .save(&vm_id, "clean-state", "agent:v1.0.0")
            .await
            .unwrap();
        assert_eq!(meta.name, "clean-state");
        assert_eq!(meta.vm_id, vm_id);
        assert_eq!(meta.image, "agent:v1.0.0");

        let loaded = store.get("clean-state").await.unwrap();
        assert_eq!(loaded.name, "clean-state");
        assert_eq!(loaded.vm_id, vm_id);
    }

    #[tokio::test]
    async fn get_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path());
        let result = store.get("nonexistent").await;
        assert!(matches!(result, Err(SnapshotError::NotFound(_))));
    }

    #[tokio::test]
    async fn list_multiple_sorted_by_time() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path());
        let vm_id = VmId::new("vm-1");

        store.save(&vm_id, "snap-a", "agent:v1").await.unwrap();
        // Small delay to ensure different timestamps
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        store.save(&vm_id, "snap-b", "agent:v1").await.unwrap();

        let list = store.list().await.unwrap();
        assert_eq!(list.len(), 2);
        assert!(list[0].created_at <= list[1].created_at);
    }

    #[tokio::test]
    async fn delete_removes_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path());
        let vm_id = VmId::new("vm-1");

        store.save(&vm_id, "to-delete", "agent:v1").await.unwrap();
        assert!(store.exists("to-delete").await);

        store.delete("to-delete").await.unwrap();
        assert!(!store.exists("to-delete").await);
        assert!(store.get("to-delete").await.is_err());
    }

    #[tokio::test]
    async fn delete_removes_vmstate_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path());
        let vm_id = VmId::new("vm-1");

        store.save(&vm_id, "snap", "agent:v1").await.unwrap();

        // Simulate a vmstate file existing
        let vmstate = dir.path().join("snap.vmstate");
        tokio::fs::write(&vmstate, b"fake state").await.unwrap();
        assert!(vmstate.exists());

        store.delete("snap").await.unwrap();
        assert!(!vmstate.exists());
    }

    #[tokio::test]
    async fn restore_requires_vmstate_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path());
        let vm_id = VmId::new("vm-1");

        // Save metadata but no vmstate file
        store.save(&vm_id, "snap", "agent:v1").await.unwrap();

        let result = store.restore(&vm_id, "snap").await;
        assert!(matches!(result, Err(SnapshotError::RestoreFailed(_))));
    }

    #[tokio::test]
    async fn restore_with_vmstate_returns_path() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path());
        let vm_id = VmId::new("vm-1");

        store.save(&vm_id, "snap", "agent:v1").await.unwrap();

        // Create the vmstate file
        let vmstate = dir.path().join("snap.vmstate");
        tokio::fs::write(&vmstate, b"fake state").await.unwrap();

        let path = store.restore(&vm_id, "snap").await.unwrap();
        assert_eq!(path, vmstate);
    }

    #[tokio::test]
    async fn metadata_serde_roundtrip() {
        let meta = SnapshotMetadata {
            name: "clean-state".into(),
            vm_id: VmId::new("vm-abc"),
            image: "agent:v1.0.0".into(),
            created_at: 1234567890,
            size: 1024 * 1024,
        };
        let json = serde_json::to_string(&meta).unwrap();
        let parsed: SnapshotMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, meta);
    }
}
