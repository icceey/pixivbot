use super::artifacts::ArchiveArtifacts;
use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::PathBuf;

const MANIFEST_VERSION: u32 = 1;
const MANIFEST_FILE: &str = "manifest.json";
const MANIFEST_TEMP_PREFIX: &str = "manifest.json.tmp-";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct ArchiveManifest {
    pub(super) version: u32,
    pub(super) download_url: String,
    pub(super) total_len: u64,
    pub(super) etag: Option<String>,
    pub(super) last_modified: Option<String>,
    pub(super) next_part_id: u64,
    pub(super) parts: Vec<ManifestPart>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct ManifestPart {
    pub(super) id: u64,
    pub(super) start: u64,
    pub(super) end: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ManifestRecovery {
    Valid(ArchiveManifest),
    Invalid(ManifestInvalid),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ManifestInvalid {
    MissingManifest,
    MalformedJson,
    UnsupportedVersion,
    ZeroTotal,
    UrlMismatch,
    EmptyParts,
    InvalidValidator,
    InvalidIntervalCoverage,
    DuplicatePartId,
    InvalidNextPartId,
    MissingReferencedPart { part_id: u64 },
    OversizedPart { part_id: u64 },
}

impl ManifestPart {
    pub(super) fn len(&self) -> u64 {
        self.end - self.start
    }
}

impl ArchiveManifest {
    fn manifest_path(artifacts: &ArchiveArtifacts) -> PathBuf {
        artifacts.parts_dir().join(MANIFEST_FILE)
    }

    pub(super) fn part_path(artifacts: &ArchiveArtifacts, id: u64) -> PathBuf {
        artifacts.parts_dir().join(format!("part-{id:016}"))
    }

    fn validate_shape(&self, current_url: &str) -> std::result::Result<(), ManifestInvalid> {
        if self.version != MANIFEST_VERSION {
            return Err(ManifestInvalid::UnsupportedVersion);
        }
        if self.total_len == 0 {
            return Err(ManifestInvalid::ZeroTotal);
        }
        if self.download_url != current_url {
            return Err(ManifestInvalid::UrlMismatch);
        }
        if self.parts.is_empty() {
            return Err(ManifestInvalid::EmptyParts);
        }
        if self.etag.is_some() && self.last_modified.is_some()
            || self
                .etag
                .as_deref()
                .is_some_and(|value| value.trim().is_empty() || value.trim().starts_with("W/"))
            || self
                .last_modified
                .as_deref()
                .is_some_and(|value| value.trim().is_empty())
        {
            return Err(ManifestInvalid::InvalidValidator);
        }

        let mut expected_start = 0;
        let mut ids = HashSet::new();
        for part in &self.parts {
            if part.start != expected_start || part.end <= part.start || part.end > self.total_len {
                return Err(ManifestInvalid::InvalidIntervalCoverage);
            }
            if !ids.insert(part.id) {
                return Err(ManifestInvalid::DuplicatePartId);
            }
            expected_start = part.end;
        }
        if expected_start != self.total_len {
            return Err(ManifestInvalid::InvalidIntervalCoverage);
        }
        let max_id = self
            .parts
            .iter()
            .map(|part| part.id)
            .max()
            .expect("parts checked non-empty");
        if self.next_part_id <= max_id {
            return Err(ManifestInvalid::InvalidNextPartId);
        }
        Ok(())
    }

    pub(super) async fn write_atomic(&mut self, artifacts: &ArchiveArtifacts) -> Result<()> {
        self.parts.sort_by_key(|part| part.start);
        let current_url = self.download_url.clone();
        self.validate_shape(&current_url).map_err(|reason| {
            Error::Other(format!(
                "refusing to persist invalid archive manifest: {reason:?}"
            ))
        })?;
        let bytes = serde_json::to_vec_pretty(&*self)?;
        let parts_dir = artifacts.parts_dir().to_path_buf();
        let manifest_path = Self::manifest_path(artifacts);
        tokio::task::spawn_blocking(move || -> Result<()> {
            std::fs::create_dir_all(&parts_dir)?;
            let mut temp = tempfile::Builder::new()
                .prefix(MANIFEST_TEMP_PREFIX)
                .tempfile_in(&parts_dir)?;
            std::io::Write::write_all(temp.as_file_mut(), &bytes)?;
            std::io::Write::flush(temp.as_file_mut())?;
            temp.as_file().sync_all()?;
            drop(
                temp.persist(&manifest_path)
                    .map_err(|error| Error::Io(error.error))?,
            );
            Ok(())
        })
        .await
        .map_err(|error| Error::Other(format!("archive manifest writer task failed: {error}")))?
    }
}

pub(super) async fn recover_manifest(
    artifacts: &ArchiveArtifacts,
    current_url: &str,
) -> Result<ManifestRecovery> {
    let bytes = match tokio::fs::read(ArchiveManifest::manifest_path(artifacts)).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(ManifestRecovery::Invalid(ManifestInvalid::MissingManifest));
        }
        Err(error) => return Err(error.into()),
    };
    let manifest: ArchiveManifest = match serde_json::from_slice(&bytes) {
        Ok(manifest) => manifest,
        Err(_) => return Ok(ManifestRecovery::Invalid(ManifestInvalid::MalformedJson)),
    };
    if let Err(reason) = manifest.validate_shape(current_url) {
        return Ok(ManifestRecovery::Invalid(reason));
    }
    for part in &manifest.parts {
        let metadata =
            match tokio::fs::metadata(ArchiveManifest::part_path(artifacts, part.id)).await {
                Ok(metadata) if metadata.is_file() => metadata,
                Ok(_) => {
                    return Ok(ManifestRecovery::Invalid(
                        ManifestInvalid::MissingReferencedPart { part_id: part.id },
                    ));
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    return Ok(ManifestRecovery::Invalid(
                        ManifestInvalid::MissingReferencedPart { part_id: part.id },
                    ));
                }
                Err(error) => return Err(error.into()),
            };
        if metadata.len() > part.len() {
            return Ok(ManifestRecovery::Invalid(ManifestInvalid::OversizedPart {
                part_id: part.id,
            }));
        }
    }
    cleanup_unreferenced_parts(artifacts, &manifest).await?;
    Ok(ManifestRecovery::Valid(manifest))
}

pub(super) async fn recover_persisted_download(
    artifacts: &ArchiveArtifacts,
) -> Result<Option<(String, u64)>> {
    let bytes = match tokio::fs::read(ArchiveManifest::manifest_path(artifacts)).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let manifest: ArchiveManifest = match serde_json::from_slice(&bytes) {
        Ok(manifest) => manifest,
        Err(_) => return Ok(None),
    };
    let download_url = manifest.download_url.clone();
    match recover_manifest(artifacts, &download_url).await? {
        ManifestRecovery::Valid(manifest) => Ok(Some((download_url, manifest.total_len))),
        ManifestRecovery::Invalid(_) => Ok(None),
    }
}

async fn cleanup_unreferenced_parts(
    artifacts: &ArchiveArtifacts,
    manifest: &ArchiveManifest,
) -> Result<()> {
    let referenced_names: HashSet<_> = manifest
        .parts
        .iter()
        .map(|part| format!("part-{:016}", part.id))
        .collect();
    let mut entries = tokio::fs::read_dir(artifacts.parts_dir()).await?;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        if name == MANIFEST_FILE || referenced_names.contains(&name.to_string_lossy().into_owned())
        {
            continue;
        }
        if entry.file_type().await?.is_dir() {
            tokio::fs::remove_dir_all(entry.path()).await?;
        } else {
            tokio::fs::remove_file(entry.path()).await?;
        }
    }
    Ok(())
}
