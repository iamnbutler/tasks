//! The migration set, and the rule for naming a new one.
//!
//! **A new migration is named for the UTC instant it was written**, never for
//! the next free number: `20260815030411_build_transcripts.sql`, which is
//! `YYYYMMDDHHMMSS` in UTC, an underscore, and a lower-snake-case description.
//! `make migration NAME=build_transcripts` writes exactly that file; there is
//! no reason to hand-roll one.
//!
//! The reason is that "the next free number" is read off a tree that cannot
//! see its siblings. Two branches cut minutes apart both look at 0023, both
//! write 0024, and neither is wrong on its own — the collision only exists
//! after the merge, where it surfaces as a *boot* failure in a process that
//! has already taken the port. A stamp does not need to see its siblings in
//! order to differ from them.
//!
//! Three facts make the switch additive rather than a rewrite:
//!
//! - 0001–0023 keep their versions, their checksums and their order. sqlx
//!   records version *and* checksum in `_sqlx_migrations`, so renaming a
//!   migration that has been applied somewhere is not an option; the last
//!   sequential version is a floor to leave alone, never a counter to bump.
//! - a 14-digit stamp is a larger `i64` than any four-digit sequence number,
//!   so the order stays "the legacy sequence, then stamps in the order they
//!   were written".
//! - the version is **digits only**. sqlx splits the filename on the first
//!   `_` and parses the left half as an `i64`, so `20260815T030411_x.sql` is
//!   a hard error out of `sqlx::migrate!` — and a name it cannot split at all
//!   (no `_`, or not ending in `.sql`) is *silently skipped*, which is worse
//!   than an error: the migration simply never runs.
//!
//! The tests below are the enforcement. They are what makes a collision a red
//! test in the branch that introduces it, rather than a reviewer's manual
//! catch or a failed boot.

/// Every migration this binary ships with, embedded at compile time.
///
/// `crates/tasks/build.rs` emits `rerun-if-changed=migrations`; without it a
/// new file alone does not dirty the crate and both the server and the guard
/// tests below run against a stale embedded set.
pub(crate) static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

#[cfg(test)]
mod tests {
    use super::MIGRATOR;
    use chrono::NaiveDate;
    use sqlx::migrate::Migrator;
    use sqlx::sqlite::SqlitePoolOptions;
    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;

    /// The last migration named by sequence number. A floor, not a counter:
    /// those 23 files are applied in databases that record their version and
    /// checksum, so they can never be renamed, and nothing may be added below
    /// the line either. `#[cfg(test)]` because the running server has no use
    /// for the boundary — only the guard does.
    const LAST_SEQUENTIAL_VERSION: i64 = 23;

    /// The directory `sqlx::migrate!` embedded, as it sits on disk. The macro
    /// resolves its path against `CARGO_MANIFEST_DIR`, so this is the same
    /// directory by the same rule, not a guess about the test's cwd.
    fn migrations_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("migrations")
    }

    /// The filename a `(version, description)` pair was resolved from — sqlx
    /// strips `.sql` and turns `_` into ` `, so this inverts that. The same
    /// reconstruction [`tasks_api::http::AppliedMigration::file_stem`] makes
    /// when it claims a report is greppable against the directory.
    fn file_name(version: i64, description: &str) -> String {
        format!("{:04}_{}.sql", version, description.replace(' ', "_"))
    }

    /// Whether a version is one this repo is allowed to contain: a frozen
    /// sequence number, or 14 digits that are a real UTC instant.
    ///
    /// The calendar check is done by hand rather than through a `chrono`
    /// format string because the fields are unseparated — a greedy `%Y` would
    /// happily eat digits that belong to the month.
    fn is_allowed_version(version: i64) -> bool {
        if (1..=LAST_SEQUENTIAL_VERSION).contains(&version) {
            return true;
        }
        if version <= 0 {
            return false;
        }
        let digits = version.to_string();
        if digits.len() != 14 {
            return false;
        }
        let field =
            |range: std::ops::Range<usize>| digits[range].parse::<u32>().unwrap_or(u32::MAX);
        NaiveDate::from_ymd_opt(field(0..4) as i32, field(4..6), field(6..8))
            .and_then(|date| date.and_hms_opt(field(8..10), field(10..12), field(12..14)))
            .is_some()
    }

    /// The collision the naming rule exists to prevent, as a red test in the
    /// branch that introduces it rather than a boot failure after the merge.
    #[test]
    fn no_two_migrations_share_a_version() {
        let mut seen: HashMap<i64, String> = HashMap::new();
        for migration in MIGRATOR.iter() {
            let name = file_name(migration.version, &migration.description);
            if let Some(first) = seen.insert(migration.version, name.clone()) {
                panic!(
                    "two migrations share version {}: {first} and {name}. \
                     Migrations are named for the UTC instant they were written, \
                     not for the next free number — run `make migration NAME=...` \
                     and move the SQL into the file it prints.",
                    migration.version
                );
            }
        }
    }

    /// Duplicate versions are only half the rule: `0024_foo.sql` collides with
    /// nothing today and passes the check above happily, while quietly
    /// re-opening the sequence for the next two branches to collide on.
    #[test]
    fn new_migrations_are_named_for_a_utc_instant() {
        for migration in MIGRATOR.iter() {
            assert!(
                is_allowed_version(migration.version),
                "{} is neither one of the frozen 0001–{LAST_SEQUENTIAL_VERSION:04} \
                 sequence numbers nor a YYYYMMDDHHMMSS UTC instant. \
                 Run `make migration NAME=...`; the sequence is closed.",
                file_name(migration.version, &migration.description),
            );
        }
    }

    /// sqlx skips a file it cannot parse *without saying so*, so "the macro
    /// compiled" is not evidence that a migration ships. Both directions of
    /// the difference are checked: a file on disk that never made it into the
    /// set, and an embedded migration whose name does not reconstruct — which
    /// is also what pins `file_stem`'s claim to be greppable.
    #[test]
    fn every_sql_file_in_the_directory_is_embedded() {
        let dir = migrations_dir();
        let on_disk: HashSet<String> = std::fs::read_dir(&dir)
            .expect("the migrations directory is where sqlx::migrate! found it")
            .map(|entry| entry.expect("readable directory entry"))
            .filter(|entry| entry.path().is_file())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            // Editor and Finder droppings are not migrations; anything else in
            // here is, and has to end in .sql to be one.
            .filter(|name| !name.starts_with('.'))
            .collect();
        let embedded: HashSet<String> = MIGRATOR
            .iter()
            .map(|m| file_name(m.version, &m.description))
            .collect();

        let mut skipped: Vec<&String> = on_disk.difference(&embedded).collect();
        skipped.sort();
        assert!(
            skipped.is_empty(),
            "{skipped:?} sit in {} but are not in the migration set — sqlx ignores \
             a name it cannot parse silently, so these never run. A migration is \
             <14 digits>_<lower_snake_case>.sql; see `make migration`.",
            dir.display(),
        );

        let mut phantom: Vec<&String> = embedded.difference(&on_disk).collect();
        phantom.sort();
        assert!(
            phantom.is_empty(),
            "{phantom:?} are embedded but no such file exists — a (version, \
             description) pair no longer reconstructs its filename, so every \
             report that names a migration is now wrong.",
        );
    }

    /// The ordering claim, through a real `Migrator` against real SQLite: a
    /// 14-digit stamp sorts after the legacy sequence, so a stamped migration
    /// may depend on everything numbered.
    #[tokio::test]
    async fn a_timestamp_applies_after_the_legacy_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let migrations = dir.path().join("migrations");
        std::fs::create_dir(&migrations).unwrap();
        std::fs::write(
            migrations.join("0023_first.sql"),
            "CREATE TABLE first (id INTEGER PRIMARY KEY);",
        )
        .unwrap();
        // Fails outright if it runs first: the table would not exist yet.
        std::fs::write(
            migrations.join("20260815030411_second.sql"),
            "INSERT INTO first (id) VALUES (1);",
        )
        .unwrap();

        let migrator = Migrator::new(migrations.as_path()).await.unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("ordered.db").display()
        );
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap();
        migrator.run(&pool).await.unwrap();

        let versions: Vec<i64> =
            sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(versions, vec![23, 20260815030411]);
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM first")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 1, "the stamped migration ran after the legacy one");
    }

    /// The trap the issue's own example filename falls into. `20260815T030411`
    /// is not an `i64`, and sqlx does not shrug at that the way it shrugs at a
    /// name with no `_` — it fails resolution outright, which out of
    /// `sqlx::migrate!` is a compile error. Digits only.
    #[tokio::test]
    async fn a_t_separated_timestamp_is_rejected_not_ignored() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("20260815T030411_second.sql"), "SELECT 1;").unwrap();

        let err = Migrator::new(dir.path())
            .await
            .expect_err("an ISO-8601 stamp is not a version");
        let message = err.to_string();
        assert!(
            message.contains("20260815T030411"),
            "the failure should name the file: {message}"
        );
    }

    /// The version that made `specs.session_id` nullable (#869), and the one
    /// the test below stops short of so it can seed the old schema.
    const HUMAN_AUTHORED_SPECS: i64 = 20260816021713;

    /// The claim that migration rests on, against real SQLite with foreign
    /// keys enforced: `specs` can be dropped and re-created under its own name
    /// while `spec_queue` and `build_specs` still reference it, and the rows in
    /// those children come back untouched.
    ///
    /// This was the thing expected to blow up. `ALTER TABLE specs_new RENAME
    /// TO specs` runs while two tables hold foreign keys pointing at a table
    /// that does not exist at that instant — which is not a schema parse error
    /// and needs no `PRAGMA legacy_alter_table`. The rest of the migration is
    /// the workaround for the part that *is* real: `DROP TABLE specs` with
    /// foreign keys on runs an implicit delete, which would cascade the review
    /// queue away and trip `build_specs`, and `PRAGMA foreign_keys` is a silent
    /// no-op inside the transaction sqlx wraps each migration in.
    #[tokio::test]
    async fn making_session_id_nullable_keeps_the_children_of_specs() {
        use sqlx::Row;
        use sqlx::sqlite::SqliteConnectOptions;

        let dir = tempfile::tempdir().unwrap();
        let earlier = dir.path().join("earlier");
        std::fs::create_dir(&earlier).unwrap();
        for entry in std::fs::read_dir(migrations_dir()).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            let version: i64 = match name.split('_').next().and_then(|v| v.parse().ok()) {
                Some(v) => v,
                None => continue,
            };
            if version < HUMAN_AUTHORED_SPECS {
                std::fs::copy(entry.path(), earlier.join(&name)).unwrap();
            }
        }

        let options = SqliteConnectOptions::new()
            .filename(dir.path().join("swap.db"))
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        let fk: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(fk, 1, "the swap is only interesting with FKs enforced");

        Migrator::new(earlier.as_path())
            .await
            .unwrap()
            .run(&pool)
            .await
            .unwrap();

        // The full chain the swap has to carry across: a project, a task, a
        // session, a spec, its queue entry, and a build holding that spec.
        for statement in [
            "INSERT INTO projects (id, repo_owner, repo_name, added_at) \
             VALUES ('proj', 'o', 'r', '2026-08-16T00:00:00Z')",
            "INSERT INTO tasks (id, project_id, gh_issue_number, title, body, labels, \
             gh_state, state, priority, manual_rank, dispatch_attempts, ingested_at, updated_at) \
             VALUES ('task', 'proj', 1, 't', 'b', '[]', 'open', 'ready_to_build', 0, NULL, 0, \
             '2026-08-16T00:00:00Z', '2026-08-16T00:00:00Z')",
            "INSERT INTO sessions (id, task_id, vm_id, branch, status, started_at) \
             VALUES ('sess', 'task', NULL, 'scout/1', 'scout_succeeded', '2026-08-16T00:00:00Z')",
            "INSERT INTO specs (id, session_id, task_id, content, complexity, files_touched, \
             created_at) VALUES ('spec', 'sess', 'task', '## Spec', 'simple', '[\"a.rs\"]', \
             '2026-08-16T00:00:00Z')",
            "INSERT INTO spec_queue (spec_id, status, rank, approved_at, feedback, \
             blocking_dependencies) VALUES ('spec', 'approved', 3, '2026-08-16T00:00:00Z', \
             'ship it', '[]')",
            "INSERT INTO builds (id, project_id, branch, base_branch, status, created_at) \
             VALUES ('build', 'proj', 'build/1', 'main', 'succeeded', '2026-08-16T00:00:00Z')",
            "INSERT INTO build_specs (build_id, spec_id, position) VALUES ('build', 'spec', 1)",
        ] {
            sqlx::query(statement).execute(&pool).await.unwrap();
        }

        MIGRATOR.run(&pool).await.expect("the swap applies");

        // The column moved…
        sqlx::query(
            "INSERT INTO specs (id, session_id, task_id, content, complexity, files_touched, \
             created_at) VALUES ('hand', NULL, 'task', '## Spec', 'simple', '[]', \
             '2026-08-16T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .expect("session_id is nullable now");

        // …and the scouted spec came across whole.
        let row = sqlx::query("SELECT session_id, files_touched FROM specs WHERE id = 'spec'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row.try_get::<String, _>("session_id").unwrap(), "sess");
        assert_eq!(
            row.try_get::<String, _>("files_touched").unwrap(),
            "[\"a.rs\"]"
        );

        // The children are the point: an unguarded DROP would have cascaded
        // this queue entry away and failed on the build_specs row.
        let entry =
            sqlx::query("SELECT status, rank, feedback FROM spec_queue WHERE spec_id = 'spec'")
                .fetch_one(&pool)
                .await
                .expect("the queue entry survived");
        assert_eq!(entry.try_get::<String, _>("status").unwrap(), "approved");
        assert_eq!(entry.try_get::<i64, _>("rank").unwrap(), 3);
        assert_eq!(entry.try_get::<String, _>("feedback").unwrap(), "ship it");
        let position: i64 = sqlx::query_scalar(
            "SELECT position FROM build_specs WHERE build_id = 'build' AND spec_id = 'spec'",
        )
        .fetch_one(&pool)
        .await
        .expect("the build still holds its spec");
        assert_eq!(position, 1);

        // The index dropped with the old table is back, so the per-task
        // lookup every read does is not a scan.
        let indexes: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'specs'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(
            indexes.iter().any(|name| name == "specs_task_idx"),
            "specs_task_idx was dropped with the table and not recreated: {indexes:?}"
        );

        // And the foreign keys still resolve, now that `specs` exists again.
        let violations = sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert!(violations.is_empty(), "the swap left dangling references");
    }

    /// The version that removed the briefing subsystem (#918), and the one the
    /// test below stops short of so it can seed a log containing the retired
    /// event kind.
    const REMOVE_BRIEFINGS: i64 = 20260817135101;

    /// Deleting an [`tasks_api::events::EventPayload`] variant is not a code
    /// change alone. The enum is internally tagged and strict, and
    /// `Store::event_from_row` deserializes with a hard `?`, so one surviving
    /// `briefing_updated` row makes `GET /events` and `/events/stream` fail
    /// **forever** — the Activity feed dies permanently on every database that
    /// ever generated a briefing, and no later pass repairs it.
    ///
    /// So the migration deletes those rows, and this is the test that says so.
    /// Without the `DELETE` it fails on the `all_events()` line below, which is
    /// the whole reason to write it. The surviving seqs are asserted to be 1
    /// and 3: retiring a vocabulary leaves a *gap*, never a renumbering —
    /// every reader compares seq (`>= ?`, `ORDER BY`, the orchestrator's
    /// `> answered_seq` watermark) and none counts on contiguity.
    #[tokio::test]
    async fn removing_briefings_takes_their_events_with_them() {
        use crate::store::Store;
        use sqlx::sqlite::SqliteConnectOptions;

        let dir = tempfile::tempdir().unwrap();
        let earlier = dir.path().join("earlier");
        std::fs::create_dir(&earlier).unwrap();
        for entry in std::fs::read_dir(migrations_dir()).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            let version: i64 = match name.split('_').next().and_then(|v| v.parse().ok()) {
                Some(v) => v,
                None => continue,
            };
            if version < REMOVE_BRIEFINGS {
                std::fs::copy(entry.path(), earlier.join(&name)).unwrap();
            }
        }

        let db = dir.path().join("briefings.db");
        let options = SqliteConnectOptions::new()
            .filename(&db)
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        Migrator::new(earlier.as_path())
            .await
            .unwrap()
            .run(&pool)
            .await
            .unwrap();

        // A log as it stood the day before: two ordinary notes with a
        // now-retired briefing event between them.
        for payload in [
            r#"{"kind":"note","source":"test","message":"before"}"#,
            r#"{"kind":"briefing_updated","section":"changes"}"#,
            r#"{"kind":"note","source":"test","message":"after"}"#,
        ] {
            sqlx::query(
                "INSERT INTO events (timestamp, payload) VALUES ('2026-08-17T00:00:00Z', ?)",
            )
            .bind(payload)
            .execute(&pool)
            .await
            .unwrap();
        }
        pool.close().await;

        // Through `Store::open`, so the removal migration is applied by the
        // same path the server boots on.
        let store = Store::open(&db).await.expect("the removal applies");
        let events = store
            .all_events()
            .await
            .expect("the feed still reads — a surviving briefing_updated row would 500 it");
        let kinds: Vec<&str> = events.iter().map(|e| e.payload.kind()).collect();
        assert_eq!(kinds, vec!["note", "note"]);
        assert_eq!(
            events.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![1, 3],
            "a retired kind leaves a gap in seq; it must never renumber the log"
        );

        drop(store);

        // And the cache table itself is gone.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(SqliteConnectOptions::new().filename(&db))
            .await
            .unwrap();
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'briefings'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(
            tables.is_empty(),
            "the briefings table survived: {tables:?}"
        );
    }

    /// The boundary itself, without a filesystem: what counts as a version.
    #[test]
    fn allowed_versions_are_the_frozen_sequence_and_real_instants() {
        assert!(is_allowed_version(1));
        assert!(is_allowed_version(LAST_SEQUENTIAL_VERSION));
        assert!(is_allowed_version(20260815030411));
        // Midnight, and a leap day, are both real instants.
        assert!(is_allowed_version(20260815000000));
        assert!(is_allowed_version(20240229120000));

        // The sequence is closed: one past the end is not "the next one".
        assert!(!is_allowed_version(LAST_SEQUENTIAL_VERSION + 1));
        // Shapes that look like a date but are not this one.
        assert!(!is_allowed_version(20260815), "date only");
        assert!(!is_allowed_version(1755230651), "unix seconds");
        assert!(!is_allowed_version(202608150304110), "milliseconds");
        // Digits in the right places, no such instant.
        assert!(!is_allowed_version(20261315030411), "month 13");
        assert!(!is_allowed_version(20260230030411), "February 30th");
        assert!(!is_allowed_version(20260815240000), "hour 24");
        assert!(!is_allowed_version(0));
        assert!(!is_allowed_version(-20260815030411));
    }
}
