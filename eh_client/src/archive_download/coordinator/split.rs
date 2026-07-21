use super::{CoordinatorStop, JoinDisposition, MultipartCoordinator, RuntimePart};
use crate::archive_download::manifest::ManifestPart;
use crate::archive_download::part::{PartExit, PartSampleEvent};
use crate::archive_download::policy::{choose_split, SplitPlan};
use crate::error::Error;

impl MultipartCoordinator {
    pub(super) async fn rebalance_once(&mut self) -> std::result::Result<(), CoordinatorStop> {
        while let Ok(event) = self.samples_rx.try_recv() {
            self.reconcile_sample(event)
                .map_err(CoordinatorStop::Error)?;
        }
        self.fill_pending_recovered()
            .map_err(CoordinatorStop::Error)?;
        if !self.pending_recovered.is_empty() {
            return Ok(());
        }

        let inputs: Vec<_> = self
            .runtime_parts
            .values()
            .map(RuntimePart::split_input)
            .collect();
        let Some(plan) = choose_split(&inputs, self.pause_senders.len(), self.max_concurrency)
        else {
            return Ok(());
        };
        self.pause_and_split(plan).await
    }

    async fn pause_and_split(
        &mut self,
        plan: SplitPlan,
    ) -> std::result::Result<(), CoordinatorStop> {
        let sender = self.pause_senders.get(&plan.part_id).ok_or_else(|| {
            CoordinatorStop::Error(Error::Other("archive split target is not active".into()))
        })?;
        sender.send(true).map_err(|_| {
            CoordinatorStop::Error(Error::Other(
                "archive split target stopped before it could pause".into(),
            ))
        })?;

        loop {
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
                    Next::Join(joined.expect("split target keeps join set non-empty"))
                }
            };
            match next {
                Next::Sample(event) => {
                    self.reconcile_sample(event)
                        .map_err(CoordinatorStop::Error)?;
                }
                Next::Join(joined) => match self.reconcile_join(joined).await? {
                    JoinDisposition::Paused(part_id) if part_id == plan.part_id => break,
                    JoinDisposition::Complete(part_id) if part_id == plan.part_id => return Ok(()),
                    JoinDisposition::Complete(_) => {
                        self.fill_pending_recovered()
                            .map_err(CoordinatorStop::Error)?;
                    }
                    JoinDisposition::Paused(_) => {
                        return Err(CoordinatorStop::Error(Error::Other(
                            "unrelated archive part paused during split".into(),
                        )));
                    }
                },
            }
        }

        let runtime = self.runtime_parts.get(&plan.part_id).ok_or_else(|| {
            CoordinatorStop::Error(Error::Other("archive split target disappeared".into()))
        })?;
        let cursor = runtime
            .part
            .start
            .checked_add(runtime.downloaded)
            .ok_or_else(|| {
                CoordinatorStop::Error(Error::Other("archive split cursor overflows u64".into()))
            })?;
        let old_end = runtime.part.end;
        if runtime.downloaded == runtime.part.len() {
            return Ok(());
        }
        if plan.split_at <= cursor || plan.split_at >= old_end {
            return Err(CoordinatorStop::Error(Error::Other(
                "archive split point is outside the remaining interval".into(),
            )));
        }

        let new_part_id = self.manifest.next_part_id;
        let next_part_id = new_part_id.checked_add(1).ok_or_else(|| {
            CoordinatorStop::Error(Error::Other("archive part id overflows u64".into()))
        })?;
        let new_path = crate::archive_download::manifest::ArchiveManifest::part_path(
            &self.artifacts,
            new_part_id,
        );
        tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&new_path)
            .await
            .map_err(|error| CoordinatorStop::Error(error.into()))?;

        let mut proposed = self.manifest.clone();
        let selected = proposed
            .parts
            .iter_mut()
            .find(|part| part.id == plan.part_id)
            .ok_or_else(|| {
                CoordinatorStop::Error(Error::Other(
                    "archive split target is missing from the manifest".into(),
                ))
            })?;
        selected.end = plan.split_at;
        proposed.parts.push(ManifestPart {
            id: new_part_id,
            start: plan.split_at,
            end: old_end,
        });
        proposed.next_part_id = next_part_id;
        if let Err(error) = proposed.write_atomic(&self.artifacts).await {
            let _ = tokio::fs::remove_file(&new_path).await;
            return Err(CoordinatorStop::Error(error));
        }

        self.manifest = proposed;
        let selected = self
            .runtime_parts
            .get_mut(&plan.part_id)
            .expect("split target checked above");
        selected.part.end = plan.split_at;
        selected.generation = selected.generation.checked_add(1).ok_or_else(|| {
            CoordinatorStop::Error(Error::Other("archive part generation overflows u64".into()))
        })?;
        self.runtime_parts.insert(
            new_part_id,
            RuntimePart {
                part: ManifestPart {
                    id: new_part_id,
                    start: plan.split_at,
                    end: old_end,
                },
                downloaded: 0,
                ewma: Some(plan.new_rate),
                attempts_used: 0,
                active: false,
                has_stable_sample: false,
                generation: 0,
            },
        );
        self.launch_part(plan.part_id, None)
            .map_err(CoordinatorStop::Error)?;
        self.launch_part(new_part_id, None)
            .map_err(CoordinatorStop::Error)?;
        Ok(())
    }
}
