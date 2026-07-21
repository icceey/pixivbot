use super::{CoordinatorStop, MultipartCoordinator, MultipartOutcome};
use crate::archive_download::part::{aggregate_downloaded_bytes, PartFailureKind};
use crate::archive_download::sequential::made_progress;
use crate::error::{Error, Result};

impl MultipartCoordinator {
    pub(super) async fn finish_stop(&mut self, stop: CoordinatorStop) -> Result<MultipartOutcome> {
        self.shutdown_tasks().await;
        match stop {
            CoordinatorStop::Error(error) => Err(error),
            CoordinatorStop::Part(failure)
                if failure.kind == PartFailureKind::RestartSequential =>
            {
                Ok(MultipartOutcome::RestartSequential)
            }
            CoordinatorStop::Part(failure) => {
                let final_downloaded =
                    aggregate_downloaded_bytes(&self.artifacts, &self.manifest).await?;
                let bytes_delta = final_downloaded.saturating_sub(self.initial_downloaded);
                let elapsed = self.started_at.elapsed();
                if made_progress(bytes_delta, elapsed.as_secs_f64()) {
                    Err(Error::DownloadInProgress {
                        inner: Box::new(failure.error),
                        attempts: failure.attempts,
                        bytes_delta,
                        elapsed,
                    })
                } else {
                    Err(failure.error)
                }
            }
        }
    }

    pub(super) async fn shutdown_tasks(&mut self) {
        for sender in self.pause_senders.values() {
            let _ = sender.send(true);
        }
        while !self.joins.is_empty() {
            tokio::select! {
                biased;
                event = self.samples_rx.recv() => {
                    if let Some(event) = event {
                        let _ = event.applied.send(());
                    }
                }
                joined = self.joins.join_next() => {
                    if let Some(Ok((part_id, _))) = joined {
                        self.pause_senders.remove(&part_id);
                        if let Some(runtime) = self.runtime_parts.get_mut(&part_id) {
                            runtime.active = false;
                        }
                    }
                }
            }
        }
        while let Ok(event) = self.samples_rx.try_recv() {
            let _ = event.applied.send(());
        }
        self.pause_senders.clear();
        for runtime in self.runtime_parts.values_mut() {
            runtime.active = false;
        }
    }
}
