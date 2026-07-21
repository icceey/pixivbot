use super::artifacts::ArchiveArtifacts;
use super::manifest::ArchiveManifest;
use crate::error::{Error, Result};
use tokio::io::AsyncWriteExt;

pub(super) async fn assemble_parts(
    artifacts: &ArchiveArtifacts,
    manifest: &ArchiveManifest,
) -> Result<()> {
    artifacts.remove_assembly_scratch().await?;
    let mut scratch = tokio::fs::File::create(artifacts.assembly_scratch()).await?;
    let mut parts: Vec<_> = manifest.parts.iter().collect();
    parts.sort_by_key(|part| part.start);

    for part in parts {
        let expected_len = part.len();
        let part_path = ArchiveManifest::part_path(artifacts, part.id);
        let actual_len = tokio::fs::metadata(&part_path).await?.len();
        if actual_len != expected_len {
            return Err(Error::Other(format!(
                "archive part {} length mismatch: expected {expected_len} bytes, found {actual_len}",
                part.id
            )));
        }

        let mut source = tokio::fs::File::open(part_path).await?;
        let copied = tokio::io::copy(&mut source, &mut scratch).await?;
        if copied != expected_len {
            return Err(Error::Other(format!(
                "archive part {} copy length mismatch: expected {expected_len} bytes, copied {copied}",
                part.id
            )));
        }
    }

    scratch.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::assemble_parts;
    use crate::archive_download::manifest::{ArchiveManifest, ManifestPart};
    use crate::ArchiveArtifacts;

    #[tokio::test]
    async fn assembly_orders_intervals_and_copies_exact_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let artifacts = ArchiveArtifacts::new(temp.path().join("archive.zip"));
        let manifest = test_manifest(vec![
            ManifestPart {
                id: 7,
                start: 4,
                end: 8,
            },
            ManifestPart {
                id: 3,
                start: 0,
                end: 4,
            },
        ]);
        tokio::fs::create_dir_all(artifacts.parts_dir())
            .await
            .unwrap();
        tokio::fs::write(ArchiveManifest::part_path(&artifacts, 7), b"efgh")
            .await
            .unwrap();
        tokio::fs::write(ArchiveManifest::part_path(&artifacts, 3), b"abcd")
            .await
            .unwrap();
        tokio::fs::write(artifacts.assembly_scratch(), b"stale")
            .await
            .unwrap();

        assemble_parts(&artifacts, &manifest).await.unwrap();

        assert_eq!(
            tokio::fs::read(artifacts.assembly_scratch()).await.unwrap(),
            b"abcdefgh"
        );
    }

    #[tokio::test]
    async fn assembly_length_mismatch_preserves_manifest_and_parts() {
        let temp = tempfile::tempdir().unwrap();
        let artifacts = ArchiveArtifacts::new(temp.path().join("archive.zip"));
        let manifest = test_manifest(vec![
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
        ]);
        let mut persisted_manifest = manifest.clone();
        persisted_manifest.write_atomic(&artifacts).await.unwrap();
        let manifest_path = artifacts.parts_dir().join("manifest.json");
        let manifest_before = tokio::fs::read(&manifest_path).await.unwrap();
        let first_part = ArchiveManifest::part_path(&artifacts, 0);
        let short_part = ArchiveManifest::part_path(&artifacts, 1);
        tokio::fs::write(&first_part, b"abcd").await.unwrap();
        tokio::fs::write(&short_part, b"xyz").await.unwrap();

        let error = assemble_parts(&artifacts, &manifest).await.unwrap_err();

        assert!(error.to_string().contains("length mismatch"));
        assert_eq!(
            tokio::fs::read(&manifest_path).await.unwrap(),
            manifest_before
        );
        assert_eq!(tokio::fs::read(&first_part).await.unwrap(), b"abcd");
        assert_eq!(tokio::fs::read(&short_part).await.unwrap(), b"xyz");
    }

    fn test_manifest(parts: Vec<ManifestPart>) -> ArchiveManifest {
        ArchiveManifest {
            version: 1,
            download_url: "https://example.invalid/archive.zip".to_owned(),
            total_len: 8,
            etag: Some("\"strong-v1\"".to_owned()),
            last_modified: None,
            next_part_id: 8,
            parts,
        }
    }
}
