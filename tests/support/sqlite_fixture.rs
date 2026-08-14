//! Isolated SQLite fixture for recap persistence schema characterization.
//!
//! The fixture is hermetic: it never inspects environment variables and never
//! reaches a configured database. Every run gets a fresh temporary file, so the
//! suite cannot observe or mutate developer data.
//!
//! Schema state comes entirely from `Database::connect_from_env`. The fixture
//! applies no SQL of its own, so a test that sees a parity table has proved the
//! application migrator created it.

use std::path::PathBuf;

use insights_bot_telegram_rs::{config::DbConfig, db::Database};
use sqlx::{AnyPool, Row, TypeInfo, ValueRef, any::AnyRow};
use tempfile::TempDir;

/// The SQLite half of the additive parity migration, read for source alignment.
///
/// The fixture never executes it; the application migrator owns that.
pub const PARITY_MIGRATION_SQLITE: &str =
    include_str!("../../migrations/sqlite/0003_rich_recap_parity.sql");
/// The PostgreSQL half, compared against the SQLite half by the alignment test.
pub const PARITY_MIGRATION_POSTGRES: &str =
    include_str!("../../migrations/postgres/0003_rich_recap_parity.sql");

/// One `PRAGMA table_info` row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ColumnInfo {
    pub name: String,
    pub declared_type: String,
    pub not_null: bool,
    pub default_value: Option<String>,
    pub primary_key_position: i64,
}

/// A temporary database that lives for exactly one test.
pub struct SchemaFixture {
    directory: TempDir,
}

impl SchemaFixture {
    /// A fresh temporary directory holding one SQLite file.
    pub fn new() -> Self {
        Self {
            directory: tempfile::tempdir().expect("a temporary directory for the fixture"),
        }
    }

    fn database_path(&self) -> PathBuf {
        self.directory.path().join("recap_parity.db")
    }

    /// The literal `sqlite://` URL for this fixture's file.
    ///
    /// Backslashes are normalised and the authority is left empty, so a Windows
    /// drive letter is parsed as part of the path instead of as a host.
    fn database_url(&self) -> String {
        let path = self.database_path().to_string_lossy().replace('\\', "/");
        format!("sqlite:///{}?mode=rwc", path.trim_start_matches('/'))
    }

    fn db_config(&self) -> DbConfig {
        DbConfig {
            postgres_url: None,
            sqlite_file: Some(self.database_url()),
        }
    }

    /// Run the application bootstrap, which is the only source of schema here.
    ///
    /// Calling this twice against the same fixture exercises the re-run path
    /// that a restarted process takes.
    pub async fn bootstrap(&self) -> AnyPool {
        self.bootstrap_database().await.pool
    }

    /// The bootstrapped handle itself, for repositories that branch on backend.
    pub async fn bootstrap_database(&self) -> Database {
        Database::connect_from_env(&self.db_config())
            .await
            .expect("the application bootstrap must succeed")
    }
}

impl Default for SchemaFixture {
    fn default() -> Self {
        Self::new()
    }
}

fn integer_at(row: &AnyRow, index: usize) -> i64 {
    row.try_get::<i64, _>(index)
        .or_else(|_| row.try_get::<i32, _>(index).map(i64::from))
        .expect("a PRAGMA integer column")
}

fn text_at(row: &AnyRow, index: usize) -> String {
    row.try_get::<String, _>(index)
        .expect("a PRAGMA text column")
}

/// Read a PRAGMA column that may legitimately be SQL `NULL`.
///
/// The `Any` driver refuses to decode a genuine `NULL` into `Option<String>` and
/// does not report it through `is_null` either, so nullness is settled on the
/// raw value's type before decoding. Only that one case yields `None`; any other
/// decode failure is a real defect and panics instead of masquerading as an
/// absent value.
fn nullable_text_at(row: &AnyRow, index: usize) -> Option<String> {
    let raw = row
        .try_get_raw(index)
        .unwrap_or_else(|error| panic!("PRAGMA column {index} must exist: {error}"));
    if raw.is_null() || raw.type_info().name().eq_ignore_ascii_case("NULL") {
        return None;
    }
    Some(row.try_get::<String, _>(index).unwrap_or_else(|error| {
        panic!("PRAGMA column {index} is present but did not decode as text: {error}")
    }))
}

/// `PRAGMA table_info(table)`, in declaration order.
pub async fn table_columns(pool: &AnyPool, table: &str) -> Vec<ColumnInfo> {
    let rows = sqlx::query(&format!("PRAGMA table_info({table})"))
        .fetch_all(pool)
        .await
        .expect("PRAGMA table_info must run");

    rows.iter()
        .map(|row| ColumnInfo {
            name: text_at(row, 1),
            declared_type: text_at(row, 2),
            not_null: integer_at(row, 3) != 0,
            // A decode failure must surface. Swallowing it into `None` would be
            // indistinguishable from a column that genuinely has no default.
            default_value: nullable_text_at(row, 4),
            primary_key_position: integer_at(row, 5),
        })
        .collect()
}

/// Look up one column, or `None` when the table does not declare it.
pub async fn column(pool: &AnyPool, table: &str, name: &str) -> Option<ColumnInfo> {
    table_columns(pool, table)
        .await
        .into_iter()
        .find(|column| column.name == name)
}

/// Whether `table` exists at all.
pub async fn table_exists(pool: &AnyPool, table: &str) -> bool {
    !table_columns(pool, table).await.is_empty()
}

/// Column tuples of every `UNIQUE` constraint declared in the table source.
///
/// Primary-key and explicitly created indexes are excluded, so the result is
/// exactly the set of source-declared uniqueness rules.
pub async fn unique_constraint_columns(pool: &AnyPool, table: &str) -> Vec<Vec<String>> {
    let indexes = sqlx::query(&format!("PRAGMA index_list({table})"))
        .fetch_all(pool)
        .await
        .expect("PRAGMA index_list must run");

    let mut declared = Vec::new();
    for row in &indexes {
        // Columns are seq, name, unique, origin, partial.
        let name = text_at(row, 1);
        let unique = integer_at(row, 2) != 0;
        let origin = text_at(row, 3);
        if !unique || origin != "u" {
            continue;
        }

        let members = sqlx::query(&format!("PRAGMA index_info({name})"))
            .fetch_all(pool)
            .await
            .expect("PRAGMA index_info must run");
        declared.push(members.iter().map(|member| text_at(member, 2)).collect());
    }
    declared
}

/// Foreign keys declared by `table`.
pub async fn foreign_keys(pool: &AnyPool, table: &str) -> Vec<String> {
    let rows = sqlx::query(&format!("PRAGMA foreign_key_list({table})"))
        .fetch_all(pool)
        .await
        .expect("PRAGMA foreign_key_list must run");

    rows.iter().map(|row| text_at(row, 2)).collect()
}
