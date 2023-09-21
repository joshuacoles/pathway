use log::error;
use std::sync::{Arc, Mutex};

use crate::persistence::tracker::SingleWorkerPersistentStorage;

#[derive(Default)]
pub struct WorkersPersistenceCoordinator {
    worker_persistence_managers: Vec<Arc<Mutex<SingleWorkerPersistentStorage>>>,
    last_timestamp_flushed: Option<u64>,
}

impl WorkersPersistenceCoordinator {
    pub fn new() -> Self {
        Self {
            worker_persistence_managers: Vec::new(),
            last_timestamp_flushed: Some(0),
        }
    }

    /// Record shared pointer to the particular worker's persistent storage in the storage.
    /// Maintain these pointers in the sorted order, so that the pointer to the storage of the
    /// worker K occupies the K-th position in the array.
    pub fn register_worker(
        &mut self,
        persistence_manager: Arc<Mutex<SingleWorkerPersistentStorage>>,
    ) {
        self.worker_persistence_managers.push(persistence_manager);
        let mut sorted_position = self.worker_persistence_managers.len() - 1;
        while sorted_position > 0 {
            let current_worker_id = self.worker_persistence_managers[sorted_position]
                .lock()
                .unwrap()
                .worker_id();
            let prev_worker_id = self.worker_persistence_managers[sorted_position - 1]
                .lock()
                .unwrap()
                .worker_id();
            if current_worker_id < prev_worker_id {
                self.worker_persistence_managers
                    .swap(sorted_position, sorted_position - 1);
                sorted_position -= 1;
            } else {
                break;
            }
        }
    }

    /// Handles the event of the timestamp update within a particular sink in a particular worker.
    /// In case the global advanced timestamp advances, the snapshot writers flush the data and
    /// the new frontiers are committed.
    ///
    /// The new frontiers for any particular time T are committed only when all workers finish the
    /// output for this time T. Synchronization is needed, because there is no guarantee that the
    /// worker which reads the entry will output it in case of multithreaded execution.
    pub fn accept_finalized_timestamp(
        &mut self,
        worker_id: usize,
        sink_id: usize,
        reported_timestamp: Option<u64>,
    ) {
        let worker_storage = &self.worker_persistence_managers[worker_id];
        worker_storage
            .lock()
            .unwrap()
            .update_sink_finalized_time(sink_id, reported_timestamp);

        let global_finalized_timestamp = self.global_closed_timestamp();

        if global_finalized_timestamp != self.last_timestamp_flushed {
            self.last_timestamp_flushed = global_finalized_timestamp;
            let mut worker_futures = Vec::new();

            for persistence_manager in &self.worker_persistence_managers {
                let commit_data = persistence_manager
                    .lock()
                    .unwrap()
                    .accept_globally_finalized_timestamp(global_finalized_timestamp);
                worker_futures.push(commit_data);
            }

            for (tracker, commit_data) in self
                .worker_persistence_managers
                .iter()
                .zip(worker_futures.iter_mut())
            {
                let is_prepared = commit_data.prepare();
                let mut tracker = tracker.lock().unwrap();
                if !is_prepared {
                    error!(
                        "Failed to prepare frontier commit for worker {}",
                        tracker.worker_id()
                    );
                    continue;
                }
                tracker.commit_globally_finalized_timestamp(commit_data);
            }
        }
    }

    pub fn global_closed_timestamp(&mut self) -> Option<u64> {
        let mut min_closed_timestamp = None;
        for worker_pm in &self.worker_persistence_managers {
            let worker_closed_timestamp = worker_pm.lock().unwrap().finalized_time_within_worker();
            if let Some(worker_closed_timestamp) = worker_closed_timestamp {
                match min_closed_timestamp {
                    None => min_closed_timestamp = Some(worker_closed_timestamp),
                    Some(current_min) => {
                        if current_min > worker_closed_timestamp {
                            min_closed_timestamp = Some(worker_closed_timestamp);
                        }
                    }
                }
            }
        }
        min_closed_timestamp
    }
}

pub type SharedWorkersPersistenceCoordinator = Arc<Mutex<WorkersPersistenceCoordinator>>;