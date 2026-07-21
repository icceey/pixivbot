use super::{MultipartCoordinator, RuntimePart};
use crate::archive_download::artifacts::ArchiveArtifacts;
use crate::archive_download::manifest::ArchiveManifest;
use crate::archive_download::part::{aggregate_downloaded_bytes, SeedResponse, Validator};
use crate::error::{Error, Result};
use crate::models::EhCookies;
use std::collections::{HashMap, VecDeque};
use std::time::Instant;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

impl MultipartCoordinator {
    #[allow(clippy::too_many_arguments)]
    pub(in crate::archive_download) async fn new(
        http: reqwest::Client,
        cookies: EhCookies,
        url: String,
        artifacts: ArchiveArtifacts,
        mut manifest: ArchiveManifest,
        max_concurrency: usize,
        mut seed: Option<SeedResponse>,
    ) -> Result<Self> {
        if max_concurrency == 0 {
            return Err(Error::Other(
                "archive download max_concurrency must be at least 1".into(),
            ));
        }
        manifest.parts.sort_by_key(|part| part.start);
        let validator = Validator::from_manifest(&manifest);
        let mut runtime_parts = HashMap::new();
        let mut incomplete = Vec::new();
        let initial_downloaded = aggregate_downloaded_bytes(&artifacts, &manifest).await?;
        for part in &manifest.parts {
            let metadata =
                tokio::fs::metadata(ArchiveManifest::part_path(&artifacts, part.id)).await?;
            if !metadata.is_file() {
                return Err(Error::Other(format!(
                    "archive part {} is not a regular file",
                    part.id
                )));
            }
            let downloaded = metadata.len();
            if downloaded > part.len() {
                return Err(Error::Other(format!(
                    "archive part {} exceeds its interval",
                    part.id
                )));
            }
            if downloaded < part.len() {
                incomplete.push(part.id);
            }
            runtime_parts.insert(
                part.id,
                RuntimePart {
                    part: part.clone(),
                    downloaded,
                    ewma: None,
                    attempts_used: 0,
                    active: false,
                    has_stable_sample: false,
                    generation: 0,
                },
            );
        }
        let (samples_tx, samples_rx) = mpsc::unbounded_channel();
        let mut coordinator = Self {
            http,
            cookies,
            url,
            artifacts,
            manifest,
            validator,
            runtime_parts,
            pending_recovered: VecDeque::new(),
            pause_senders: HashMap::new(),
            joins: JoinSet::new(),
            samples_tx,
            samples_rx,
            max_concurrency,
            initial_downloaded,
            started_at: Instant::now(),
        };
        for (index, part_id) in incomplete.into_iter().enumerate() {
            if index < max_concurrency {
                let part_seed = (part_id == 0).then(|| seed.take()).flatten();
                coordinator.launch_part(part_id, part_seed)?;
            } else {
                coordinator.pending_recovered.push_back(part_id);
            }
        }
        Ok(coordinator)
    }
}
