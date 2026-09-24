//! Offline request queue, the counterpart of `RequestQueue` in aw-client-python.
//!
//! Heartbeats sent through a [`RequestQueue`] are written to a file and delivered by a
//! background thread, so a watcher keeps recording while the server is down or
//! restarting, and nothing queued is lost if the watcher itself exits. Buckets
//! registered with [`RequestQueue::register_bucket`] are created every time the
//! queue (re)connects, before any queued heartbeat is sent.
//!
//! Delivery follows the Python client: connection failures and 500s are retried,
//! a 400 (a payload that will never be accepted) is dropped, and any other error
//! is logged and dropped.
//!
//! The queue file is append-only JSON lines, with a sibling `.offset` file that counts
//! the lines already delivered. Delivering a heartbeat costs one small write, and a
//! crash re-sends at most the one heartbeat that was in flight. The file is truncated
//! whenever the queue drains.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map};

use aw_models::Event;

/// Bumped whenever the queue file format changes.
const QUEUE_VERSION: u32 = 1;
/// How long to wait between reconnect attempts, as in Python.
const RECONNECT_INTERVAL: Duration = Duration::from_secs(10);
/// Pause after a failed delivery before retrying it.
const RETRY_DELAY: Duration = Duration::from_millis(500);
/// Upper bound for one request, so stopping the queue never waits on a hung server
/// for the client's full timeout.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct QueuedHeartbeat {
    bucket_id: String,
    pulsetime: f64,
    event: Event,
}

/// What the worker thread needs to talk to the server.
#[derive(Clone)]
pub(crate) struct Transport {
    pub(crate) client: reqwest::Client,
    pub(crate) baseurl: reqwest::Url,
    pub(crate) name: String,
    pub(crate) hostname: String,
}

impl Transport {
    fn url(&self, segments: &[&str]) -> reqwest::Url {
        let mut url = self.baseurl.clone();
        if let Ok(mut path) = url.path_segments_mut() {
            path.pop_if_empty().extend(["api", "0"]).extend(segments);
        }
        url
    }

    /// Check that the server answers, for a queue with no buckets to create.
    async fn ping(&self) -> Result<(), reqwest::Error> {
        self.client
            .get(self.url(&["info"]))
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    async fn create_bucket(&self, bucket_id: &str, event_type: &str) -> Result<(), reqwest::Error> {
        let body = json!({
            "id": bucket_id,
            "client": self.name,
            "type": event_type,
            "hostname": self.hostname,
            "data": Map::new(),
        });
        // An existing bucket answers 304, which is a success here.
        self.client
            .post(self.url(&["buckets", bucket_id]))
            .timeout(REQUEST_TIMEOUT)
            .json(&body)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    async fn heartbeat(&self, request: &QueuedHeartbeat) -> Result<(), reqwest::Error> {
        let mut url = self.url(&["buckets", &request.bucket_id, "heartbeat"]);
        url.query_pairs_mut()
            .append_pair("pulsetime", &request.pulsetime.to_string());
        self.client
            .post(url)
            .timeout(REQUEST_TIMEOUT)
            .json(&request.event)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

struct State {
    queue: VecDeque<QueuedHeartbeat>,
    log: QueueFile,
    /// (bucket_id, event_type) to create on every (re)connect.
    buckets: Vec<(String, String)>,
    /// Set when a bucket is registered, so it's created before the next delivery.
    buckets_pending: bool,
    connected: bool,
    stop: bool,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Sleep for `timeout` or until woken; returns whether the queue should stop.
    fn wait(&self, timeout: Duration) -> bool {
        let state = self.lock();
        if state.stop {
            return true;
        }
        let (state, _) = self
            .wake
            .wait_timeout(state, timeout)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.stop
    }
}

/// A background queue that delivers heartbeats to the server, see the [module docs](self).
///
/// Create one with [`AwClient::request_queue`](crate::AwClient::request_queue). Dropping
/// it (or calling [`stop`](Self::stop)) stops the worker; undelivered heartbeats stay in
/// the queue file and are sent by the next queue opened on the same file.
pub struct RequestQueue {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    path: PathBuf,
}

impl std::fmt::Debug for RequestQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "RequestQueue(path={:?})", self.path)
    }
}

/// Default queue file for a client, under the aw-client data directory (the one
/// aw-core's `get_data_dir("aw-client")` returns): `queued/<name>[-testing].v1.jsonl`.
pub fn default_queue_path(client_name: &str, testing: bool) -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    let root = dirs::data_local_dir()?
        .join("activitywatch")
        .join("activitywatch");
    #[cfg(not(target_os = "windows"))]
    let root = dirs::data_dir()?.join("activitywatch");
    let file = format!(
        "{client_name}{}.v{QUEUE_VERSION}.jsonl",
        if testing { "-testing" } else { "" }
    );
    Some(root.join("aw-client").join("queued").join(file))
}

impl RequestQueue {
    pub(crate) fn start(transport: Transport, path: PathBuf) -> io::Result<RequestQueue> {
        let (log, queue) = QueueFile::open(&path)?;
        if !queue.is_empty() {
            log::info!(
                "Loaded {} queued heartbeats from {}",
                queue.len(),
                path.display()
            );
        }
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                queue,
                log,
                buckets: Vec::new(),
                buckets_pending: false,
                connected: false,
                stop: false,
            }),
            wake: Condvar::new(),
        });
        let worker_shared = Arc::clone(&shared);
        let worker = std::thread::Builder::new()
            .name(format!("aw-client-queue-{}", transport.name))
            .spawn(move || run_worker(worker_shared, transport))?;
        Ok(RequestQueue {
            shared,
            worker: Some(worker),
            path,
        })
    }

    /// Create `bucket_id` whenever the queue connects, before sending queued heartbeats.
    pub fn register_bucket(&self, bucket_id: &str, event_type: &str) {
        let mut state = self.shared.lock();
        let entry = (bucket_id.to_string(), event_type.to_string());
        if !state.buckets.contains(&entry) {
            state.buckets.push(entry);
        }
        state.buckets_pending = true;
        drop(state);
        self.shared.wake.notify_all();
    }

    /// Queue a heartbeat. It's written to the queue file before this returns and
    /// delivered in order by the background thread.
    pub fn heartbeat(&self, bucket_id: &str, event: &Event, pulsetime: f64) -> io::Result<()> {
        let request = QueuedHeartbeat {
            bucket_id: bucket_id.to_string(),
            pulsetime,
            event: event.clone(),
        };
        let mut state = self.shared.lock();
        state.log.append(&request)?;
        state.queue.push_back(request);
        drop(state);
        self.shared.wake.notify_all();
        Ok(())
    }

    /// Number of heartbeats not yet delivered.
    pub fn len(&self) -> usize {
        self.shared.lock().queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether the last attempt to reach the server succeeded.
    pub fn is_connected(&self) -> bool {
        self.shared.lock().connected
    }

    /// Path of the queue file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Stop the worker and wait for it to exit. Undelivered heartbeats stay queued.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.shared.lock().stop = true;
        self.shared.wake.notify_all();
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                log::error!("aw-client request queue worker panicked");
            }
        }
    }
}

impl Drop for RequestQueue {
    fn drop(&mut self) {
        self.shutdown();
    }
}

enum Delivery {
    Sent,
    /// Server unreachable: mark disconnected and reconnect first.
    Disconnected,
    /// Transient server error: try the same heartbeat again shortly.
    Retry,
    /// Will never succeed: drop it.
    Drop,
}

fn classify(err: &reqwest::Error) -> Delivery {
    if err.is_connect() || err.is_timeout() || err.is_request() {
        return Delivery::Disconnected;
    }
    match err.status() {
        Some(reqwest::StatusCode::BAD_REQUEST) => Delivery::Drop,
        Some(reqwest::StatusCode::INTERNAL_SERVER_ERROR) => Delivery::Retry,
        _ => Delivery::Drop,
    }
}

fn run_worker(shared: Arc<Shared>, transport: Transport) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            log::error!("aw-client request queue could not start a runtime: {err}");
            return;
        }
    };

    loop {
        let (connected, buckets_pending, buckets) = {
            let state = shared.lock();
            if state.stop {
                return;
            }
            (
                state.connected,
                state.buckets_pending,
                state.buckets.clone(),
            )
        };

        if !connected || buckets_pending {
            let created = runtime.block_on(async {
                if buckets.is_empty() {
                    return transport.ping().await;
                }
                for (bucket_id, event_type) in &buckets {
                    transport.create_bucket(bucket_id, event_type).await?;
                }
                Ok(())
            });
            match created {
                Ok(()) => {
                    let mut state = shared.lock();
                    if !state.connected {
                        log::info!("Connection to aw-server established by {}", transport.name);
                    }
                    state.connected = true;
                    // A bucket registered while we were creating the others is still pending.
                    state.buckets_pending = state.buckets.len() != buckets.len();
                }
                Err(err) => {
                    let queued = {
                        let mut state = shared.lock();
                        state.connected = false;
                        state.queue.len()
                    };
                    log::warn!("Not connected to server ({err}), {queued} requests in queue");
                    if shared.wait(RECONNECT_INTERVAL) {
                        return;
                    }
                }
            }
            continue;
        }

        let Some(request) = shared.lock().queue.front().cloned() else {
            // Idle until a heartbeat is queued, a bucket is registered, or we're stopped.
            if shared.wait(Duration::from_secs(1)) {
                return;
            }
            continue;
        };

        let delivery = match runtime.block_on(transport.heartbeat(&request)) {
            Ok(()) => Delivery::Sent,
            Err(err) => {
                let delivery = classify(&err);
                match delivery {
                    Delivery::Disconnected => log::warn!(
                        "Connection refused or timeout, will queue requests until connection is available: {err}"
                    ),
                    Delivery::Retry => {
                        log::error!("Server error, retrying heartbeat to {}: {err}", request.bucket_id)
                    }
                    Delivery::Drop => log::error!(
                        "Heartbeat to {} failed, not retrying: {err}; event: {:?}",
                        request.bucket_id,
                        request.event
                    ),
                    Delivery::Sent => {}
                }
                delivery
            }
        };

        match delivery {
            Delivery::Sent | Delivery::Drop => {
                let mut state = shared.lock();
                state.queue.pop_front();
                let remaining = state.queue.len();
                if let Err(err) = state.log.mark_delivered(remaining) {
                    log::error!("Failed to update queue file: {err}");
                }
            }
            Delivery::Disconnected => {
                // The reconnect step at the top of the loop waits between attempts.
                shared.lock().connected = false;
            }
            Delivery::Retry => {
                if shared.wait(RETRY_DELAY) {
                    return;
                }
            }
        }
    }
}

/// Append-only JSON-lines queue file plus a `.offset` file counting delivered lines.
struct QueueFile {
    path: PathBuf,
    offset_path: PathBuf,
    file: File,
    delivered: u64,
}

impl QueueFile {
    fn open(path: &Path) -> io::Result<(QueueFile, VecDeque<QueuedHeartbeat>)> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut offset_name = path.file_name().unwrap_or_default().to_os_string();
        offset_name.push(".offset");
        let offset_path = path.with_file_name(offset_name);

        let delivered: u64 = match fs::read_to_string(&offset_path) {
            Ok(raw) => raw.trim().parse().unwrap_or(0),
            Err(err) if err.kind() == io::ErrorKind::NotFound => 0,
            Err(err) => return Err(err),
        };

        let mut queue = VecDeque::new();
        match File::open(path) {
            Ok(file) => {
                for (index, line) in BufReader::new(file).lines().enumerate() {
                    let line = line?;
                    if (index as u64) < delivered || line.trim().is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<QueuedHeartbeat>(&line) {
                        Ok(request) => queue.push_back(request),
                        // A line cut short by a crash mid-write; skip it.
                        Err(err) => log::warn!(
                            "Skipping unreadable line {} in {}: {err}",
                            index + 1,
                            path.display()
                        ),
                    }
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }

        // Rewrite the file with just the pending heartbeats: this drops delivered and
        // unreadable lines (a crash mid-write leaves a partial last line, which the
        // next append would otherwise run into) and resets the offset.
        let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
        tmp_name.push(".tmp");
        let tmp_path = path.with_file_name(tmp_name);
        {
            let mut tmp = File::create(&tmp_path)?;
            for request in &queue {
                let mut line = serde_json::to_vec(request)?;
                line.push(b'\n');
                tmp.write_all(&line)?;
            }
            tmp.sync_all()?;
        }
        fs::rename(&tmp_path, path)?;
        match fs::remove_file(&offset_path) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err),
            _ => {}
        }

        let file = OpenOptions::new().append(true).open(path)?;
        Ok((
            QueueFile {
                path: path.to_path_buf(),
                offset_path,
                file,
                delivered: 0,
            },
            queue,
        ))
    }

    fn append(&mut self, request: &QueuedHeartbeat) -> io::Result<()> {
        let mut line = serde_json::to_vec(request)?;
        line.push(b'\n');
        self.file.write_all(&line)?;
        self.file.flush()
    }

    /// Record one more delivered line; once nothing is left, truncate both files.
    fn mark_delivered(&mut self, remaining: usize) -> io::Result<()> {
        if remaining == 0 {
            self.file.set_len(0)?;
            self.delivered = 0;
            return match fs::remove_file(&self.offset_path) {
                Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
                _ => Ok(()),
            };
        }
        self.delivered += 1;
        fs::write(&self.offset_path, self.delivered.to_string())
    }
}

impl std::fmt::Debug for QueueFile {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "QueueFile({:?})", self.path)
    }
}
