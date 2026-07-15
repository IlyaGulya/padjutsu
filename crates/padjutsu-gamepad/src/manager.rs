use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use ahash::AHashMap;
use crossbeam_channel::{bounded, unbounded, Sender};

use crate::command::Command;
use crate::Result;
use crate::events::{ControllerEvent, EventReceiver};
use crate::handle::ControllerHandle;
use crate::runtime::start_runtime_thread;
use crate::types::{AxisSnapshot, ControllerId, ControllerInfo};

/// Shared state used by the manager, the runtime loop and controller handles.
pub(crate) struct Inner {
    pub subscribers: Mutex<Vec<Sender<ControllerEvent>>>,
    pub controllers_info: RwLock<AHashMap<ControllerId, ControllerInfo>>,
    pub controller_axes: RwLock<AHashMap<ControllerId, AxisSnapshot>>,
    pub cmd_tx: Sender<Command>,
}

/// Manager responsible for discovering controllers and emitting events.
pub struct ControllerManager {
    pub(crate) inner: Arc<Inner>,
}

impl ControllerManager {
    /// Creates a new manager and starts the background runtime thread.
    /// Blocks briefly until the initial device enumeration completes (up to 1s).
    pub fn new() -> Result<Self> {
        let (cmd_tx, cmd_rx) = unbounded::<Command>();
        let inner = Arc::new(Inner {
            subscribers: Mutex::new(Vec::new()),
            controllers_info: RwLock::new(AHashMap::new()),
            controller_axes: RwLock::new(AHashMap::new()),
            cmd_tx,
        });

        let inner_clone = inner.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        start_runtime_thread(inner_clone, cmd_rx, Some(ready_tx));

        // Best-effort wait for the initial enumeration. Time out if backend fails.
        let _ = ready_rx.recv_timeout(Duration::from_secs(1));

        Ok(Self { inner })
    }

    /// Subscribes to controller events. Dropped subscribers are cleaned automatically.
    ///
    /// Channel is bounded to 4096 events. If the consumer falls behind, the
    /// broadcaster will drop new events rather than buffer unbounded memory.
    /// This is intentional: for real-time input, dropping the latest is
    /// preferable to growing the queue indefinitely.
    pub fn subscribe(&self) -> EventReceiver {
        let (tx, rx) = bounded(4096);
        if let Ok(mut subs) = self.inner.subscribers.lock() {
            subs.push(tx);
        }
        rx
    }

    /// Returns a snapshot of currently known controllers.
    pub fn controllers(&self) -> Vec<ControllerInfo> {
        if let Ok(map) = self.inner.controllers_info.read() {
            return map.values().cloned().collect();
        }
        Vec::new()
    }

    /// Returns the latest axis state observed directly by the SDL runtime.
    /// This state is independent of the event subscriber queue, so consumers
    /// can skip historical motion and recover from a dropped neutral event.
    pub fn axis_snapshots(&self) -> Vec<(ControllerId, AxisSnapshot)> {
        if let Ok(map) = self.inner.controller_axes.read() {
            return map.iter().map(|(id, axes)| (*id, *axes)).collect();
        }
        Vec::new()
    }

    /// Visits the latest axis snapshots without allocating a per-tick buffer.
    pub fn for_each_axis_snapshot(
        &self,
        mut visitor: impl FnMut(ControllerId, AxisSnapshot),
    ) {
        if let Ok(map) = self.inner.controller_axes.read() {
            for (id, axes) in map.iter() {
                visitor(*id, *axes);
            }
        }
    }

    /// Returns a handle to a controller by id if it is currently known.
    pub fn controller(&self, id: ControllerId) -> Option<ControllerHandle> {
        if let Ok(map) = self.inner.controllers_info.read() {
            if map.contains_key(&id) {
                return Some(ControllerHandle {
                    id,
                    inner: self.inner.clone(),
                });
            }
        }
        None
    }
}
