use super::artifacts::ArchiveArtifacts;
use super::manifest::ArchiveManifest;
use super::part::{run_part, PartExit, PartFailure, PartSampleEvent, SeedResponse, Validator};
use crate::error::{Error, Result};
use crate::models::EhCookies;
use std::collections::{HashMap, VecDeque};
use std::time::Instant;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;

mod runtime;
mod setup;
mod shutdown;
mod split;

use runtime::{apply_part_sample, RuntimePart, SampleDisposition};

pub(super) enum MultipartOutcome {
    Complete(ArchiveManifest),
    RestartSequential,
}

pub(super) struct MultipartCoordinator {
    http: reqwest::Client,
    cookies: EhCookies,
    url: String,
    artifacts: ArchiveArtifacts,
    manifest: ArchiveManifest,
    validator: Validator,
    runtime_parts: HashMap<u64, RuntimePart>,
    pending_recovered: VecDeque<u64>,
    pause_senders: HashMap<u64, watch::Sender<bool>>,
    joins: JoinSet<(u64, PartExit)>,
    samples_tx: mpsc::UnboundedSender<PartSampleEvent>,
    samples_rx: mpsc::UnboundedReceiver<PartSampleEvent>,
    max_concurrency: usize,
    initial_downloaded: u64,
    started_at: Instant,
}

enum CoordinatorStop {
    Error(Error),
    Part(PartFailure),
}

enum JoinDisposition {
    Complete(u64),
    Paused(u64),
}

impl MultipartCoordinator {
    fn launch_part(&mut self, part_id: u64, seed: Option<SeedResponse>) -> Result<()> {
        let runtime = self
            .runtime_parts
            .get_mut(&part_id)
            .ok_or_else(|| Error::Other("archive runtime part is missing".into()))?;
        if runtime.active || runtime.downloaded >= runtime.part.len() {
            return Err(Error::Other(
                "archive runtime part cannot be launched".into(),
            ));
        }
        runtime.active = true;
        let part = runtime.part.clone();
        let attempts_used = runtime.attempts_used;
        let generation = runtime.generation;
        let part_path = ArchiveManifest::part_path(&self.artifacts, part_id);
        let (pause_tx, pause_rx) = watch::channel(false);
        self.pause_senders.insert(part_id, pause_tx);
        self.joins.spawn(run_part(
            self.http.clone(),
            self.cookies.clone(),
            self.url.clone(),
            self.manifest.total_len,
            self.validator.clone(),
            part,
            part_path,
            attempts_used,
            generation,
            seed,
            pause_rx,
            self.samples_tx.clone(),
        ));
        Ok(())
    }

    pub(super) async fn run(mut self) -> Result<MultipartOutcome> {
        loop {
            if self.joins.is_empty() {
                if let Err(error) = self.fill_pending_recovered() {
                    return self.finish_stop(CoordinatorStop::Error(error)).await;
                }
                if self.joins.is_empty() {
                    if self
                        .runtime_parts
                        .values()
                        .all(|runtime| runtime.downloaded == runtime.part.len())
                    {
                        return Ok(MultipartOutcome::Complete(self.manifest));
                    }
                    return self
                        .finish_stop(CoordinatorStop::Error(Error::Other(
                            "archive multipart coordinator has unfinished inactive parts".into(),
                        )))
                        .await;
                }
            }

            enum Next {
                Sample(PartSampleEvent),
                Join(std::result::Result<(u64, PartExit), tokio::task::JoinError>),
            }
            let next = tokio::select! {
                biased;
                event = self.samples_rx.recv() => {
                    Next::Sample(event.expect("coordinator owns the sample sender"))
                }
                joined = self.joins.join_next() => {
                    Next::Join(joined.expect("join set checked non-empty"))
                }
            };
            match next {
                Next::Sample(event) => match self.reconcile_sample(event) {
                    Ok(SampleDisposition::Stable) => {
                        if let Err(stop) = self.rebalance_once().await {
                            return self.finish_stop(stop).await;
                        }
                    }
                    Ok(SampleDisposition::Ignored | SampleDisposition::Reconciled) => {}
                    Err(error) => {
                        return self.finish_stop(CoordinatorStop::Error(error)).await;
                    }
                },
                Next::Join(joined) => match self.reconcile_join(joined).await {
                    Ok(JoinDisposition::Complete(_)) => {
                        if let Err(stop) = self.rebalance_once().await {
                            return self.finish_stop(stop).await;
                        }
                    }
                    Ok(JoinDisposition::Paused(_)) => {
                        return self
                            .finish_stop(CoordinatorStop::Error(Error::Other(
                                "archive part paused without a split request".into(),
                            )))
                            .await;
                    }
                    Err(stop) => return self.finish_stop(stop).await,
                },
            }
        }
    }

    fn reconcile_sample(&mut self, event: PartSampleEvent) -> Result<SampleDisposition> {
        let result = match self.runtime_parts.get_mut(&event.sample.part_id) {
            Some(runtime) => apply_part_sample(runtime, event.sample),
            None => Ok(SampleDisposition::Ignored),
        };
        let _ = event.applied.send(());
        result
    }

    async fn reconcile_join(
        &mut self,
        joined: std::result::Result<(u64, PartExit), tokio::task::JoinError>,
    ) -> std::result::Result<JoinDisposition, CoordinatorStop> {
        let (part_id, exit) = joined.map_err(|error| {
            CoordinatorStop::Error(Error::Other(format!(
                "archive part worker task failed: {error}"
            )))
        })?;
        self.pause_senders.remove(&part_id);
        let part_path = ArchiveManifest::part_path(&self.artifacts, part_id);
        let runtime = self.runtime_parts.get_mut(&part_id).ok_or_else(|| {
            CoordinatorStop::Error(Error::Other(
                "archive part worker returned an unknown id".into(),
            ))
        })?;
        runtime.active = false;
        match exit {
            PartExit::Complete { attempts_used } | PartExit::Paused { attempts_used } => {
                let metadata = tokio::fs::metadata(part_path)
                    .await
                    .map_err(|error| CoordinatorStop::Error(error.into()))?;
                if !metadata.is_file() || metadata.len() > runtime.part.len() {
                    return Err(CoordinatorStop::Error(Error::Other(
                        "archive part worker produced an invalid durable file".into(),
                    )));
                }
                runtime.downloaded = metadata.len();
                runtime.attempts_used = attempts_used;
                match exit {
                    PartExit::Complete { .. } => {
                        if runtime.downloaded != runtime.part.len() {
                            return Err(CoordinatorStop::Error(Error::Other(
                                "archive part worker completed before its interval".into(),
                            )));
                        }
                        Ok(JoinDisposition::Complete(part_id))
                    }
                    PartExit::Paused { .. } => Ok(JoinDisposition::Paused(part_id)),
                    PartExit::Failed(_) => unreachable!(),
                }
            }
            PartExit::Failed(failure) => {
                runtime.attempts_used = failure.attempts;
                Err(CoordinatorStop::Part(failure))
            }
        }
    }

    fn fill_pending_recovered(&mut self) -> Result<()> {
        while self.pause_senders.len() < self.max_concurrency {
            let Some(part_id) = self.pending_recovered.pop_front() else {
                break;
            };
            self.launch_part(part_id, None)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive_download::manifest::ManifestPart;

    #[tokio::test]
    async fn recovery_activates_incomplete_parts_by_start_and_queues_the_rest() {
        let temp = tempfile::tempdir().unwrap();
        let artifacts = ArchiveArtifacts::new(temp.path().join("archive.zip"));
        tokio::fs::create_dir_all(artifacts.parts_dir())
            .await
            .unwrap();
        let manifest = ArchiveManifest {
            version: 1,
            download_url: "http://127.0.0.1:9/archive".into(),
            total_len: 400,
            etag: None,
            last_modified: None,
            next_part_id: 4,
            parts: vec![
                ManifestPart {
                    id: 2,
                    start: 200,
                    end: 300,
                },
                ManifestPart {
                    id: 3,
                    start: 300,
                    end: 400,
                },
                ManifestPart {
                    id: 0,
                    start: 0,
                    end: 100,
                },
                ManifestPart {
                    id: 1,
                    start: 100,
                    end: 200,
                },
            ],
        };
        for (id, bytes) in [(0, 10), (1, 0), (2, 100), (3, 0)] {
            tokio::fs::write(
                ArchiveManifest::part_path(&artifacts, id),
                vec![0_u8; bytes],
            )
            .await
            .unwrap();
        }

        let mut coordinator = MultipartCoordinator::new(
            reqwest::Client::new(),
            EhCookies::default(),
            manifest.download_url.clone(),
            artifacts,
            manifest,
            2,
            None,
        )
        .await
        .unwrap();

        let mut active: Vec<_> = coordinator.pause_senders.keys().copied().collect();
        active.sort_unstable();
        assert_eq!(active, vec![0, 1]);
        assert_eq!(
            coordinator
                .pending_recovered
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![3]
        );
        assert!(!coordinator.runtime_parts[&2].active);
        assert_eq!(coordinator.runtime_parts[&2].downloaded, 100);
        coordinator.shutdown_tasks().await;
    }
}
