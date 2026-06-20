#[macro_use]
extern crate log;

use std::fmt;

#[macro_export]
macro_rules! json_map {
    { $( $key:literal : $value:expr),* } => {{
        use serde_json::Value;
        use serde_json::map::Map;
        #[allow(unused_mut)]
        let mut map : Map<String, Value> = Map::new();
        $(
          map.insert( $key.to_string(), json!($value) );
        )*
        map
    }};
}

mod datastore;
mod legacy_import;
mod privacy_filter;
mod worker;

pub use self::datastore::DatastoreInstance;
pub use self::worker::Datastore;

#[derive(Clone, Debug)]
pub struct EventFilter {
    pub key: String,
    pub vals: Vec<serde_json::Value>,
}

#[derive(Clone)]
pub enum DatastoreMethod {
    Memory(),
    File(String),
}

impl fmt::Debug for DatastoreMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DatastoreMethod::Memory() => write!(f, "Memory()"),
            DatastoreMethod::File(p) => write!(f, "File({p:?})"),
        }
    }
}

/* TODO: Implement this as a proper error */
#[derive(Debug, Clone)]
pub enum DatastoreError {
    NoSuchBucket(String),
    BucketAlreadyExists(String),
    NoSuchKey(String),
    MpscError,
    InternalError(String),
    // Errors specific to when migrate is disabled
    Uninitialized(String),
    OldDbVersion(String),
}

impl From<duckdb::Error> for DatastoreError {
    fn from(err: duckdb::Error) -> Self {
        DatastoreError::InternalError(err.to_string())
    }
}
