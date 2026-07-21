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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use crate::ArchiveArtifacts;

    const URL: &str = "https://example.invalid/archive.zip";

    #[tokio::test]
    async fn manifest_round_trip_replaces_existing_manifest_atomically() {
        let temp = tempfile::tempdir().unwrap();
        let artifacts = ArchiveArtifacts::new(temp.path().join("archive.zip"));
        let mut original = valid_manifest(URL);
        original.write_atomic(&artifacts).await.unwrap();

        let mut replacement = valid_manifest(URL);
        replacement.etag = Some("\"strong-v2\"".to_owned());
        replacement.write_atomic(&artifacts).await.unwrap();
        write_part(&artifacts, 0, b"12").await;
        write_part(&artifacts, 1, b"34").await;

        let stored: ArchiveManifest = serde_json::from_slice(
            &tokio::fs::read(ArchiveManifest::manifest_path(&artifacts))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(stored, replacement);
        assert_eq!(
            recover_manifest(&artifacts, URL).await.unwrap(),
            ManifestRecovery::Valid(replacement)
        );
    }

    #[tokio::test]
    async fn manifest_recovery_classifies_deterministic_invalid_states() {
        let temp = tempfile::tempdir().unwrap();
        let artifacts = ArchiveArtifacts::new(temp.path().join("archive.zip"));
        tokio::fs::create_dir_all(artifacts.parts_dir())
            .await
            .unwrap();
        assert_invalid(
            &artifacts,
            URL,
            ManifestInvalid::MissingManifest,
            "missing_manifest",
        )
        .await;

        tokio::fs::write(ArchiveManifest::manifest_path(&artifacts), b"not json")
            .await
            .unwrap();
        assert_invalid(
            &artifacts,
            URL,
            ManifestInvalid::MalformedJson,
            "malformed_json",
        )
        .await;

        let mut unsupported_version = valid_manifest(URL);
        unsupported_version.version = 2;
        let mut zero_total = valid_manifest(URL);
        zero_total.total_len = 0;
        let url_mismatch = valid_manifest(URL);
        let mut empty_parts = valid_manifest(URL);
        empty_parts.parts.clear();
        let mut gap = valid_manifest(URL);
        gap.parts[1].start = 5;
        let mut overlap = valid_manifest(URL);
        overlap.parts[1].start = 3;
        let mut zero_length = valid_manifest(URL);
        zero_length.parts[1].end = 4;
        let mut end_beyond_total = valid_manifest(URL);
        end_beyond_total.parts[1].end = 9;
        let mut duplicate_id = valid_manifest(URL);
        duplicate_id.parts[1].id = 0;
        let mut stale_next_part_id = valid_manifest(URL);
        stale_next_part_id.next_part_id = 1;
        let mut both_validators = valid_manifest(URL);
        both_validators.last_modified = Some("Tue, 21 Jul 2026 12:00:00 GMT".to_owned());
        let mut weak_etag = valid_manifest(URL);
        weak_etag.etag = Some("W/\"weak-v1\"".to_owned());
        let mut empty_etag = valid_manifest(URL);
        empty_etag.etag = Some(" ".to_owned());
        let mut empty_last_modified = valid_manifest(URL);
        empty_last_modified.etag = None;
        empty_last_modified.last_modified = Some(" ".to_owned());
        let cases = [
            (
                "unsupported_version",
                unsupported_version,
                URL,
                ManifestInvalid::UnsupportedVersion,
            ),
            ("zero_total", zero_total, URL, ManifestInvalid::ZeroTotal),
            (
                "url_mismatch",
                url_mismatch,
                "https://example.invalid/other.zip",
                ManifestInvalid::UrlMismatch,
            ),
            ("empty_parts", empty_parts, URL, ManifestInvalid::EmptyParts),
            ("gap", gap, URL, ManifestInvalid::InvalidIntervalCoverage),
            (
                "overlap",
                overlap,
                URL,
                ManifestInvalid::InvalidIntervalCoverage,
            ),
            (
                "zero_length",
                zero_length,
                URL,
                ManifestInvalid::InvalidIntervalCoverage,
            ),
            (
                "end_beyond_total",
                end_beyond_total,
                URL,
                ManifestInvalid::InvalidIntervalCoverage,
            ),
            (
                "duplicate_id",
                duplicate_id,
                URL,
                ManifestInvalid::DuplicatePartId,
            ),
            (
                "stale_next_part_id",
                stale_next_part_id,
                URL,
                ManifestInvalid::InvalidNextPartId,
            ),
            (
                "both_validators",
                both_validators,
                URL,
                ManifestInvalid::InvalidValidator,
            ),
            (
                "weak_etag",
                weak_etag,
                URL,
                ManifestInvalid::InvalidValidator,
            ),
            (
                "empty_etag",
                empty_etag,
                URL,
                ManifestInvalid::InvalidValidator,
            ),
            (
                "empty_last_modified",
                empty_last_modified,
                URL,
                ManifestInvalid::InvalidValidator,
            ),
        ];
        for (name, manifest, current_url, expected) in cases {
            write_manifest(&artifacts, &manifest).await;
            assert_invalid(&artifacts, current_url, expected, name).await;
        }

        write_manifest(&artifacts, &valid_manifest(URL)).await;
        assert_invalid(
            &artifacts,
            URL,
            ManifestInvalid::MissingReferencedPart { part_id: 0 },
            "missing_referenced_part",
        )
        .await;

        write_part(&artifacts, 0, b"12345").await;
        assert_invalid(
            &artifacts,
            URL,
            ManifestInvalid::OversizedPart { part_id: 0 },
            "oversized_referenced_part",
        )
        .await;
    }

    #[test]
    fn manifest_interval_and_next_id_failures_are_classified_independently() {
        let mut incomplete_final_coverage = valid_manifest(URL);
        incomplete_final_coverage.parts[1].end = 7;
        let mut stale_next_part_id = valid_manifest(URL);
        stale_next_part_id.next_part_id = 1;
        let cases = [
            (
                "incomplete_final_coverage",
                incomplete_final_coverage,
                ManifestInvalid::InvalidIntervalCoverage,
            ),
            (
                "stale_next_part_id",
                stale_next_part_id,
                ManifestInvalid::InvalidNextPartId,
            ),
        ];

        for (name, manifest, expected) in cases {
            assert_eq!(manifest.validate_shape(URL), Err(expected), "{name}");
        }
    }

    #[tokio::test]
    async fn manifest_recovery_removes_only_unreferenced_parts() {
        let temp = tempfile::tempdir().unwrap();
        let artifacts = ArchiveArtifacts::new(temp.path().join("archive.zip"));
        let manifest = valid_manifest(URL);
        write_manifest(&artifacts, &manifest).await;
        write_part(&artifacts, 0, b"12").await;
        write_part(&artifacts, 1, b"34").await;
        let unreferenced = ArchiveManifest::part_path(&artifacts, 99);
        let abandoned_temp = artifacts.parts_dir().join("manifest.json.tmp-abandoned");
        tokio::fs::write(&unreferenced, b"old").await.unwrap();
        tokio::fs::write(&abandoned_temp, b"old").await.unwrap();

        assert_eq!(
            recover_manifest(&artifacts, URL).await.unwrap(),
            ManifestRecovery::Valid(manifest)
        );
        assert!(ArchiveManifest::part_path(&artifacts, 0).is_file());
        assert!(ArchiveManifest::part_path(&artifacts, 1).is_file());
        assert!(!unreferenced.exists());
        assert!(!abandoned_temp.exists());
    }

    #[tokio::test]
    async fn manifest_recovery_io_error_propagates_and_preserves_state() {
        let temp = tempfile::tempdir().unwrap();
        let artifacts = ArchiveArtifacts::new(temp.path().join("archive.zip"));
        tokio::fs::create_dir_all(ArchiveManifest::manifest_path(&artifacts))
            .await
            .unwrap();
        let sentinel = ArchiveManifest::part_path(&artifacts, 77);
        tokio::fs::write(&sentinel, b"sentinel").await.unwrap();

        let error = recover_manifest(&artifacts, URL).await.unwrap_err();
        assert!(matches!(error, Error::Io(_)));
        assert!(ArchiveManifest::manifest_path(&artifacts).is_dir());
        assert!(sentinel.is_file());
    }

    fn valid_manifest(url: &str) -> ArchiveManifest {
        ArchiveManifest {
            version: 1,
            download_url: url.to_owned(),
            total_len: 8,
            etag: Some("\"strong-v1\"".to_owned()),
            last_modified: None,
            next_part_id: 2,
            parts: vec![
                ManifestPart {
                    id: 0,
                    start: 0,
                    end: 4,
                },
                ManifestPart {
                    id: 1,
                    start: 4,
                    end: 8,
                },
            ],
        }
    }

    async fn write_manifest(artifacts: &ArchiveArtifacts, manifest: &ArchiveManifest) {
        tokio::fs::create_dir_all(artifacts.parts_dir())
            .await
            .unwrap();
        tokio::fs::write(
            ArchiveManifest::manifest_path(artifacts),
            serde_json::to_vec(manifest).unwrap(),
        )
        .await
        .unwrap();
    }

    async fn write_part(artifacts: &ArchiveArtifacts, id: u64, contents: &[u8]) {
        tokio::fs::create_dir_all(artifacts.parts_dir())
            .await
            .unwrap();
        tokio::fs::write(ArchiveManifest::part_path(artifacts, id), contents)
            .await
            .unwrap();
    }

    async fn assert_invalid(
        artifacts: &ArchiveArtifacts,
        current_url: &str,
        expected: ManifestInvalid,
        name: &str,
    ) {
        assert_eq!(
            recover_manifest(artifacts, current_url).await.unwrap(),
            ManifestRecovery::Invalid(expected),
            "{name}"
        );
    }
}
