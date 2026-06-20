use crate::datastore::DatastoreInstance;
use duckdb::Connection;

#[derive(Debug, Clone)]
pub enum LegacyDatastoreImportError {
    SQLPrepareError(String),
    SQLMapError(String),
}

/// Legacy import (from the old peewee/SQLite aw-server database) is not supported
/// on the DuckDB backend. This is a no-op kept so the worker's first-init path
/// continues to compile and run; existing peewee databases are simply ignored.
pub fn legacy_import(
    _new_ds: &mut DatastoreInstance,
    _new_conn: &Connection,
) -> Result<(), LegacyDatastoreImportError> {
    info!("Legacy import is not supported on the DuckDB backend; skipping");
    Ok(())
}
