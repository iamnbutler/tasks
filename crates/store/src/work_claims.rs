//! Work claims persistence — tracks claimed work items.
//!
//! Part of the centralized work queue (#658). The queue itself is derived
//! from source systems (GitHub issues, etc.); this table persists claim
//! status so restarts don't re-dispatch in-progress work.

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};

use crate::StoreError;

/// A persisted work claim record.
#[derive(Debug, Clone)]
pub struct WorkClaim {
    pub work_id: String,
    pub work_type: String,
    pub source_id: String,
    pub project_id: String,
    pub container_id: Option<String>,
    pub claimed_at: Option<DateTime<Utc>>,
    pub released_at: Option<DateTime<Utc>>,
    pub release_note: Option<String>,
    pub completed_at: Option<DateTime<Utc>>,
}

impl WorkClaim {
    fn from_row(row: &rusqlite::Row) -> rusqlite::Result<Self> {
        Ok(Self {
            work_id: row.get("work_id")?,
            work_type: row.get("work_type")?,
            source_id: row.get("source_id")?,
            project_id: row.get("project_id")?,
            container_id: row.get("container_id")?,
            claimed_at: row
                .get::<_, Option<String>>("claimed_at")?
                .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                .map(|dt| dt.with_timezone(&Utc)),
            released_at: row
                .get::<_, Option<String>>("released_at")?
                .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                .map(|dt| dt.with_timezone(&Utc)),
            release_note: row.get("release_note")?,
            completed_at: row
                .get::<_, Option<String>>("completed_at")?
                .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                .map(|dt| dt.with_timezone(&Utc)),
        })
    }
}

/// Insert or update a work claim (upsert).
pub fn upsert_claim(
    conn: &Connection,
    work_id: &str,
    work_type: &str,
    source_id: &str,
    project_id: &str,
    container_id: &str,
) -> Result<(), StoreError> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO work_claims (work_id, work_type, source_id, project_id, container_id, claimed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(work_id) DO UPDATE SET
             container_id = excluded.container_id,
             claimed_at = excluded.claimed_at,
             released_at = NULL,
             release_note = NULL",
        params![work_id, work_type, source_id, project_id, container_id, now],
    )?;
    Ok(())
}

/// Get a work claim by ID.
pub fn get_claim(conn: &Connection, work_id: &str) -> Result<Option<WorkClaim>, StoreError> {
    let claim = conn
        .query_row(
            "SELECT * FROM work_claims WHERE work_id = ?1",
            params![work_id],
            WorkClaim::from_row,
        )
        .optional()?;
    Ok(claim)
}

/// Get all active claims (claimed but not completed).
pub fn get_active_claims(conn: &Connection) -> Result<Vec<WorkClaim>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT * FROM work_claims
         WHERE container_id IS NOT NULL AND completed_at IS NULL",
    )?;
    let claims = stmt
        .query_map([], WorkClaim::from_row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(claims)
}

/// Release a claim (container gave up the work).
pub fn release_claim(
    conn: &Connection,
    work_id: &str,
    note: Option<&str>,
) -> Result<(), StoreError> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE work_claims SET
            container_id = NULL,
            released_at = ?2,
            release_note = ?3
         WHERE work_id = ?1",
        params![work_id, now, note],
    )?;
    Ok(())
}

/// Mark a claim as completed.
pub fn complete_claim(conn: &Connection, work_id: &str) -> Result<(), StoreError> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE work_claims SET completed_at = ?2 WHERE work_id = ?1",
        params![work_id, now],
    )?;
    Ok(())
}

/// Check if a source_id has an active (uncompleted) claim.
pub fn has_active_claim_for_source(
    conn: &Connection,
    source_id: &str,
) -> Result<bool, StoreError> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM work_claims
         WHERE source_id = ?1 AND completed_at IS NULL AND container_id IS NOT NULL",
        params![source_id],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// Delete completed claims older than the given timestamp.
pub fn cleanup_old_claims(
    conn: &Connection,
    before: DateTime<Utc>,
) -> Result<usize, StoreError> {
    let before_str = before.to_rfc3339();
    let deleted = conn.execute(
        "DELETE FROM work_claims WHERE completed_at IS NOT NULL AND completed_at < ?1",
        params![before_str],
    )?;
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::schema::initialize(&conn).unwrap();
        conn
    }

    #[test]
    fn upsert_and_get_claim() {
        let conn = setup_db();

        upsert_claim(&conn, "work-1", "task", "issue-123", "proj-1", "container-a").unwrap();

        let claim = get_claim(&conn, "work-1").unwrap().unwrap();
        assert_eq!(claim.work_id, "work-1");
        assert_eq!(claim.work_type, "task");
        assert_eq!(claim.source_id, "issue-123");
        assert_eq!(claim.project_id, "proj-1");
        assert_eq!(claim.container_id, Some("container-a".to_string()));
        assert!(claim.claimed_at.is_some());
        assert!(claim.released_at.is_none());
        assert!(claim.completed_at.is_none());
    }

    #[test]
    fn upsert_updates_existing_claim() {
        let conn = setup_db();

        // Create initial claim
        upsert_claim(&conn, "work-1", "task", "issue-123", "proj-1", "container-a").unwrap();

        // Upsert with new container
        upsert_claim(&conn, "work-1", "task", "issue-123", "proj-1", "container-b").unwrap();

        let claim = get_claim(&conn, "work-1").unwrap().unwrap();
        assert_eq!(claim.container_id, Some("container-b".to_string()));
    }

    #[test]
    fn get_nonexistent_claim() {
        let conn = setup_db();
        let claim = get_claim(&conn, "nonexistent").unwrap();
        assert!(claim.is_none());
    }

    #[test]
    fn release_claim_clears_container() {
        let conn = setup_db();

        upsert_claim(&conn, "work-1", "task", "issue-123", "proj-1", "container-a").unwrap();
        release_claim(&conn, "work-1", Some("agent crashed")).unwrap();

        let claim = get_claim(&conn, "work-1").unwrap().unwrap();
        assert!(claim.container_id.is_none());
        assert!(claim.released_at.is_some());
        assert_eq!(claim.release_note, Some("agent crashed".to_string()));
    }

    #[test]
    fn complete_claim_sets_timestamp() {
        let conn = setup_db();

        upsert_claim(&conn, "work-1", "task", "issue-123", "proj-1", "container-a").unwrap();
        complete_claim(&conn, "work-1").unwrap();

        let claim = get_claim(&conn, "work-1").unwrap().unwrap();
        assert!(claim.completed_at.is_some());
    }

    #[test]
    fn get_active_claims_filters_correctly() {
        let conn = setup_db();

        // Active claim
        upsert_claim(&conn, "work-1", "task", "issue-1", "proj-1", "container-a").unwrap();

        // Completed claim
        upsert_claim(&conn, "work-2", "task", "issue-2", "proj-1", "container-b").unwrap();
        complete_claim(&conn, "work-2").unwrap();

        // Released claim (no container)
        upsert_claim(&conn, "work-3", "task", "issue-3", "proj-1", "container-c").unwrap();
        release_claim(&conn, "work-3", None).unwrap();

        let active = get_active_claims(&conn).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].work_id, "work-1");
    }

    #[test]
    fn has_active_claim_for_source_checks_correctly() {
        let conn = setup_db();

        // No claims yet
        assert!(!has_active_claim_for_source(&conn, "issue-123").unwrap());

        // Create active claim
        upsert_claim(&conn, "work-1", "task", "issue-123", "proj-1", "container-a").unwrap();
        assert!(has_active_claim_for_source(&conn, "issue-123").unwrap());

        // Release the claim
        release_claim(&conn, "work-1", None).unwrap();
        assert!(!has_active_claim_for_source(&conn, "issue-123").unwrap());

        // Reclaim it
        upsert_claim(&conn, "work-1", "task", "issue-123", "proj-1", "container-b").unwrap();
        assert!(has_active_claim_for_source(&conn, "issue-123").unwrap());

        // Complete it
        complete_claim(&conn, "work-1").unwrap();
        assert!(!has_active_claim_for_source(&conn, "issue-123").unwrap());
    }

    #[test]
    fn cleanup_old_claims_removes_old_completed() {
        let conn = setup_db();

        // Create and complete two claims
        upsert_claim(&conn, "work-1", "task", "issue-1", "proj-1", "container-a").unwrap();
        complete_claim(&conn, "work-1").unwrap();

        upsert_claim(&conn, "work-2", "task", "issue-2", "proj-1", "container-b").unwrap();
        complete_claim(&conn, "work-2").unwrap();

        // Create an active claim (should not be deleted)
        upsert_claim(&conn, "work-3", "task", "issue-3", "proj-1", "container-c").unwrap();

        // Cleanup with future timestamp should delete completed claims
        let future = Utc::now() + chrono::Duration::hours(1);
        let deleted = cleanup_old_claims(&conn, future).unwrap();
        assert_eq!(deleted, 2);

        // Active claim should remain
        assert!(get_claim(&conn, "work-3").unwrap().is_some());
        assert!(get_claim(&conn, "work-1").unwrap().is_none());
        assert!(get_claim(&conn, "work-2").unwrap().is_none());
    }
}
