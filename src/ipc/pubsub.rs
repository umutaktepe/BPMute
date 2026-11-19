use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::Notify;

use super::protocol::{PROTOCOL_VERSION, PushEventEnvelope};

/// Default per-subscriber bounded queue capacity for high-frequency `HrSample` events.
pub const DEFAULT_STREAM_QUEUE_CAPACITY: usize = 64;

/// Per-subscriber event queue with reliable control delivery and drop-oldest stream backpressure.
#[derive(Debug)]
pub struct SubscriberQueue {
    stream_capacity: usize,
    inner: Mutex<Queues>,
    notify: Notify,
}

#[derive(Debug, Default)]
struct Queues {
    control: VecDeque<PushEventEnvelope>,
    stream: VecDeque<PushEventEnvelope>,
    dropped_stream_events: u64,
}

impl SubscriberQueue {
    /// Creates a new bounded subscriber queue.
    pub fn new(stream_capacity: usize) -> Self {
        Self {
            stream_capacity: stream_capacity.max(1),
            inner: Mutex::new(Queues::default()),
            notify: Notify::new(),
        }
    }

    /// Enqueues a reliable control event (`Snapshot`, `ConfigUpdated`, `StateChanged`, `AlertChanged`).
    pub fn push_control(&self, event: PushEventEnvelope) {
        let mut guard = self.inner.lock().expect("subscriber queue mutex poisoned");
        guard.control.push_back(event);
        drop(guard);
        self.notify.notify_one();
    }

    /// Enqueues a high-frequency stream event (`HrSample`), dropping the oldest queued stream
    /// event if the queue is at capacity.
    pub fn push_stream(&self, event: PushEventEnvelope) {
        let mut guard = self.inner.lock().expect("subscriber queue mutex poisoned");
        if guard.stream.len() >= self.stream_capacity {
            guard.stream.pop_front();
            guard.dropped_stream_events += 1;
        }
        guard.stream.push_back(event);
        drop(guard);
        self.notify.notify_one();
    }

    /// Pops the next deliverable event, prioritizing control events ahead of stream ticks.
    pub fn pop_next(&self) -> Option<PushEventEnvelope> {
        let mut guard = self.inner.lock().expect("subscriber queue mutex poisoned");
        guard
            .control
            .pop_front()
            .or_else(|| guard.stream.pop_front())
    }

    /// Returns how many stream events have been dropped due to slow consumption.
    pub fn dropped_stream_events(&self) -> u64 {
        self.inner
            .lock()
            .expect("subscriber queue mutex poisoned")
            .dropped_stream_events
    }

    /// Waits asynchronously until at least one event is available and pops it.
    pub async fn next_event(&self) -> PushEventEnvelope {
        loop {
            let notified = self.notify.notified();
            if let Some(ev) = self.pop_next() {
                return ev;
            }
            notified.await;
        }
    }
}

/// Thread-safe pub/sub broker fanning out daemon events to connected IPC subscribers.
#[derive(Debug)]
pub struct EventBroker {
    next_sub_id: AtomicU64,
    next_seq: AtomicU64,
    stream_capacity: usize,
    subscribers: Mutex<HashMap<u64, Arc<SubscriberQueue>>>,
}

impl Default for EventBroker {
    fn default() -> Self {
        Self::new(DEFAULT_STREAM_QUEUE_CAPACITY)
    }
}

impl EventBroker {
    /// Creates a new broker with the given per-subscriber stream queue capacity.
    pub fn new(stream_capacity: usize) -> Self {
        Self {
            next_sub_id: AtomicU64::new(1),
            next_seq: AtomicU64::new(1),
            stream_capacity: stream_capacity.max(1),
            subscribers: Mutex::new(HashMap::new()),
        }
    }

    /// Registers a new subscriber and returns its ID and queue handle.
    pub fn subscribe(&self) -> (u64, Arc<SubscriberQueue>) {
        let id = self.next_sub_id.fetch_add(1, Ordering::Relaxed);
        let queue = Arc::new(SubscriberQueue::new(self.stream_capacity));
        self.subscribers
            .lock()
            .expect("broker mutex poisoned")
            .insert(id, Arc::clone(&queue));
        (id, queue)
    }

    /// Removes a subscriber by ID. Returns `true` if it was active.
    pub fn unsubscribe(&self, id: u64) -> bool {
        self.subscribers
            .lock()
            .expect("broker mutex poisoned")
            .remove(&id)
            .is_some()
    }

    /// Builds an ordered push event envelope with the next monotonic sequence number.
    pub fn make_event(&self, event: impl Into<String>, data: Value) -> PushEventEnvelope {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        PushEventEnvelope {
            v: PROTOCOL_VERSION,
            event: event.into(),
            seq,
            data,
        }
    }

    /// Broadcasts a reliable control event to all active subscribers.
    pub fn publish_control(&self, event: &str, data: Value) {
        let targets: Vec<Arc<SubscriberQueue>> = self
            .subscribers
            .lock()
            .expect("broker mutex poisoned")
            .values()
            .cloned()
            .collect();
        if targets.is_empty() {
            return;
        }
        let envelope = self.make_event(event, data);
        for queue in targets {
            queue.push_control(envelope.clone());
        }
    }

    /// Broadcasts a droppable stream event (`HrSample`) to all active subscribers.
    pub fn publish_stream(&self, event: &str, data: Value) {
        let targets: Vec<Arc<SubscriberQueue>> = self
            .subscribers
            .lock()
            .expect("broker mutex poisoned")
            .values()
            .cloned()
            .collect();
        if targets.is_empty() {
            return;
        }
        let envelope = self.make_event(event, data);
        for queue in targets {
            queue.push_stream(envelope.clone());
        }
    }
}
