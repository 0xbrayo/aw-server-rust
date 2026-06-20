use std::collections::HashMap;

use chrono::DateTime;
use chrono::Duration;
use chrono::Utc;

use duckdb::params;
use duckdb::types::ToSql;
use duckdb::Connection;

use serde_json::value::Value;

use aw_models::Bucket;
use aw_models::BucketMetadata;
use aw_models::Event;

use super::DatastoreError;

/// Parse an event's `data` JSON object.
///
/// This runs once per event returned from a query, so it dominates the cost of
/// reading large time ranges. On 64-bit x86/ARM we use the SIMD-accelerated
/// sonic-rs parser; other targets fall back to serde_json. Both produce an
/// identical `serde_json::Map`, so callers and the on-disk format are
/// unaffected. The error is normalised to a String so the call sites are
/// arch-independent.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn parse_event_data(data_str: &str) -> Result<serde_json::Map<String, Value>, String> {
    sonic_rs::from_str(data_str).map_err(|e| e.to_string())
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn parse_event_data(data_str: &str) -> Result<serde_json::Map<String, Value>, String> {
    serde_json::from_str(data_str).map_err(|e| e.to_string())
}

/// Build the DuckDB JSON-extraction SQL fragment for a top-level event-data key,
/// escaping it for safe inlining into a JSON path string. `data` is stored as
/// VARCHAR; DuckDB implicitly casts it to JSON for `json_extract_string`.
fn json_extract_expr(column: &str, key: &str) -> String {
    // Escape backslashes and double quotes for the quoted JSON path member, and
    // single quotes for the surrounding SQL string literal.
    let escaped = key
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\'', "''");
    format!("json_extract_string({column}, '$.\"{escaped}\"')")
}

// === Event read queries ===
//
// These are free functions taking a `&Connection` and a resolved bucket row id
// (`bid`) so they can run on either the worker's write connection (via the
// `DatastoreInstance` methods below, which look up `bid` from the bucket cache)
// or a separate read-only connection (via `worker::Reader`, which looks up
// `bid` with `query_bid`). Keeping the SQL and row parsing in one place means
// both paths stay byte-for-byte identical.

/// Build an `Event` from a `(id, starttime, endtime, data)` row. When `clip` is
/// set, the event's start/end are clamped to `[clip_start_ns, clip_end_ns]`.
fn event_from_row(
    row: &duckdb::Row,
    clip_start_ns: i64,
    clip_end_ns: i64,
    clip: bool,
) -> duckdb::Result<Event> {
    let id: i64 = row.get(0)?;
    let mut starttime_ns: i64 = row.get(1)?;
    let mut endtime_ns: i64 = row.get(2)?;
    let data_str: String = row.get(3)?;

    if clip {
        if starttime_ns < clip_start_ns {
            starttime_ns = clip_start_ns;
        }
        if endtime_ns > clip_end_ns {
            endtime_ns = clip_end_ns;
        }
    }
    let duration_ns = endtime_ns - starttime_ns;
    let time_seconds: i64 = starttime_ns / 1_000_000_000;
    let time_subnanos: u32 = (starttime_ns % 1_000_000_000) as u32;
    let data = parse_event_data(&data_str)
        .map_err(|e| duckdb::Error::InvalidColumnName(format!("invalid event data JSON: {e}")))?;

    Ok(Event {
        id: Some(id),
        timestamp: DateTime::from_timestamp(time_seconds, time_subnanos).unwrap(),
        duration: Duration::nanoseconds(duration_ns),
        data,
    })
}

/// Resolve a bucket's integer row id by name. Used by the read-only connection
/// path, which has no access to the worker's in-memory bucket cache.
pub(crate) fn query_bid(conn: &Connection, bucket_id: &str) -> Result<i64, DatastoreError> {
    let mut stmt = conn
        .prepare_cached("SELECT id FROM buckets WHERE name = ?")
        .map_err(|err| {
            DatastoreError::InternalError(format!("Failed to prepare bucket id lookup: {err}"))
        })?;
    match stmt.query_row([bucket_id], |row| row.get(0)) {
        Ok(bid) => Ok(bid),
        Err(duckdb::Error::QueryReturnedNoRows) => {
            Err(DatastoreError::NoSuchBucket(bucket_id.to_string()))
        }
        Err(err) => Err(DatastoreError::InternalError(format!(
            "Failed to look up bucket id for {bucket_id}: {err}"
        ))),
    }
}

pub(crate) fn query_event(
    conn: &Connection,
    bid: i64,
    event_id: i64,
) -> Result<Event, DatastoreError> {
    let mut stmt = conn
        .prepare_cached(
            "
                SELECT id, starttime, endtime, data
                FROM events
                WHERE bucketrow = ?
                    AND id = ?
                LIMIT 1
            ;",
        )
        .map_err(|err| {
            DatastoreError::InternalError(format!(
                "Failed to prepare get_event SQL statement: {err}"
            ))
        })?;
    stmt.query_row([&bid, &event_id], |row| event_from_row(row, 0, 0, false))
        .map_err(|err| {
            DatastoreError::InternalError(format!("Failed to map get_event SQL statement: {err}"))
        })
}

pub(crate) fn query_events(
    conn: &Connection,
    bid: i64,
    bucket_id: &str,
    starttime_opt: Option<DateTime<Utc>>,
    endtime_opt: Option<DateTime<Utc>>,
    limit_opt: Option<u64>,
    clip_to_query_range: bool,
    filters: Option<&[crate::EventFilter]>,
) -> Result<Vec<Event>, DatastoreError> {
    let mut list = Vec::new();

    let starttime_filter_ns: i64 = match starttime_opt {
        Some(dt) => dt.timestamp_nanos_opt().unwrap(),
        None => 0,
    };
    let endtime_filter_ns: i64 = match endtime_opt {
        Some(dt) => dt.timestamp_nanos_opt().unwrap(),
        None => std::i64::MAX,
    };
    if starttime_filter_ns > endtime_filter_ns {
        warn!("Starttime in event query was lower than endtime!");
        return Ok(list);
    }
    let limit = match limit_opt {
        Some(l) => l as i64,
        None => -1,
    };

    let has_filters = filters.map(|f| !f.is_empty()).unwrap_or(false);

    if !has_filters {
        // DuckDB has no "LIMIT -1"; an absent limit is expressed with a NULL
        // bound parameter (DuckDB treats LIMIT NULL as no limit).
        let limit_param: Option<i64> = if limit < 0 { None } else { Some(limit) };
        let mut stmt = conn
            .prepare_cached(
                "
                    SELECT id, starttime, endtime, data
                    FROM events
                    WHERE bucketrow = ?
                        AND endtime >= ?
                        AND starttime <= ?
                    ORDER BY starttime DESC, id ASC
                    LIMIT ?
                ;",
            )
            .map_err(|err| {
                DatastoreError::InternalError(format!(
                    "Failed to prepare get_events SQL statement: {err}"
                ))
            })?;

        let rows = stmt
            .query_map(
                params![bid, starttime_filter_ns, endtime_filter_ns, limit_param],
                |row| {
                    event_from_row(
                        row,
                        starttime_filter_ns,
                        endtime_filter_ns,
                        clip_to_query_range,
                    )
                },
            )
            .map_err(|err| {
                DatastoreError::InternalError(format!(
                    "Failed to map get_events SQL statement: {err}"
                ))
            })?;

        for row in rows {
            match row {
                Ok(event) => list.push(event),
                Err(err) => warn!("Corrupt event in bucket {}: {}", bucket_id, err),
            };
        }
    } else {
        let mut sql = "SELECT id, starttime, endtime, data FROM events WHERE bucketrow = ? AND endtime >= ? AND starttime <= ?".to_string();
        let mut params: Vec<Box<dyn ToSql>> = vec![
            Box::new(bid),
            Box::new(starttime_filter_ns),
            Box::new(endtime_filter_ns),
        ];

        for filter in filters.unwrap() {
            if filter.vals.is_empty() {
                sql.push_str(" AND 1=0");
                continue;
            }
            sql.push_str(&format!(
                " AND {} IN (",
                json_extract_expr("data", &filter.key)
            ));
            for (i, val) in filter.vals.iter().enumerate() {
                if i > 0 {
                    sql.push_str(", ");
                }
                sql.push('?');

                // json_extract_string yields text, so compare every value as its
                // string form. Binding numbers/bools as native SQL types instead
                // would force a fragile VARCHAR-vs-BIGINT cast and diverge from
                // query_events_intersected, which also stringifies.
                let sql_val: String = match val {
                    serde_json::Value::String(s) => s.clone(),
                    serde_json::Value::Null => String::new(),
                    other => other.to_string(),
                };
                params.push(Box::new(sql_val));
            }
            sql.push(')');
        }

        // json_extract_string returns text, so values are compared as strings.
        sql.push_str(" ORDER BY starttime DESC, id ASC LIMIT ?");
        let limit_param: Option<i64> = if limit < 0 { None } else { Some(limit) };
        params.push(Box::new(limit_param));

        let mut stmt = conn.prepare(&sql).map_err(|err| {
            DatastoreError::InternalError(format!(
                "Failed to prepare get_events SQL statement: {err}"
            ))
        })?;

        let params_refs: Vec<&dyn ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let rows = stmt
            .query_map(params_refs.as_slice(), |row| {
                event_from_row(
                    row,
                    starttime_filter_ns,
                    endtime_filter_ns,
                    clip_to_query_range,
                )
            })
            .map_err(|err| {
                DatastoreError::InternalError(format!(
                    "Failed to map get_events SQL statement: {err}"
                ))
            })?;

        for row in rows {
            match row {
                Ok(event) => list.push(event),
                Err(err) => warn!("Corrupt event in bucket {}: {}", bucket_id, err),
            };
        }
    }

    Ok(list)
}

pub(crate) fn query_events_grouped(
    conn: &Connection,
    bid: i64,
    bucket_id: &str,
    starttime_opt: Option<DateTime<Utc>>,
    endtime_opt: Option<DateTime<Utc>>,
    group_by_key: &str,
) -> Result<Vec<Event>, DatastoreError> {
    let mut list = Vec::new();

    let starttime_filter_ns: i64 = match starttime_opt {
        Some(dt) => dt.timestamp_nanos_opt().unwrap(),
        None => 0,
    };
    let endtime_filter_ns: i64 = match endtime_opt {
        Some(dt) => dt.timestamp_nanos_opt().unwrap(),
        None => std::i64::MAX,
    };

    if starttime_filter_ns > endtime_filter_ns {
        warn!("Starttime in event query was lower than endtime!");
        return Ok(list);
    }

    let sql = format!(
        // DuckDB's sum() over a BIGINT column returns HUGEINT (i128); cast back
        // to BIGINT so the row reads as i64. A bucket's total duration in
        // nanoseconds cannot overflow i64 (~292 years), so the cast is safe.
        "SELECT
            min(starttime) as starttime,
            CAST(sum(endtime - starttime) AS BIGINT) as duration_ns,
            {} as group_val
        FROM events
        WHERE bucketrow = ? AND endtime >= ? AND starttime <= ?
        GROUP BY group_val
        ORDER BY duration_ns DESC",
        json_extract_expr("data", group_by_key)
    );

    let mut stmt = conn.prepare(&sql).map_err(|err| {
        DatastoreError::InternalError(format!(
            "Failed to prepare query_events_grouped SQL statement: {err}"
        ))
    })?;

    let rows = stmt
        .query_map([&bid, &starttime_filter_ns, &endtime_filter_ns], |row| {
            let starttime_ns: i64 = row.get(0)?;
            let duration_ns: i64 = row.get(1)?;
            let group_val: Option<String> = row.get(2)?;

            let time_seconds: i64 = starttime_ns / 1_000_000_000;
            let time_subnanos: u32 = (starttime_ns % 1_000_000_000) as u32;

            let mut data_map = serde_json::Map::new();
            if let Some(val) = group_val {
                data_map.insert(group_by_key.to_string(), serde_json::Value::String(val));
            } else {
                data_map.insert(group_by_key.to_string(), serde_json::Value::Null);
            }

            Ok(Event {
                id: None,
                timestamp: DateTime::from_timestamp(time_seconds, time_subnanos).unwrap(),
                duration: Duration::nanoseconds(duration_ns),
                data: data_map,
            })
        })
        .map_err(|err| {
            DatastoreError::InternalError(format!(
                "Failed to map query_events_grouped SQL statement: {err}"
            ))
        })?;

    for row in rows {
        match row {
            Ok(event) => list.push(event),
            Err(err) => warn!("Corrupt grouped event in bucket {}: {}", bucket_id, err),
        };
    }

    Ok(list)
}

pub(crate) fn query_events_intersected(
    conn: &Connection,
    target_bid: i64,
    filter_bid: i64,
    target_bucket_id: &str,
    starttime_opt: Option<DateTime<Utc>>,
    endtime_opt: Option<DateTime<Utc>>,
    filter_key: &str,
    filter_val: &serde_json::Value,
) -> Result<Vec<Event>, DatastoreError> {
    let mut list = Vec::new();

    let starttime_filter_ns: i64 = match starttime_opt {
        Some(dt) => dt.timestamp_nanos_opt().unwrap(),
        None => 0,
    };
    let endtime_filter_ns: i64 = match endtime_opt {
        Some(dt) => dt.timestamp_nanos_opt().unwrap(),
        None => std::i64::MAX,
    };

    if starttime_filter_ns > endtime_filter_ns {
        warn!("Starttime in event query was lower than endtime!");
        return Ok(list);
    }

    let sql = format!(
        "SELECT
            window.id,
            greatest(window.starttime, afk.starttime) as intersect_start,
            least(window.endtime, afk.endtime) as intersect_end,
            window.data
        FROM events window
        JOIN events afk ON
            window.bucketrow = ?
            AND afk.bucketrow = ?
            AND window.starttime < afk.endtime
            AND window.endtime > afk.starttime
            AND {} = ?
        WHERE greatest(window.starttime, afk.starttime) < least(window.endtime, afk.endtime)
           AND window.endtime >= ?
           AND window.starttime <= ?
        ORDER BY intersect_start ASC",
        json_extract_expr("afk.data", filter_key)
    );

    let mut stmt = conn.prepare(&sql).map_err(|err| {
        DatastoreError::InternalError(format!(
            "Failed to prepare query_events_intersected SQL statement: {err}"
        ))
    })?;

    // json_extract_string returns text, so the filter value is compared as a string.
    let filter_val_str: String = match filter_val {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    };

    let params: [&dyn ToSql; 5] = [
        &target_bid,
        &filter_bid,
        &filter_val_str,
        &starttime_filter_ns,
        &endtime_filter_ns,
    ];

    let rows = stmt
        .query_map(params, |row| {
            let id: i64 = row.get(0)?;
            let starttime_ns: i64 = row.get(1)?;
            let endtime_ns: i64 = row.get(2)?;
            let data_str: String = row.get(3)?;

            let time_seconds: i64 = starttime_ns / 1_000_000_000;
            let time_subnanos: u32 = (starttime_ns % 1_000_000_000) as u32;
            let duration_ns: i64 = endtime_ns - starttime_ns;

            let data = parse_event_data(&data_str).unwrap_or_default();

            Ok(Event {
                id: Some(id),
                timestamp: DateTime::from_timestamp(time_seconds, time_subnanos).unwrap(),
                duration: Duration::nanoseconds(duration_ns),
                data,
            })
        })
        .map_err(|err| {
            DatastoreError::InternalError(format!(
                "Failed to map query_events_intersected SQL statement: {err}"
            ))
        })?;

    for row in rows {
        match row {
            Ok(event) => list.push(event),
            Err(err) => warn!(
                "Corrupt intersected event in bucket {}: {}",
                target_bucket_id, err
            ),
        };
    }

    Ok(list)
}

pub(crate) fn query_event_count(
    conn: &Connection,
    bid: i64,
    starttime_opt: Option<DateTime<Utc>>,
    endtime_opt: Option<DateTime<Utc>>,
) -> Result<i64, DatastoreError> {
    let starttime_filter_ns: i64 = match starttime_opt {
        Some(dt) => dt.timestamp_nanos_opt().unwrap(),
        None => 0,
    };
    let endtime_filter_ns: i64 = match endtime_opt {
        Some(dt) => dt.timestamp_nanos_opt().unwrap(),
        None => std::i64::MAX,
    };
    if starttime_filter_ns >= endtime_filter_ns {
        warn!("Endtime in event query was same or lower than starttime!");
        return Ok(0);
    }

    let mut stmt = conn
        .prepare_cached(
            "
            SELECT count(*) FROM events
            WHERE bucketrow = ?
                AND endtime >= ?
                AND starttime <= ?",
        )
        .map_err(|err| {
            DatastoreError::InternalError(format!(
                "Failed to prepare get_event_count SQL statement: {err}",
            ))
        })?;

    stmt.query_row([&bid, &starttime_filter_ns, &endtime_filter_ns], |row| {
        row.get(0)
    })
    .map_err(|err| {
        DatastoreError::InternalError(format!(
            "Failed to query get_event_count SQL statement: {err}"
        ))
    })
}

/*
 * ### Schema notes (DuckDB) ###
 * The old SQLite `user_version` migration ladder is gone: DuckDB has no
 * per-database user_version pragma and this backend starts from a fresh schema
 * (no migration from the SQLite databases). `db_version` is kept on
 * `DatastoreInstance` for API compatibility and pinned to NEWEST_DB_VERSION.
 */
static NEWEST_DB_VERSION: i32 = 5;

/// Create the schema if absent. Auto-increment ids come from DuckDB sequences
/// (there is no AUTOINCREMENT); timestamps are stored as nanosecond BIGINTs and
/// `data` as VARCHAR (JSON text), matching the values bound from Rust.
fn _create_tables(conn: &Connection) -> Result<(), DatastoreError> {
    conn.execute_batch(
        "
        CREATE SEQUENCE IF NOT EXISTS seq_bucket_id START 1;
        CREATE TABLE IF NOT EXISTS buckets (
            id BIGINT PRIMARY KEY DEFAULT nextval('seq_bucket_id'),
            name VARCHAR UNIQUE NOT NULL,
            \"type\" VARCHAR NOT NULL,
            client VARCHAR NOT NULL,
            hostname VARCHAR NOT NULL,
            created VARCHAR NOT NULL,
            data VARCHAR NOT NULL DEFAULT '{}'
        );
        CREATE SEQUENCE IF NOT EXISTS seq_event_id START 1;
        CREATE TABLE IF NOT EXISTS events (
            id BIGINT PRIMARY KEY DEFAULT nextval('seq_event_id'),
            bucketrow BIGINT NOT NULL,
            starttime BIGINT NOT NULL,
            endtime BIGINT NOT NULL,
            data VARCHAR NOT NULL
        );
        CREATE TABLE IF NOT EXISTS key_value (
            key VARCHAR PRIMARY KEY,
            value VARCHAR,
            last_modified BIGINT NOT NULL
        );
        -- Serves the per-bucket newest-first range scan every event read does
        -- (WHERE bucketrow=? AND endtime>=? AND starttime<=? ORDER BY starttime).
        CREATE INDEX IF NOT EXISTS events_bucketrow_starttime
            ON events (bucketrow, starttime, endtime);
        ",
    )
    .map_err(|err| DatastoreError::InternalError(format!("Failed to create DuckDB schema: {err}")))
}

/// Realign the id sequences with the largest id already stored.
///
/// Ids assigned explicitly (e.g. events arriving from sync/import via
/// `INSERT OR REPLACE`) do not advance the `nextval` sequence, so after such a
/// load the sequence can still hand out an already-used id and collide on the
/// primary key. DuckDB has no `setval`/`ALTER SEQUENCE RESTART`, and
/// `CREATE OR REPLACE SEQUENCE` is refused while a table default depends on the
/// sequence, so advance `nextval` (which is allowed) until it is past the
/// largest stored id. Gaps in the id space are harmless.
fn _resync_sequences(conn: &Connection) -> Result<(), DatastoreError> {
    for (seq, table) in [("seq_bucket_id", "buckets"), ("seq_event_id", "events")] {
        let max_id: i64 = conn
            .query_row(
                &format!("SELECT coalesce(max(id), 0) FROM {table}"),
                [],
                |row| row.get(0),
            )
            .map_err(|err| {
                DatastoreError::InternalError(format!("Failed to read max id from {table}: {err}"))
            })?;
        if max_id == 0 {
            // Empty table: the freshly created sequence already starts at 1.
            continue;
        }
        let cur: i64 = conn
            .query_row(&format!("SELECT nextval('{seq}')"), [], |row| row.get(0))
            .map_err(|err| {
                DatastoreError::InternalError(format!("Failed to read sequence {seq}: {err}"))
            })?;
        if cur < max_id {
            // Consume the (max_id - cur) values between here and max_id in one
            // query so the next nextval yields max_id + 1.
            conn.execute_batch(&format!(
                "SELECT nextval('{seq}') FROM range({});",
                max_id - cur
            ))
            .map_err(|err| {
                DatastoreError::InternalError(format!("Failed to advance sequence {seq}: {err}"))
            })?;
        }
    }
    Ok(())
}

/// Whether the `buckets` table already exists, used to decide first-init.
fn _buckets_table_exists(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT count(*) FROM information_schema.tables WHERE table_name = 'buckets'",
        [],
        |row| row.get::<_, i64>(0),
    )
    .map(|n| n > 0)
    .unwrap_or(false)
}

pub struct DatastoreInstance {
    buckets_cache: HashMap<String, Bucket>,
    first_init: bool,
    pub db_version: i32,
}

impl DatastoreInstance {
    pub fn new(
        conn: &Connection,
        migrate_enabled: bool,
    ) -> Result<DatastoreInstance, DatastoreError> {
        let existed = _buckets_table_exists(conn);
        let first_init = !existed;

        if migrate_enabled {
            _create_tables(conn)?;
            // An existing database may already hold rows with higher ids than the
            // (freshly re-created) sequences would produce; realign them.
            _resync_sequences(conn)?;
        } else if !existed {
            return Err(DatastoreError::Uninitialized(
                "Tried to open an uninitialized datastore with migration disabled".to_string(),
            ));
        }

        let mut ds = DatastoreInstance {
            buckets_cache: HashMap::new(),
            first_init,
            db_version: NEWEST_DB_VERSION,
        };
        ds.get_stored_buckets(conn)?;
        Ok(ds)
    }

    fn get_stored_buckets(&mut self, conn: &Connection) -> Result<(), DatastoreError> {
        let mut stmt = match conn.prepare_cached(
            "
            SELECT  buckets.id, buckets.name, buckets.\"type\", buckets.client,
                    buckets.hostname, buckets.created,
                    min(events.starttime), max(events.endtime),
                    buckets.data
            FROM buckets
            LEFT OUTER JOIN events ON buckets.id = events.bucketrow
            GROUP BY ALL
            ;",
        ) {
            Ok(stmt) => stmt,
            Err(err) => {
                return Err(DatastoreError::InternalError(format!(
                    "Failed to prepare get_stored_buckets SQL statement: {err}"
                )))
            }
        };
        let buckets = match stmt.query_map([], |row| {
            let opt_start_ns: Option<i64> = row.get(6)?;
            let opt_start = match opt_start_ns {
                Some(starttime_ns) => {
                    let seconds: i64 = starttime_ns / 1_000_000_000;
                    let subnanos: u32 = (starttime_ns % 1_000_000_000) as u32;
                    Some(DateTime::from_timestamp(seconds, subnanos).unwrap())
                }
                None => None,
            };

            let opt_end_ns: Option<i64> = row.get(7)?;
            let opt_end = match opt_end_ns {
                Some(endtime_ns) => {
                    let seconds: i64 = endtime_ns / 1_000_000_000;
                    let subnanos: u32 = (endtime_ns % 1_000_000_000) as u32;
                    Some(DateTime::from_timestamp(seconds, subnanos).unwrap())
                }
                None => None,
            };

            // If data column is not set (possible on old installations), use an empty map as default
            let data_str: String = row.get(8)?;
            let data_json = match serde_json::from_str(&data_str) {
                Ok(data) => data,
                Err(e) => {
                    return Err(duckdb::Error::InvalidColumnName(format!(
                        "Failed to parse data to JSON: {e:?}"
                    )))
                }
            };

            // `created` is stored as an RFC3339 string.
            let created_str: String = row.get(5)?;
            let created = DateTime::parse_from_rfc3339(&created_str)
                .map(|dt| dt.with_timezone(&Utc))
                .map_err(|e| {
                    duckdb::Error::InvalidColumnName(format!("Failed to parse created: {e:?}"))
                })?;

            Ok(Bucket {
                bid: row.get(0)?,
                id: row.get(1)?,
                _type: row.get(2)?,
                client: row.get(3)?,
                hostname: row.get(4)?,
                created: Some(created),
                data: data_json,
                metadata: BucketMetadata {
                    start: opt_start,
                    end: opt_end,
                },
                events: None,
                last_updated: None,
            })
        }) {
            Ok(buckets) => buckets,
            Err(err) => {
                return Err(DatastoreError::InternalError(format!(
                    "Failed to query get_stored_buckets SQL statement: {err:?}"
                )))
            }
        };
        for bucket in buckets {
            match bucket {
                Ok(b) => {
                    self.buckets_cache.insert(b.id.clone(), b.clone());
                }
                Err(e) => {
                    return Err(DatastoreError::InternalError(format!(
                        "Failed to parse bucket from DuckDB, database is corrupt! {e:?}"
                    )))
                }
            }
        }
        Ok(())
    }

    pub fn ensure_legacy_import(&mut self, conn: &Connection) -> Result<bool, ()> {
        use super::legacy_import::legacy_import;
        if !self.first_init {
            Ok(false)
        } else {
            self.first_init = false;
            match legacy_import(self, conn) {
                Ok(_) => {
                    info!("Successfully imported legacy database");
                    self.get_stored_buckets(conn).unwrap();
                    Ok(true)
                }
                Err(err) => {
                    warn!("Failed to import legacy database: {:?}", err);
                    Err(())
                }
            }
        }
    }

    pub fn create_bucket(
        &mut self,
        conn: &Connection,
        mut bucket: Bucket,
    ) -> Result<(), DatastoreError> {
        // The cache is authoritative for existence; checking it here avoids
        // relying on DuckDB-specific constraint-violation error matching.
        if self.buckets_cache.contains_key(&bucket.id) {
            return Err(DatastoreError::BucketAlreadyExists(bucket.id.to_string()));
        }

        bucket.created = match bucket.created {
            Some(created) => Some(created),
            None => Some(Utc::now()),
        };
        let created_str = bucket.created.unwrap().to_rfc3339();
        let data = serde_json::to_string(&bucket.data).unwrap();

        let mut stmt = match conn.prepare_cached(
            "
                INSERT INTO buckets (name, \"type\", client, hostname, created, data)
                VALUES (?, ?, ?, ?, ?, ?)
                RETURNING id",
        ) {
            Ok(stmt) => stmt,
            Err(err) => {
                return Err(DatastoreError::InternalError(format!(
                    "Failed to prepare create_bucket SQL statement: {err}"
                )))
            }
        };
        let rowid: Result<i64, duckdb::Error> = stmt.query_row(
            params![
                bucket.id,
                bucket._type,
                bucket.client,
                bucket.hostname,
                created_str,
                data,
            ],
            |row| row.get(0),
        );

        match rowid {
            Ok(rowid) => {
                info!("Created bucket {}", bucket.id);
                bucket.bid = Some(rowid);
                // Take out events from struct before caching
                let events = bucket.events;
                bucket.events = None;
                // Cache bucket
                self.buckets_cache.insert(bucket.id.clone(), bucket.clone());
                // Insert events
                if let Some(events) = events {
                    self.insert_events(conn, &bucket.id, events.take_inner())?;
                    bucket.events = None;
                }
                Ok(())
            }
            Err(err) => Err(DatastoreError::InternalError(format!(
                "Failed to execute create_bucket SQL statement: {err}"
            ))),
        }
    }

    pub fn delete_bucket(
        &mut self,
        conn: &Connection,
        bucket_id: &str,
    ) -> Result<(), DatastoreError> {
        let bucket = (self.get_bucket(bucket_id))?;
        // Delete all events in bucket
        match conn.execute("DELETE FROM events WHERE bucketrow = ?", [&bucket.bid]) {
            Ok(_) => (),
            Err(err) => return Err(DatastoreError::InternalError(err.to_string())),
        }
        // Delete bucket itself
        match conn.execute("DELETE FROM buckets WHERE id = ?", [&bucket.bid]) {
            Ok(_) => {
                self.buckets_cache.remove(bucket_id);
                Ok(())
            }
            Err(err) => Err(DatastoreError::InternalError(err.to_string())),
        }
    }

    pub fn get_bucket(&self, bucket_id: &str) -> Result<Bucket, DatastoreError> {
        let cached_bucket = self.buckets_cache.get(bucket_id);
        match cached_bucket {
            Some(bucket) => Ok(bucket.clone()),
            None => Err(DatastoreError::NoSuchBucket(bucket_id.to_string())),
        }
    }

    pub fn get_buckets(&self) -> HashMap<String, Bucket> {
        self.buckets_cache.clone()
    }

    pub fn insert_events(
        &mut self,
        conn: &Connection,
        bucket_id: &str,
        mut events: Vec<Event>,
    ) -> Result<Vec<Event>, DatastoreError> {
        let mut bucket = self.get_bucket(bucket_id)?;
        let bid = bucket.bid.unwrap();
        let mut saw_explicit_id = false;

        for event in &mut events {
            let starttime_nanos = event.timestamp.timestamp_nanos_opt().unwrap();
            let duration_nanos = match event.duration.num_nanoseconds() {
                Some(nanos) => nanos,
                None => {
                    return Err(DatastoreError::InternalError(
                        "Failed to convert duration to nanoseconds".to_string(),
                    ))
                }
            };
            let endtime_nanos = starttime_nanos + duration_nanos;
            let data = serde_json::to_string(&event.data).unwrap();

            // New events let the sequence assign the id; events that arrive with
            // an explicit id (e.g. from sync/import) upsert on the primary key.
            let new_id: Result<i64, duckdb::Error> = match event.id {
                Some(id) => {
                    saw_explicit_id = true;
                    let mut stmt = conn.prepare_cached(
                        "INSERT OR REPLACE INTO events(id, bucketrow, starttime, endtime, data)
                         VALUES (?, ?, ?, ?, ?) RETURNING id",
                    )?;
                    stmt.query_row(
                        params![id, bid, starttime_nanos, endtime_nanos, data],
                        |row| row.get(0),
                    )
                }
                None => {
                    let mut stmt = conn.prepare_cached(
                        "INSERT INTO events(bucketrow, starttime, endtime, data)
                         VALUES (?, ?, ?, ?) RETURNING id",
                    )?;
                    stmt.query_row(params![bid, starttime_nanos, endtime_nanos, data], |row| {
                        row.get(0)
                    })
                }
            };
            match new_id {
                Ok(id) => {
                    self.update_endtime(&mut bucket, event);
                    event.id = Some(id);
                }
                Err(err) => {
                    return Err(DatastoreError::InternalError(format!(
                        "Failed to insert event: {event:?}, {err}"
                    )));
                }
            };
        }
        // Explicit ids bypass the sequence, so realign it to avoid a later
        // auto-id insert colliding on the primary key.
        if saw_explicit_id {
            _resync_sequences(conn)?;
        }
        Ok(events)
    }

    pub fn delete_events_by_id(
        &self,
        conn: &Connection,
        bucket_id: &str,
        event_ids: Vec<i64>,
    ) -> Result<(), DatastoreError> {
        let bucket = self.get_bucket(bucket_id)?;
        let bid = bucket.bid.unwrap();
        let mut stmt = match conn.prepare_cached(
            "
                DELETE FROM events
                WHERE bucketrow = ? AND id = ?",
        ) {
            Ok(stmt) => stmt,
            Err(err) => {
                return Err(DatastoreError::InternalError(format!(
                    "Failed to prepare delete_events SQL statement: {err}"
                )))
            }
        };
        for id in event_ids {
            let res = stmt.execute(params![bid, id]);
            match res {
                Ok(_) => {}
                Err(err) => {
                    return Err(DatastoreError::InternalError(format!(
                        "Failed to delete event with id {id} in bucket {bucket_id}: {err:?}"
                    )));
                }
            };
        }
        Ok(())
    }

    // TODO: Function for deleting events by timerange with limit

    fn update_endtime(&mut self, bucket: &mut Bucket, event: &Event) {
        let mut update = false;
        /* Potentially update start */
        match bucket.metadata.start {
            None => {
                bucket.metadata.start = Some(event.timestamp);
                update = true;
            }
            Some(current_start) => {
                if current_start > event.timestamp {
                    bucket.metadata.start = Some(event.timestamp);
                    update = true;
                }
            }
        }
        /* Potentially update end */
        let event_endtime = event.calculate_endtime();
        match bucket.metadata.end {
            None => {
                bucket.metadata.end = Some(event_endtime);
                update = true;
            }
            Some(current_end) => {
                if current_end < event_endtime {
                    bucket.metadata.end = Some(event_endtime);
                    update = true;
                }
            }
        }
        /* Update buckets_cache if start or end has been updated */
        if update {
            self.buckets_cache.insert(bucket.id.clone(), bucket.clone());
        }
    }

    pub fn replace_last_event(
        &mut self,
        conn: &Connection,
        bucket_id: &str,
        event_id: i64,
        event: &Event,
    ) -> Result<(), DatastoreError> {
        let mut bucket = self.get_bucket(bucket_id)?;
        let bid = bucket.bid.unwrap();

        // Use event ID directly instead of max(endtime) to avoid mismatch with get_events ordering
        let mut stmt = match conn.prepare_cached(
            "
                UPDATE events
                SET starttime = ?, endtime = ?, data = ?
                WHERE bucketrow = ? AND id = ?
            ",
        ) {
            Ok(stmt) => stmt,
            Err(err) => {
                return Err(DatastoreError::InternalError(format!(
                    "Failed to prepare replace_last_event SQL statement: {err}"
                )))
            }
        };
        let starttime_nanos = event.timestamp.timestamp_nanos_opt().unwrap();
        let duration_nanos = match event.duration.num_nanoseconds() {
            Some(nanos) => nanos,
            None => {
                return Err(DatastoreError::InternalError(
                    "Failed to convert duration to nanoseconds".to_string(),
                ))
            }
        };
        let endtime_nanos = starttime_nanos + duration_nanos;
        let data = serde_json::to_string(&event.data).unwrap();
        match stmt.execute(params![starttime_nanos, endtime_nanos, data, bid, event_id]) {
            Ok(0) => Err(DatastoreError::InternalError(format!(
                "replace_last_event matched 0 rows for event_id {event_id} - cache/DB inconsistency"
            ))),
            Ok(_) => {
                self.update_endtime(&mut bucket, event);
                Ok(())
            }
            Err(err) => Err(DatastoreError::InternalError(format!(
                "Failed to execute replace_last_event SQL statement: {err}"
            ))),
        }
    }

    pub fn heartbeat(
        &mut self,
        conn: &Connection,
        bucket_id: &str,
        heartbeat: Event,
        pulsetime: f64,
        last_heartbeat: &mut HashMap<String, Option<Event>>,
    ) -> Result<Event, DatastoreError> {
        self.get_bucket(bucket_id)?;
        if !last_heartbeat.contains_key(bucket_id) {
            last_heartbeat.insert(bucket_id.to_string(), None);
        }
        let last_event = match last_heartbeat.remove(bucket_id).unwrap() {
            // last heartbeat is in cache
            Some(last_event) => last_event,
            None => {
                // last heartbeat was not in cache, fetch from DB
                let mut last_event_vec = self.get_events(conn, bucket_id, None, None, Some(1))?;
                match last_event_vec.pop() {
                    Some(last_event) => last_event,
                    None => {
                        // There was no last event, insert and return
                        let mut inserted = self.insert_events(conn, bucket_id, vec![heartbeat])?;
                        return Ok(inserted.pop().unwrap());
                    }
                }
            }
        };
        let inserted_heartbeat = match aw_transform::heartbeat(&last_event, &heartbeat, pulsetime) {
            Some(mut merged_heartbeat) => {
                debug!("Merged heartbeat successfully");
                // Use the event ID from last_event to ensure we update the correct row
                let event_id = last_event.id.ok_or_else(|| {
                    DatastoreError::InternalError("last_event has no ID".to_string())
                })?;
                self.replace_last_event(conn, bucket_id, event_id, &merged_heartbeat)?;
                // Preserve the event ID on the cached heartbeat so subsequent
                // heartbeats can look it up for replace_last_event
                merged_heartbeat.id = Some(event_id);
                merged_heartbeat
            }
            None => {
                debug!("Failed to merge heartbeat");
                // insert_events sets the ID on the events in the vec, so use the
                // returned event (with ID) instead of the original heartbeat
                let mut inserted = self.insert_events(conn, bucket_id, vec![heartbeat])?;
                inserted.pop().unwrap()
            }
        };
        last_heartbeat.insert(bucket_id.to_string(), Some(inserted_heartbeat.clone()));
        Ok(inserted_heartbeat)
    }

    pub fn get_event(
        &mut self,
        conn: &Connection,
        bucket_id: &str,
        event_id: i64,
    ) -> Result<Event, DatastoreError> {
        let bid = self.get_bucket(bucket_id)?.bid.unwrap();
        query_event(conn, bid, event_id)
    }

    fn get_events_inner(
        &mut self,
        conn: &Connection,
        bucket_id: &str,
        starttime_opt: Option<DateTime<Utc>>,
        endtime_opt: Option<DateTime<Utc>>,
        limit_opt: Option<u64>,
        clip_to_query_range: bool,
    ) -> Result<Vec<Event>, DatastoreError> {
        let bid = self.get_bucket(bucket_id)?.bid.unwrap();
        query_events(
            conn,
            bid,
            bucket_id,
            starttime_opt,
            endtime_opt,
            limit_opt,
            clip_to_query_range,
            None,
        )
    }

    pub fn get_events(
        &mut self,
        conn: &Connection,
        bucket_id: &str,
        starttime_opt: Option<DateTime<Utc>>,
        endtime_opt: Option<DateTime<Utc>>,
        limit_opt: Option<u64>,
    ) -> Result<Vec<Event>, DatastoreError> {
        self.get_events_inner(conn, bucket_id, starttime_opt, endtime_opt, limit_opt, true)
    }

    pub fn get_events_unclipped(
        &mut self,
        conn: &Connection,
        bucket_id: &str,
        starttime_opt: Option<DateTime<Utc>>,
        endtime_opt: Option<DateTime<Utc>>,
        limit_opt: Option<u64>,
    ) -> Result<Vec<Event>, DatastoreError> {
        self.get_events_inner(
            conn,
            bucket_id,
            starttime_opt,
            endtime_opt,
            limit_opt,
            false,
        )
    }

    pub fn get_events_filtered(
        &mut self,
        conn: &Connection,
        bucket_id: &str,
        starttime_opt: Option<DateTime<Utc>>,
        endtime_opt: Option<DateTime<Utc>>,
        limit_opt: Option<u64>,
        filters: &[crate::EventFilter],
    ) -> Result<Vec<Event>, DatastoreError> {
        let bid = self.get_bucket(bucket_id)?.bid.unwrap();
        query_events(
            conn,
            bid,
            bucket_id,
            starttime_opt,
            endtime_opt,
            limit_opt,
            true,
            Some(filters),
        )
    }

    pub fn get_events_grouped(
        &mut self,
        conn: &Connection,
        bucket_id: &str,
        starttime_opt: Option<DateTime<Utc>>,
        endtime_opt: Option<DateTime<Utc>>,
        group_by_key: &str,
    ) -> Result<Vec<Event>, DatastoreError> {
        let bid = self.get_bucket(bucket_id)?.bid.unwrap();
        query_events_grouped(
            conn,
            bid,
            bucket_id,
            starttime_opt,
            endtime_opt,
            group_by_key,
        )
    }

    pub fn get_events_intersected(
        &mut self,
        conn: &Connection,
        target_bucket_id: &str,
        filter_bucket_id: &str,
        starttime_opt: Option<DateTime<Utc>>,
        endtime_opt: Option<DateTime<Utc>>,
        filter_key: &str,
        filter_val: &serde_json::Value,
    ) -> Result<Vec<Event>, DatastoreError> {
        let target_bid = self.get_bucket(target_bucket_id)?.bid.unwrap();
        let filter_bid = self.get_bucket(filter_bucket_id)?.bid.unwrap();
        query_events_intersected(
            conn,
            target_bid,
            filter_bid,
            target_bucket_id,
            starttime_opt,
            endtime_opt,
            filter_key,
            filter_val,
        )
    }

    pub fn get_event_count(
        &self,
        conn: &Connection,
        bucket_id: &str,
        starttime_opt: Option<DateTime<Utc>>,
        endtime_opt: Option<DateTime<Utc>>,
    ) -> Result<i64, DatastoreError> {
        let bid = self.get_bucket(bucket_id)?.bid.unwrap();
        query_event_count(conn, bid, starttime_opt, endtime_opt)
    }

    pub fn insert_key_value(
        &self,
        conn: &Connection,
        key: &str,
        data: &str,
    ) -> Result<(), DatastoreError> {
        let mut stmt = match conn.prepare_cached(
            "
                INSERT OR REPLACE INTO key_value(key, value, last_modified)
                VALUES (?, ?, ?)",
        ) {
            Ok(stmt) => stmt,
            Err(err) => {
                return Err(DatastoreError::InternalError(format!(
                    "Failed to prepare insert_value SQL statement: {err}"
                )))
            }
        };
        let timestamp = Utc::now().timestamp();
        #[allow(clippy::expect_fun_call)]
        stmt.execute(params![key, data, timestamp])
            .expect(&format!("Failed to insert key-value pair: {key}"));
        Ok(())
    }

    pub fn delete_key_value(&self, conn: &Connection, key: &str) -> Result<(), DatastoreError> {
        conn.execute("DELETE FROM key_value WHERE key = ?", [key])
            .expect("Error deleting value from database");
        Ok(())
    }

    pub fn get_key_value(&self, conn: &Connection, key: &str) -> Result<String, DatastoreError> {
        let mut stmt = match conn.prepare_cached("SELECT value FROM key_value WHERE key = ?") {
            Ok(stmt) => stmt,
            Err(err) => {
                return Err(DatastoreError::InternalError(format!(
                    "Failed to prepare get_value SQL statement: {err}"
                )))
            }
        };

        match stmt.query_row([key], |row| row.get(0)) {
            Ok(result) => Ok(result),
            Err(err) => match err {
                duckdb::Error::QueryReturnedNoRows => {
                    Err(DatastoreError::NoSuchKey(key.to_string()))
                }
                _ => Err(DatastoreError::InternalError(format!(
                    "Get value query failed for key {key}"
                ))),
            },
        }
    }

    pub fn get_key_values(
        &self,
        conn: &Connection,
        pattern: &str,
    ) -> Result<HashMap<String, String>, DatastoreError> {
        let mut stmt =
            match conn.prepare_cached("SELECT key, value FROM key_value WHERE key LIKE ?") {
                Ok(stmt) => stmt,
                Err(err) => {
                    return Err(DatastoreError::InternalError(format!(
                        "Failed to prepare get_value SQL statement: {err}"
                    )))
                }
            };

        let mut output = HashMap::<String, String>::new();
        let result = stmt.query_map([pattern], |row| {
            Ok((row.get::<usize, String>(0)?, row.get::<usize, String>(1)?))
        });
        match result {
            Ok(settings) => {
                for row in settings {
                    // Unwrap to String or panic on SQL row if type is invalid. Can't happen with a
                    // properly initialized table.
                    let (key, value) = row.unwrap();
                    // Only return keys starting with "settings.".
                    if !key.starts_with("settings.") {
                        continue;
                    }
                    output.insert(key, value);
                }
                Ok(output)
            }
            Err(err) => match err {
                duckdb::Error::QueryReturnedNoRows => Ok(output),
                _ => Err(DatastoreError::InternalError(
                    "Failed to get settings".to_string(),
                )),
            },
        }
    }
}
