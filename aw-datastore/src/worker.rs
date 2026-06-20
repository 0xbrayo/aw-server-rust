use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;
use std::thread;

use chrono::DateTime;
use chrono::Duration;
use chrono::Utc;

use duckdb::Connection;

use aw_models::Bucket;
use aw_models::Event;

use crate::privacy_filter::PrivacyFilterEngine;
use crate::DatastoreError;
use crate::DatastoreInstance;
use crate::DatastoreMethod;

type RequestSender = mpsc_requests::RequestSender<Command, Result<Response, DatastoreError>>;
type RequestReceiver = mpsc_requests::RequestReceiver<Command, Result<Response, DatastoreError>>;

#[derive(Clone)]
pub struct Datastore {
    requester: RequestSender,
    // Read-only side channel for event reads on file-backed stores. Lets a slow
    // webui query run on its own connection in parallel instead of queueing
    // behind watcher writes on the single worker thread. `None` for in-memory
    // stores, where reads stay on the worker. See `Reader`.
    reader: Option<Arc<Reader>>,
}

impl fmt::Debug for Datastore {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Datastore()")
    }
}

/// Read-only access path that bypasses the worker thread.
///
/// DuckDB allows only one handle to open a database file; additional
/// connections must be *clones* of that handle (`try_clone`), which share the
/// single in-process database instance and its MVCC. This pool holds such
/// clones so a webui query can read concurrently with the writer and with other
/// readers.
///
/// **Consistency:** clones see only *committed* data (DuckDB MVCC snapshots).
/// The worker batches writes and commits at most every ~15s (or every 100
/// events, or on `force_commit`), so reads here can lag the newest heartbeats by
/// that window — acceptable for the dashboards/timelines the webui renders.
struct Reader {
    /// A connection to the shared DuckDB instance, cloned to mint pool members.
    template: Mutex<Connection>,
    /// Idle connections available for reuse, capped at `max_idle`.
    idle: Mutex<Vec<Connection>>,
    max_idle: usize,
    /// bucket name -> row id. Bucket ids are stable for a bucket's lifetime and
    /// creation/deletion force a commit, so a name resolved once stays valid.
    bids: RwLock<HashMap<String, i64>>,
}

/// A connection borrowed from the [`Reader`] pool, returned on drop.
struct PooledConn {
    conn: Option<Connection>,
    reader: Arc<Reader>,
}

impl PooledConn {
    fn conn(&self) -> &Connection {
        self.conn.as_ref().expect("connection taken")
    }
}

impl Drop for PooledConn {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            let mut idle = self.reader.idle.lock().unwrap();
            if idle.len() < self.reader.max_idle {
                idle.push(conn);
            }
            // else: over the idle cap, just drop and close this connection.
        }
    }
}

impl Reader {
    fn make_connection(&self) -> Result<Connection, DatastoreError> {
        let template = self.template.lock().unwrap();
        template.try_clone().map_err(|e| {
            DatastoreError::InternalError(format!("Failed to clone read connection: {e}"))
        })
    }

    fn checkout(self: &Arc<Self>) -> Result<PooledConn, DatastoreError> {
        let reused = self.idle.lock().unwrap().pop();
        let conn = match reused {
            Some(conn) => conn,
            None => self.make_connection()?,
        };
        Ok(PooledConn {
            conn: Some(conn),
            reader: Arc::clone(self),
        })
    }

    fn resolve_bid(&self, conn: &Connection, bucket_id: &str) -> Result<i64, DatastoreError> {
        if let Some(bid) = self.bids.read().unwrap().get(bucket_id).copied() {
            return Ok(bid);
        }
        let bid = crate::datastore::query_bid(conn, bucket_id)?;
        self.bids
            .write()
            .unwrap()
            .insert(bucket_id.to_string(), bid);
        Ok(bid)
    }

    /// Drop a cached name->id mapping. Must be called whenever a bucket is
    /// deleted or (re)created on the worker, since a recreated bucket gets a new
    /// row id from the sequence and the stale id would otherwise be served.
    fn invalidate_bid(&self, bucket_id: &str) {
        self.bids.write().unwrap().remove(bucket_id);
    }

    fn get_events(
        self: &Arc<Self>,
        bucket_id: &str,
        starttime_opt: Option<DateTime<Utc>>,
        endtime_opt: Option<DateTime<Utc>>,
        limit_opt: Option<u64>,
        clip: bool,
    ) -> Result<Vec<Event>, DatastoreError> {
        let pooled = self.checkout()?;
        let conn = pooled.conn();
        let bid = self.resolve_bid(conn, bucket_id)?;
        crate::datastore::query_events(
            conn,
            bid,
            bucket_id,
            starttime_opt,
            endtime_opt,
            limit_opt,
            clip,
            None,
        )
    }

    fn get_events_filtered(
        self: &Arc<Self>,
        bucket_id: &str,
        starttime_opt: Option<DateTime<Utc>>,
        endtime_opt: Option<DateTime<Utc>>,
        limit_opt: Option<u64>,
        filters: &[crate::EventFilter],
    ) -> Result<Vec<Event>, DatastoreError> {
        let pooled = self.checkout()?;
        let conn = pooled.conn();
        let bid = self.resolve_bid(conn, bucket_id)?;
        crate::datastore::query_events(
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

    fn get_event(
        self: &Arc<Self>,
        bucket_id: &str,
        event_id: i64,
    ) -> Result<Event, DatastoreError> {
        let pooled = self.checkout()?;
        let conn = pooled.conn();
        let bid = self.resolve_bid(conn, bucket_id)?;
        crate::datastore::query_event(conn, bid, event_id)
    }

    fn get_events_grouped(
        self: &Arc<Self>,
        bucket_id: &str,
        starttime_opt: Option<DateTime<Utc>>,
        endtime_opt: Option<DateTime<Utc>>,
        group_by_key: &str,
    ) -> Result<Vec<Event>, DatastoreError> {
        let pooled = self.checkout()?;
        let conn = pooled.conn();
        let bid = self.resolve_bid(conn, bucket_id)?;
        crate::datastore::query_events_grouped(
            conn,
            bid,
            bucket_id,
            starttime_opt,
            endtime_opt,
            group_by_key,
        )
    }

    fn get_events_intersected(
        self: &Arc<Self>,
        target_bucket_id: &str,
        filter_bucket_id: &str,
        starttime_opt: Option<DateTime<Utc>>,
        endtime_opt: Option<DateTime<Utc>>,
        filter_key: &str,
        filter_val: &serde_json::Value,
    ) -> Result<Vec<Event>, DatastoreError> {
        let pooled = self.checkout()?;
        let conn = pooled.conn();
        let target_bid = self.resolve_bid(conn, target_bucket_id)?;
        let filter_bid = self.resolve_bid(conn, filter_bucket_id)?;
        crate::datastore::query_events_intersected(
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

    fn get_event_count(
        self: &Arc<Self>,
        bucket_id: &str,
        starttime_opt: Option<DateTime<Utc>>,
        endtime_opt: Option<DateTime<Utc>>,
    ) -> Result<i64, DatastoreError> {
        let pooled = self.checkout()?;
        let conn = pooled.conn();
        let bid = self.resolve_bid(conn, bucket_id)?;
        crate::datastore::query_event_count(conn, bid, starttime_opt, endtime_opt)
    }
}

/*
 * TODO: Add an separate "Import" request which does an import with an transaction
 */

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum Response {
    Empty(),
    Bucket(Bucket),
    BucketMap(HashMap<String, Bucket>),
    Event(Event),
    EventList(Vec<Event>),
    Count(i64),
    KeyValue(String),
    KeyValues(HashMap<String, String>),
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum Command {
    CreateBucket(Bucket),
    DeleteBucket(String),
    GetBucket(String),
    GetBuckets(),
    InsertEvents(String, Vec<Event>),
    Heartbeat(String, Event, f64),
    GetEvent(String, i64),
    GetEvents(
        String,
        Option<DateTime<Utc>>,
        Option<DateTime<Utc>>,
        Option<u64>,
        bool,
    ),
    GetEventCount(String, Option<DateTime<Utc>>, Option<DateTime<Utc>>),
    DeleteEventsById(String, Vec<i64>),
    ForceCommit(),
    GetKeyValues(String),
    GetKeyValue(String),
    SetKeyValue(String, String),
    DeleteKeyValue(String),
    RefreshPrivacyFilter(),
    Close(),
}

fn _unwrap_empty_response(response: Response) -> Result<(), DatastoreError> {
    match response {
        Response::Empty() => Ok(()),
        _ => panic!("Invalid response"),
    }
}

struct DatastoreWorker {
    responder: RequestReceiver,
    legacy_import: bool,
    quit: bool,
    uncommitted_events: usize,
    commit: bool,
    last_heartbeat: HashMap<String, Option<Event>>,
    privacy_engine: PrivacyFilterEngine,
}

impl DatastoreWorker {
    pub fn new(
        responder: mpsc_requests::RequestReceiver<Command, Result<Response, DatastoreError>>,
        legacy_import: bool,
    ) -> Self {
        DatastoreWorker {
            responder,
            legacy_import,
            quit: false,
            uncommitted_events: 0,
            commit: false,
            last_heartbeat: HashMap::new(),
            privacy_engine: PrivacyFilterEngine::new(vec![]),
        }
    }

    fn work_loop(&mut self, conn: Connection, mut ds: DatastoreInstance) {
        // Ensure legacy import (a no-op on the DuckDB backend, kept for parity).
        if self.legacy_import {
            if let Err(err) = conn.execute_batch("BEGIN TRANSACTION") {
                panic!("Unable to start transaction for legacy import: {err}");
            }
            match ds.ensure_legacy_import(&conn) {
                Ok(_) => (),
                Err(err) => error!("Failed to do legacy import: {:?}", err),
            }
            if let Err(err) = conn.execute_batch("COMMIT") {
                error!("Failed to commit legacy import transaction: {err}");
                let _ = conn.execute_batch("ROLLBACK");
            }
        }

        // Start handling and respond to requests
        loop {
            let last_commit_time: DateTime<Utc> = Utc::now();
            // DuckDB has no "immediate" transaction behavior; a plain BEGIN
            // starts the batch. Writes accumulate until we COMMIT below.
            if let Err(err) = conn.execute_batch("BEGIN TRANSACTION") {
                error!("Unable to start transaction! {:?}", err);
                std::thread::sleep(std::time::Duration::from_millis(1000));
                continue;
            }

            self.uncommitted_events = 0;
            self.commit = false;
            // ForceCommit and Close promise the caller that their data is
            // committed, so their acks are held back until the transaction
            // below has actually committed. All other commands are acked
            // immediately so a watcher heartbeat does not wait for the batch.
            let mut deferred_ack = None;
            let mut transaction_aborted = false;
            loop {
                let (request, response_sender) = match self.responder.poll() {
                    Ok((req, res_sender)) => (req, res_sender),
                    Err(err) => {
                        // All references to responder is gone, quit
                        error!("DB worker quitting, error: {err:?}");
                        self.quit = true;
                        break;
                    }
                };
                let ack_after_commit = matches!(request, Command::ForceCommit() | Command::Close());
                let response = self.handle_request(request, &mut ds, &conn);
                if ack_after_commit {
                    // Both commands force a commit, so the loop ends here.
                    deferred_ack = Some((response_sender, response));
                    break;
                }
                // A failed write statement aborts the whole DuckDB transaction:
                // every later statement (and the COMMIT) would then fail too. Stop
                // the batch and roll back so the next iteration starts clean.
                // Logical errors (NoSuchBucket, BucketAlreadyExists) leave the
                // transaction intact, so only InternalError — which wraps duckdb
                // failures — triggers the abort.
                let aborted = matches!(&response, Err(DatastoreError::InternalError(_)));
                response_sender.respond(response);
                if aborted {
                    transaction_aborted = true;
                    break;
                }

                let now: DateTime<Utc> = Utc::now();
                let commit_interval_passed: bool = (now - last_commit_time) > Duration::seconds(15);
                if self.commit
                    || commit_interval_passed
                    || self.uncommitted_events > 100
                    || self.quit
                {
                    break;
                };
            }
            if transaction_aborted {
                error!(
                    "Rolling back datastore transaction after a failed statement ({} events lost)",
                    self.uncommitted_events
                );
                if let Err(err) = conn.execute_batch("ROLLBACK") {
                    error!("Failed to roll back aborted transaction: {err}");
                }
                if self.quit {
                    break;
                };
                continue;
            }
            debug!(
                "Committing DB! Force commit {}, {} uncommitted events",
                self.commit, self.uncommitted_events
            );
            match conn.execute_batch("COMMIT") {
                Ok(_) => {
                    if let Some((sender, response)) = deferred_ack.take() {
                        sender.respond(response);
                    }
                }
                Err(err) => {
                    error!(
                        "Failed to commit datastore transaction ({} events lost): {err}",
                        self.uncommitted_events
                    );
                    let _ = conn.execute_batch("ROLLBACK");
                    if let Some((sender, _)) = deferred_ack.take() {
                        sender.respond(Err(DatastoreError::InternalError(format!(
                            "Failed to commit datastore transaction: {err}"
                        ))));
                    }
                }
            }
            if self.quit {
                break;
            };
        }
        info!("DB Worker thread finished");
    }

    fn handle_request(
        &mut self,
        request: Command,
        ds: &mut DatastoreInstance,
        conn: &Connection,
    ) -> Result<Response, DatastoreError> {
        match request {
            Command::CreateBucket(bucket) => match ds.create_bucket(conn, bucket) {
                Ok(_) => {
                    self.commit = true;
                    Ok(Response::Empty())
                }
                Err(e) => Err(e),
            },
            Command::DeleteBucket(bucketname) => match ds.delete_bucket(conn, &bucketname) {
                Ok(_) => {
                    self.commit = true;
                    Ok(Response::Empty())
                }
                Err(e) => Err(e),
            },
            Command::GetBucket(bucketname) => match ds.get_bucket(&bucketname) {
                Ok(b) => Ok(Response::Bucket(b)),
                Err(e) => Err(e),
            },
            Command::GetBuckets() => Ok(Response::BucketMap(ds.get_buckets())),
            Command::InsertEvents(bucketname, events) => {
                let filtered = self.privacy_engine.filter_events(&bucketname, events);
                if filtered.is_empty() {
                    return Ok(Response::EventList(vec![]));
                }
                match ds.insert_events(conn, &bucketname, filtered) {
                    Ok(events) => {
                        self.uncommitted_events += events.len();
                        self.last_heartbeat.insert(bucketname.to_string(), None); // invalidate last_heartbeat cache
                        Ok(Response::EventList(events))
                    }
                    Err(e) => Err(e),
                }
            }
            Command::Heartbeat(bucketname, event, pulsetime) => {
                // Apply privacy filter to heartbeat
                let filtered = match self.privacy_engine.filter_event(&bucketname, event.clone()) {
                    Some(event) => event,
                    None => {
                        let last = self
                            .last_heartbeat
                            .get(&bucketname)
                            .and_then(|e| e.clone())
                            .unwrap_or(event);
                        return Ok(Response::Event(last));
                    }
                };
                match ds.heartbeat(
                    conn,
                    &bucketname,
                    filtered,
                    pulsetime,
                    &mut self.last_heartbeat,
                ) {
                    Ok(e) => {
                        self.uncommitted_events += 1;
                        Ok(Response::Event(e))
                    }
                    Err(e) => Err(e),
                }
            }
            Command::GetEvent(bucketname, event_id) => {
                match ds.get_event(conn, &bucketname, event_id) {
                    Ok(el) => Ok(Response::Event(el)),
                    Err(e) => Err(e),
                }
            }
            Command::GetEvents(bucketname, starttime_opt, endtime_opt, limit_opt, unclipped) => {
                let result = if unclipped {
                    ds.get_events_unclipped(
                        conn,
                        &bucketname,
                        starttime_opt,
                        endtime_opt,
                        limit_opt,
                    )
                } else {
                    ds.get_events(conn, &bucketname, starttime_opt, endtime_opt, limit_opt)
                };
                match result {
                    Ok(el) => Ok(Response::EventList(el)),
                    Err(e) => Err(e),
                }
            }
            Command::GetEventCount(bucketname, starttime_opt, endtime_opt) => {
                match ds.get_event_count(conn, &bucketname, starttime_opt, endtime_opt) {
                    Ok(n) => Ok(Response::Count(n)),
                    Err(e) => Err(e),
                }
            }
            Command::DeleteEventsById(bucketname, event_ids) => {
                match ds.delete_events_by_id(conn, &bucketname, event_ids) {
                    Ok(()) => Ok(Response::Empty()),
                    Err(e) => Err(e),
                }
            }
            Command::ForceCommit() => {
                self.commit = true;
                Ok(Response::Empty())
            }
            Command::GetKeyValues(pattern) => match ds.get_key_values(conn, pattern.as_str()) {
                Ok(result) => Ok(Response::KeyValues(result)),
                Err(e) => Err(e),
            },
            Command::SetKeyValue(key, data) => match ds.insert_key_value(conn, &key, &data) {
                Ok(()) => Ok(Response::Empty()),
                Err(e) => Err(e),
            },
            Command::GetKeyValue(key) => match ds.get_key_value(conn, &key) {
                Ok(result) => Ok(Response::KeyValue(result)),
                Err(e) => Err(e),
            },
            Command::DeleteKeyValue(key) => match ds.delete_key_value(conn, &key) {
                Ok(()) => Ok(Response::Empty()),
                Err(e) => Err(e),
            },
            Command::RefreshPrivacyFilter() => {
                // Reload privacy filter rules from settings
                match ds.get_key_value(conn, "settings.privacy_filters") {
                    Ok(json_str) => match PrivacyFilterEngine::from_json(&json_str) {
                        Ok(engine) => self.privacy_engine = engine,
                        Err(e) => warn!("Failed to parse privacy_filters setting: {e}"),
                    },
                    Err(_) => {
                        // Settings key absent — clear rules so removing the key disables filtering
                        self.privacy_engine = PrivacyFilterEngine::new(vec![]);
                    }
                }
                Ok(Response::Empty())
            }
            Command::Close() => {
                self.quit = true;
                Ok(Response::Empty())
            }
        }
    }
}

fn open_connection(method: &DatastoreMethod) -> Connection {
    match method {
        DatastoreMethod::Memory() => {
            Connection::open_in_memory().expect("Failed to create in-memory datastore")
        }
        DatastoreMethod::File(path) => Connection::open(path).expect("Failed to create datastore"),
    }
}

impl Datastore {
    pub fn new(dbpath: String, legacy_import: bool) -> Self {
        let method = DatastoreMethod::File(dbpath);
        Datastore::_new_internal(method, legacy_import)
    }

    pub fn new_in_memory(legacy_import: bool) -> Self {
        let method = DatastoreMethod::Memory();
        Datastore::_new_internal(method, legacy_import)
    }

    fn _new_internal(method: DatastoreMethod, legacy_import: bool) -> Self {
        let (requester, responder) =
            mpsc_requests::channel::<Command, Result<Response, DatastoreError>>();

        // Open the single DuckDB connection up front and initialise the schema,
        // so a read pool can be built from clones of the same in-process
        // instance (DuckDB permits only one handle to open the file).
        let conn = open_connection(&method);
        let ds = DatastoreInstance::new(&conn, true).expect("Failed to initialise datastore");

        // In-memory stores route reads through the worker for simplicity (the
        // worker holds the only handle needed). File-backed stores get a read
        // pool of cloned connections unless explicitly disabled (the pool reads
        // only committed data, so strict read-your-writes can opt out here).
        let read_pool_disabled = std::env::var_os("AW_DATASTORE_DISABLE_READ_POOL").is_some();
        let reader = match &method {
            _ if read_pool_disabled => None,
            DatastoreMethod::Memory() => None,
            DatastoreMethod::File(_) => {
                let template = conn
                    .try_clone()
                    .expect("Failed to clone read connection template");
                Some(Arc::new(Reader {
                    template: Mutex::new(template),
                    idle: Mutex::new(Vec::new()),
                    max_idle: 4,
                    bids: RwLock::new(HashMap::new()),
                }))
            }
        };

        let _thread = thread::spawn(move || {
            let mut di = DatastoreWorker::new(responder, legacy_import);
            di.work_loop(conn, ds);
        });
        Datastore { requester, reader }
    }

    /// Send a command to the worker thread and wait for its response.
    fn request(&self, cmd: Command) -> Result<Response, DatastoreError> {
        let receiver = self.requester.request(cmd).map_err(|e| {
            DatastoreError::InternalError(format!(
                "Failed to send request, datastore worker is gone: {e:?}"
            ))
        })?;
        receiver.collect().map_err(|e| {
            DatastoreError::InternalError(format!(
                "Failed to receive response, datastore worker died while handling request: {e:?}"
            ))
        })?
    }

    pub fn create_bucket(&self, bucket: &Bucket) -> Result<(), DatastoreError> {
        let cmd = Command::CreateBucket(bucket.clone());
        let result = _unwrap_empty_response(self.request(cmd)?);
        // A recreated bucket gets a fresh row id; drop any stale reader mapping.
        if result.is_ok() {
            if let Some(reader) = &self.reader {
                reader.invalidate_bid(&bucket.id);
            }
        }
        result
    }

    pub fn delete_bucket(&self, bucket_id: &str) -> Result<(), DatastoreError> {
        let cmd = Command::DeleteBucket(bucket_id.to_string());
        let result = _unwrap_empty_response(self.request(cmd)?);
        if result.is_ok() {
            if let Some(reader) = &self.reader {
                reader.invalidate_bid(bucket_id);
            }
        }
        result
    }

    pub fn get_bucket(&self, bucket_id: &str) -> Result<Bucket, DatastoreError> {
        let cmd = Command::GetBucket(bucket_id.to_string());
        match self.request(cmd)? {
            Response::Bucket(b) => Ok(b),
            _ => panic!("Invalid response"),
        }
    }

    pub fn get_buckets(&self) -> Result<HashMap<String, Bucket>, DatastoreError> {
        let cmd = Command::GetBuckets();
        match self.request(cmd)? {
            Response::BucketMap(bm) => Ok(bm),
            e => Err(DatastoreError::InternalError(format!(
                "Invalid response: {e:?}"
            ))),
        }
    }

    pub fn insert_events(
        &self,
        bucket_id: &str,
        events: &[Event],
    ) -> Result<Vec<Event>, DatastoreError> {
        let cmd = Command::InsertEvents(bucket_id.to_string(), events.to_vec());
        match self.request(cmd)? {
            Response::EventList(events) => Ok(events),
            _ => panic!("Invalid response"),
        }
    }

    pub fn heartbeat(
        &self,
        bucket_id: &str,
        heartbeat: Event,
        pulsetime: f64,
    ) -> Result<Event, DatastoreError> {
        let cmd = Command::Heartbeat(bucket_id.to_string(), heartbeat, pulsetime);
        match self.request(cmd)? {
            Response::Event(e) => Ok(e),
            _ => panic!("Invalid response"),
        }
    }

    pub fn get_event(&self, bucket_id: &str, event_id: i64) -> Result<Event, DatastoreError> {
        if let Some(reader) = &self.reader {
            return reader.get_event(bucket_id, event_id);
        }
        let cmd = Command::GetEvent(bucket_id.to_string(), event_id);
        match self.request(cmd)? {
            Response::Event(el) => Ok(el),
            _ => panic!("Invalid response"),
        }
    }

    pub fn get_events(
        &self,
        bucket_id: &str,
        starttime_opt: Option<DateTime<Utc>>,
        endtime_opt: Option<DateTime<Utc>>,
        limit_opt: Option<u64>,
    ) -> Result<Vec<Event>, DatastoreError> {
        if let Some(reader) = &self.reader {
            return reader.get_events(bucket_id, starttime_opt, endtime_opt, limit_opt, true);
        }
        let cmd = Command::GetEvents(
            bucket_id.to_string(),
            starttime_opt,
            endtime_opt,
            limit_opt,
            false,
        );
        match self.request(cmd)? {
            Response::EventList(el) => Ok(el),
            _ => panic!("Invalid response"),
        }
    }

    pub fn get_events_filtered(
        &self,
        bucket_id: &str,
        starttime_opt: Option<DateTime<Utc>>,
        endtime_opt: Option<DateTime<Utc>>,
        limit_opt: Option<u64>,
        filters: &[crate::EventFilter],
    ) -> Result<Vec<Event>, DatastoreError> {
        if let Some(reader) = &self.reader {
            return reader.get_events_filtered(
                bucket_id,
                starttime_opt,
                endtime_opt,
                limit_opt,
                filters,
            );
        }
        // Fallback to fetching all events and filtering in memory if we don't have a reader
        // (e.g. in-memory DB, where reads go through the worker).
        let mut events = self.get_events(bucket_id, starttime_opt, endtime_opt, limit_opt)?;
        for filter in filters {
            events = aw_transform::filter_keyvals(events, &filter.key, &filter.vals);
        }
        Ok(events)
    }

    pub fn get_events_grouped(
        &self,
        bucket_id: &str,
        starttime_opt: Option<DateTime<Utc>>,
        endtime_opt: Option<DateTime<Utc>>,
        group_by_key: &str,
    ) -> Result<Vec<Event>, DatastoreError> {
        if let Some(reader) = &self.reader {
            return reader.get_events_grouped(bucket_id, starttime_opt, endtime_opt, group_by_key);
        }

        let events = self.get_events(bucket_id, starttime_opt, endtime_opt, None)?;
        // Fallback: group and sum in memory
        let mut grouped: std::collections::HashMap<String, Event> =
            std::collections::HashMap::new();
        for event in events {
            if let Some(val) = event.data.get(group_by_key) {
                let key_str = val.to_string();
                if let Some(existing) = grouped.get_mut(&key_str) {
                    existing.duration += event.duration;
                } else {
                    let mut data_map = serde_json::Map::new();
                    data_map.insert(group_by_key.to_string(), val.clone());
                    grouped.insert(
                        key_str,
                        Event {
                            id: None,
                            timestamp: event.timestamp,
                            duration: event.duration,
                            data: data_map,
                        },
                    );
                }
            }
        }
        let mut grouped_vec: Vec<Event> = grouped.into_values().collect();
        grouped_vec.sort_by(|a, b| b.duration.cmp(&a.duration));
        Ok(grouped_vec)
    }

    pub fn get_events_intersected(
        &self,
        target_bucket_id: &str,
        filter_bucket_id: &str,
        starttime_opt: Option<DateTime<Utc>>,
        endtime_opt: Option<DateTime<Utc>>,
        filter_key: &str,
        filter_val: &serde_json::Value,
    ) -> Result<Vec<Event>, DatastoreError> {
        if let Some(reader) = &self.reader {
            return reader.get_events_intersected(
                target_bucket_id,
                filter_bucket_id,
                starttime_opt,
                endtime_opt,
                filter_key,
                filter_val,
            );
        }

        // Fallback: fetch both and intersect in memory
        let target_events = self.get_events(target_bucket_id, starttime_opt, endtime_opt, None)?;
        let filter_events = self.get_events(filter_bucket_id, starttime_opt, endtime_opt, None)?;
        let filter_events =
            aw_transform::filter_keyvals(filter_events, filter_key, &[filter_val.clone()]);

        Ok(aw_transform::filter_period_intersect(
            target_events,
            filter_events,
        ))
    }

    pub fn get_events_unclipped(
        &self,
        bucket_id: &str,
        starttime_opt: Option<DateTime<Utc>>,
        endtime_opt: Option<DateTime<Utc>>,
        limit_opt: Option<u64>,
    ) -> Result<Vec<Event>, DatastoreError> {
        if let Some(reader) = &self.reader {
            return reader.get_events(bucket_id, starttime_opt, endtime_opt, limit_opt, false);
        }
        let cmd = Command::GetEvents(
            bucket_id.to_string(),
            starttime_opt,
            endtime_opt,
            limit_opt,
            true,
        );
        match self.request(cmd)? {
            Response::EventList(el) => Ok(el),
            _ => panic!("Invalid response"),
        }
    }

    pub fn get_event_count(
        &self,
        bucket_id: &str,
        starttime_opt: Option<DateTime<Utc>>,
        endtime_opt: Option<DateTime<Utc>>,
    ) -> Result<i64, DatastoreError> {
        if let Some(reader) = &self.reader {
            return reader.get_event_count(bucket_id, starttime_opt, endtime_opt);
        }
        let cmd = Command::GetEventCount(bucket_id.to_string(), starttime_opt, endtime_opt);
        match self.request(cmd)? {
            Response::Count(n) => Ok(n),
            _ => panic!("Invalid response"),
        }
    }

    pub fn delete_events_by_id(
        &self,
        bucket_id: &str,
        event_ids: Vec<i64>,
    ) -> Result<(), DatastoreError> {
        let cmd = Command::DeleteEventsById(bucket_id.to_string(), event_ids);
        _unwrap_empty_response(self.request(cmd)?)
    }

    pub fn force_commit(&self) -> Result<(), DatastoreError> {
        let cmd = Command::ForceCommit();
        _unwrap_empty_response(self.request(cmd)?)
    }

    pub fn get_key_values(&self, pattern: &str) -> Result<HashMap<String, String>, DatastoreError> {
        let cmd = Command::GetKeyValues(pattern.to_string());
        match self.request(cmd)? {
            Response::KeyValues(value) => Ok(value),
            _ => panic!("Invalid response"),
        }
    }

    pub fn get_key_value(&self, key: &str) -> Result<String, DatastoreError> {
        let cmd = Command::GetKeyValue(key.to_string());
        match self.request(cmd)? {
            Response::KeyValue(kv) => Ok(kv),
            _ => panic!("Invalid response"),
        }
    }

    pub fn set_key_value(&self, key: &str, data: &str) -> Result<(), DatastoreError> {
        let cmd = Command::SetKeyValue(key.to_string(), data.to_string());
        _unwrap_empty_response(self.request(cmd)?)
    }

    pub fn delete_key_value(&self, key: &str) -> Result<(), DatastoreError> {
        let cmd = Command::DeleteKeyValue(key.to_string());
        _unwrap_empty_response(self.request(cmd)?)
    }

    pub fn refresh_privacy_filter(&self) -> Result<(), DatastoreError> {
        _unwrap_empty_response(self.request(Command::RefreshPrivacyFilter())?)
    }

    // Should block until worker has stopped
    pub fn close(&self) {
        info!("Sending close request to database");
        match self.request(Command::Close()) {
            Ok(Response::Empty()) => (),
            Ok(_) => panic!("Invalid response"),
            // Worker already gone means there is nothing left to close
            Err(e) => warn!("Error closing database: {e:?}"),
        }
    }
}
