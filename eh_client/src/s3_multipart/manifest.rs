use super::{ProviderKind, PART_SIZE};
use crate::error::Error;
use serde::{Deserialize, Serialize};
use std::io::{ErrorKind, Write};
use std::path::Path;

const MANIFEST_VERSION: u32 = 1;
const MANIFEST_TEMP_PREFIX: &str = "manifest.json.tmp-";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct MultipartManifest {
    pub(super) version: u32,
    pub(super) provider: ProviderKind,
    pub(super) uploader_identity_sha256: String,
    pub(super) object_key: String,
    pub(super) logical_object_id: String,
    pub(super) object_sha256: String,
    pub(super) object_len: u64,
    pub(super) content_type: String,
    pub(super) part_size: u64,
    pub(super) upload_id: String,
    pub(super) zip_extraction_prefix: Option<String>,
    pub(super) requested_entries_sha256: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ManifestIdentity<'a> {
    pub(super) provider: ProviderKind,
    pub(super) uploader_identity_sha256: &'a str,
    pub(super) logical_object_id: &'a str,
    pub(super) object_sha256: &'a str,
    pub(super) object_len: u64,
    pub(super) content_type: &'a str,
    pub(super) requested_entries_sha256: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ManifestLoad {
    Missing,
    Valid(MultipartManifest),
    Stale {
        manifest: MultipartManifest,
        reason: ManifestMismatch,
    },
    MalformedJson,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ManifestMismatch {
    UnsupportedVersion,
    Provider,
    UploaderIdentity,
    LogicalObject,
    ObjectFingerprint,
    ObjectLength,
    ContentType,
    PartSize,
    ZipOptions,
    InvalidStoredValue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TerminalAbortManifest {
    pub(super) object_key: String,
    pub(super) upload_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TerminalAbortManifestLoad {
    Verified(TerminalAbortManifest),
    Missing,
    MalformedJson,
    Unverifiable,
}

pub(super) fn is_terminal_abort_manifest_candidate(path: &Path) -> bool {
    path.extension().and_then(|extension| extension.to_str()) == Some("json")
        || is_temporary_manifest(path)
}

pub(super) fn is_temporary_manifest(path: &Path) -> bool {
    path.file_name()
        .and_then(|file_name| file_name.to_str())
        .is_some_and(|file_name| file_name.starts_with(MANIFEST_TEMP_PREFIX))
}

pub(super) async fn load_manifest(
    path: &Path,
    identity: &ManifestIdentity<'_>,
) -> crate::Result<ManifestLoad> {
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(ManifestLoad::Missing),
        Err(error) => return Err(error.into()),
    };
    let manifest: MultipartManifest = match serde_json::from_slice(&bytes) {
        Ok(manifest) => manifest,
        Err(_) => return Ok(ManifestLoad::MalformedJson),
    };
    match validate_manifest(&manifest, identity) {
        Ok(()) => Ok(ManifestLoad::Valid(manifest)),
        Err(reason) => Ok(ManifestLoad::Stale { manifest, reason }),
    }
}

/// Load only the identity needed to safely abort a terminal multipart session.
///
/// Terminal cleanup deliberately does not compare the current object bytes or
/// fingerprint: after a task has reached a terminal state, it must be able to
/// clean a matching persisted session even when the archive is being removed.
pub(super) async fn load_terminal_abort_manifest(
    path: &Path,
    provider: ProviderKind,
    uploader_identity_sha256: &str,
) -> crate::Result<TerminalAbortManifestLoad> {
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(TerminalAbortManifestLoad::Missing);
        }
        Err(error) => return Err(error.into()),
    };
    let manifest: MultipartManifest = match serde_json::from_slice(&bytes) {
        Ok(manifest) => manifest,
        Err(_) => return Ok(TerminalAbortManifestLoad::MalformedJson),
    };
    if manifest.version != MANIFEST_VERSION
        || validate_stored_values(&manifest).is_err()
        || manifest.provider != provider
        || manifest.uploader_identity_sha256 != uploader_identity_sha256
    {
        return Ok(TerminalAbortManifestLoad::Unverifiable);
    }
    Ok(TerminalAbortManifestLoad::Verified(TerminalAbortManifest {
        object_key: manifest.object_key,
        upload_id: manifest.upload_id,
    }))
}

pub(super) fn new_manifest(
    identity: &ManifestIdentity<'_>,
    object_key: String,
    upload_id: String,
) -> crate::Result<MultipartManifest> {
    validate_identity(identity)?;
    if !is_nonempty_value(&object_key) || !is_nonempty_value(&upload_id) {
        return Err(Error::Other(
            "multipart manifest object key and upload ID must be non-empty".to_owned(),
        ));
    }

    let (zip_extraction_prefix, requested_entries_sha256) = match identity.requested_entries_sha256
    {
        Some(requested_entries_sha256) => {
            let Some(stem) = object_key.strip_suffix(".zip") else {
                return Err(Error::Other(
                    "ZIP multipart manifests require an object key ending in .zip".to_owned(),
                ));
            };
            (
                Some(format!("{stem}/")),
                Some(requested_entries_sha256.to_owned()),
            )
        }
        None => (None, None),
    };

    Ok(MultipartManifest {
        version: MANIFEST_VERSION,
        provider: identity.provider,
        uploader_identity_sha256: identity.uploader_identity_sha256.to_owned(),
        object_key,
        logical_object_id: identity.logical_object_id.to_owned(),
        object_sha256: identity.object_sha256.to_owned(),
        object_len: identity.object_len,
        content_type: identity.content_type.to_owned(),
        part_size: PART_SIZE as u64,
        upload_id,
        zip_extraction_prefix,
        requested_entries_sha256,
    })
}

pub(super) async fn write_manifest_atomic(
    path: &Path,
    manifest: &MultipartManifest,
) -> crate::Result<()> {
    let bytes = serde_json::to_vec_pretty(manifest)?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || -> crate::Result<()> {
        std::fs::create_dir_all(&parent)?;
        let mut temp = tempfile::Builder::new()
            .prefix(MANIFEST_TEMP_PREFIX)
            .tempfile_in(&parent)?;
        temp.write_all(&bytes)?;
        temp.flush()?;
        temp.as_file().sync_all()?;
        drop(
            temp.persist(&path)
                .map_err(|error| Error::Io(error.error))?,
        );
        Ok(())
    })
    .await
    .map_err(|error| Error::Other(format!("multipart manifest writer task failed: {error}")))?
}

pub(super) async fn remove_manifest(path: &Path) -> crate::Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn validate_manifest(
    manifest: &MultipartManifest,
    identity: &ManifestIdentity<'_>,
) -> std::result::Result<(), ManifestMismatch> {
    if manifest.version != MANIFEST_VERSION {
        return Err(ManifestMismatch::UnsupportedVersion);
    }
    validate_stored_values(manifest)?;
    if manifest.provider != identity.provider {
        return Err(ManifestMismatch::Provider);
    }
    if manifest.uploader_identity_sha256 != identity.uploader_identity_sha256 {
        return Err(ManifestMismatch::UploaderIdentity);
    }
    if manifest.logical_object_id != identity.logical_object_id {
        return Err(ManifestMismatch::LogicalObject);
    }
    if manifest.object_sha256 != identity.object_sha256 {
        return Err(ManifestMismatch::ObjectFingerprint);
    }
    if manifest.object_len != identity.object_len {
        return Err(ManifestMismatch::ObjectLength);
    }
    if manifest.content_type != identity.content_type {
        return Err(ManifestMismatch::ContentType);
    }
    if manifest.part_size != PART_SIZE as u64 {
        return Err(ManifestMismatch::PartSize);
    }

    let expected_prefix = manifest
        .object_key
        .strip_suffix(".zip")
        .map(|stem| format!("{stem}/"));
    match identity.requested_entries_sha256 {
        Some(requested_entries_sha256)
            if manifest.zip_extraction_prefix.as_ref() == expected_prefix.as_ref()
                && manifest.requested_entries_sha256.as_deref()
                    == Some(requested_entries_sha256) =>
        {
            Ok(())
        }
        Some(_) => Err(ManifestMismatch::ZipOptions),
        None if manifest.zip_extraction_prefix.is_none()
            && manifest.requested_entries_sha256.is_none() =>
        {
            Ok(())
        }
        None => Err(ManifestMismatch::ZipOptions),
    }
}

fn validate_identity(identity: &ManifestIdentity<'_>) -> crate::Result<()> {
    let required_values = [
        identity.uploader_identity_sha256,
        identity.logical_object_id,
        identity.object_sha256,
        identity.content_type,
    ];
    if required_values
        .into_iter()
        .any(|value| !is_nonempty_value(value))
        || !is_lowercase_sha256(identity.uploader_identity_sha256)
        || !is_lowercase_sha256(identity.object_sha256)
        || identity
            .requested_entries_sha256
            .is_some_and(|value| !is_lowercase_sha256(value))
    {
        return Err(Error::Other(
            "multipart manifest identity contains an invalid value".to_owned(),
        ));
    }
    Ok(())
}

fn validate_stored_values(
    manifest: &MultipartManifest,
) -> std::result::Result<(), ManifestMismatch> {
    let required_values = [
        manifest.uploader_identity_sha256.as_str(),
        manifest.object_key.as_str(),
        manifest.logical_object_id.as_str(),
        manifest.object_sha256.as_str(),
        manifest.content_type.as_str(),
        manifest.upload_id.as_str(),
    ];
    if required_values
        .into_iter()
        .any(|value| !is_nonempty_value(value))
        || !is_lowercase_sha256(&manifest.uploader_identity_sha256)
        || !is_lowercase_sha256(&manifest.object_sha256)
        || manifest
            .requested_entries_sha256
            .as_deref()
            .is_some_and(|value| !is_lowercase_sha256(value))
        || manifest
            .zip_extraction_prefix
            .as_deref()
            .is_some_and(|value| !is_nonempty_value(value))
    {
        return Err(ManifestMismatch::InvalidStoredValue);
    }
    Ok(())
}

fn is_nonempty_value(value: &str) -> bool {
    !value.trim().is_empty() && !value.contains('\0')
}

fn is_lowercase_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use crate::s3_multipart::{
        fingerprint_fields, requested_entries_fingerprint, sha256_hex,
        uploader_identity_fingerprint,
    };
    use std::io::ErrorKind;

    const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const HASH_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[tokio::test]
    async fn manifest_round_trip_is_atomic_and_contains_only_session_identity() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state").join("archive.json");
        let object_bytes = b"unique-object-byte-sentinel-never-persisted";
        let object_hash = sha256_hex(object_bytes);
        let identity = ManifestIdentity {
            object_sha256: &object_hash,
            ..standard_identity()
        };
        let original = new_manifest(
            &identity,
            "objects/archive.bin".to_owned(),
            "upload-1".to_owned(),
        )
        .unwrap();
        write_manifest_atomic(&path, &original).await.unwrap();

        let replacement = new_manifest(
            &identity,
            "objects/archive.bin".to_owned(),
            "upload-2".to_owned(),
        )
        .unwrap();
        write_manifest_atomic(&path, &replacement).await.unwrap();

        assert_eq!(
            load_manifest(&path, &identity).await.unwrap(),
            ManifestLoad::Valid(replacement.clone())
        );
        let json = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(
            serde_json::from_str::<MultipartManifest>(&json).unwrap(),
            replacement
        );
        assert!(json.contains("upload-2"));
        assert!(json.contains(&object_hash));
        assert_eq!(object_hash.len(), 64);
        for forbidden in [
            "AKIA_TEST",
            "secret-test-value",
            "X-Amz-",
            "unique-object-byte-sentinel-never-persisted",
            "\"etag\"",
        ] {
            assert!(
                !json.contains(forbidden),
                "persisted forbidden value {forbidden}"
            );
        }

        let mut entries = tokio::fs::read_dir(path.parent().unwrap()).await.unwrap();
        let mut names = Vec::new();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
        assert_eq!(names, ["archive.json"]);
    }

    #[tokio::test]
    async fn manifest_identity_rejects_provider_uploader_logical_object_content_and_zip_changes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("archive.json");
        let standard = standard_identity();
        let manifest = new_manifest(
            &standard,
            "objects/archive.bin".to_owned(),
            "upload-1".to_owned(),
        )
        .unwrap();
        let changed_provider = ManifestIdentity {
            provider: ProviderKind::IpfS3,
            ..standard
        };
        let changed_uploader = ManifestIdentity {
            uploader_identity_sha256: HASH_B,
            ..standard
        };
        let changed_logical = ManifestIdentity {
            logical_object_id: "gallery-456",
            ..standard
        };
        let changed_object = ManifestIdentity {
            object_sha256: HASH_B,
            ..standard
        };
        let changed_len = ManifestIdentity {
            object_len: 100,
            ..standard
        };
        let changed_content_type = ManifestIdentity {
            content_type: "application/zip",
            ..standard
        };
        let cases = [
            (changed_provider, ManifestMismatch::Provider),
            (changed_uploader, ManifestMismatch::UploaderIdentity),
            (changed_logical, ManifestMismatch::LogicalObject),
            (changed_object, ManifestMismatch::ObjectFingerprint),
            (changed_len, ManifestMismatch::ObjectLength),
            (changed_content_type, ManifestMismatch::ContentType),
        ];
        for (identity, reason) in cases {
            write_manifest_atomic(&path, &manifest).await.unwrap();
            assert_stale(&path, &identity, reason).await;
        }

        let mut wrong_part_size = manifest.clone();
        wrong_part_size.part_size += 1;
        write_manifest_atomic(&path, &wrong_part_size)
            .await
            .unwrap();
        assert_stale(&path, &standard, ManifestMismatch::PartSize).await;

        let mut unexpected_standard_zip_options = manifest.clone();
        unexpected_standard_zip_options.zip_extraction_prefix = Some("objects/archive/".to_owned());
        write_manifest_atomic(&path, &unexpected_standard_zip_options)
            .await
            .unwrap();
        assert_stale(&path, &standard, ManifestMismatch::ZipOptions).await;

        let entries = ["one.jpg".to_owned(), "two.jpg".to_owned()];
        let entry_hash = requested_entries_fingerprint(&entries);
        let zip_identity = ManifestIdentity {
            requested_entries_sha256: Some(&entry_hash),
            ..standard
        };
        let zip_manifest = new_manifest(
            &zip_identity,
            "objects/archive.zip".to_owned(),
            "upload-zip".to_owned(),
        )
        .unwrap();
        let mut wrong_prefix = zip_manifest.clone();
        wrong_prefix.zip_extraction_prefix = Some("objects/not-archive/".to_owned());
        write_manifest_atomic(&path, &wrong_prefix).await.unwrap();
        assert_stale(&path, &zip_identity, ManifestMismatch::ZipOptions).await;

        let mut wrong_entries = zip_manifest;
        wrong_entries.requested_entries_sha256 = Some(HASH_B.to_owned());
        write_manifest_atomic(&path, &wrong_entries).await.unwrap();
        assert_stale(&path, &zip_identity, ManifestMismatch::ZipOptions).await;
    }

    #[test]
    fn fingerprints_are_length_delimited_deterministic_and_credential_free() {
        let split_one = fingerprint_fields(&["ab", "c"]);
        let split_two = fingerprint_fields(&["a", "bc"]);
        assert_ne!(split_one, split_two);
        assert_eq!(split_one, fingerprint_fields(&["ab", "c"]));
        assert_eq!(split_one.len(), 64);
        assert!(split_one
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit()));

        let credential_sentinel = "secret-test-value";
        let uploader = uploader_identity_fingerprint(
            ProviderKind::S3,
            "https://s3.example.invalid",
            "us-east-1",
            "bucket",
            true,
        );
        assert_eq!(
            uploader,
            fingerprint_fields(&[
                "s3",
                "https://s3.example.invalid",
                "us-east-1",
                "bucket",
                "true",
            ])
        );
        assert!(!uploader.contains(credential_sentinel));
        assert_eq!(
            requested_entries_fingerprint(&["a".to_owned(), "bc".to_owned()]),
            fingerprint_fields(&["a", "bc"])
        );
        assert_ne!(
            requested_entries_fingerprint(&["a".to_owned(), "bc".to_owned()]),
            requested_entries_fingerprint(&["bc".to_owned(), "a".to_owned()])
        );
    }

    #[tokio::test]
    async fn load_classifies_missing_malformed_invalid_and_io_errors() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("archive.json");
        let identity = standard_identity();
        assert_eq!(
            load_manifest(&path, &identity).await.unwrap(),
            ManifestLoad::Missing
        );

        tokio::fs::write(&path, b"not json").await.unwrap();
        assert_eq!(
            load_manifest(&path, &identity).await.unwrap(),
            ManifestLoad::MalformedJson
        );

        let invalid = MultipartManifest {
            version: 1,
            provider: ProviderKind::S3,
            uploader_identity_sha256: HASH_A.to_owned(),
            object_key: String::new(),
            logical_object_id: "gallery-123".to_owned(),
            object_sha256: HASH_A.to_owned(),
            object_len: 99,
            content_type: "image/jpeg".to_owned(),
            part_size: super::super::PART_SIZE as u64,
            upload_id: "upload-1".to_owned(),
            zip_extraction_prefix: None,
            requested_entries_sha256: None,
        };
        write_manifest_atomic(&path, &invalid).await.unwrap();
        assert_stale(&path, &identity, ManifestMismatch::InvalidStoredValue).await;

        let mut unsupported_version = new_manifest(
            &identity,
            "objects/archive.bin".to_owned(),
            "upload-1".to_owned(),
        )
        .unwrap();
        unsupported_version.version = 2;
        write_manifest_atomic(&path, &unsupported_version)
            .await
            .unwrap();
        assert_stale(&path, &identity, ManifestMismatch::UnsupportedVersion).await;

        let io_path = temp.path().join("io-error.json");
        tokio::fs::create_dir(&io_path).await.unwrap();
        let error = load_manifest(&io_path, &identity).await.unwrap_err();
        assert!(matches!(error, Error::Io(error) if error.kind() != ErrorKind::NotFound));
    }

    #[tokio::test]
    async fn terminal_abort_load_classifies_verifiable_and_unverifiable_manifests() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("archive.json");
        let identity = standard_identity();
        assert_eq!(
            load_terminal_abort_manifest(
                &path,
                identity.provider,
                identity.uploader_identity_sha256
            )
            .await
            .unwrap(),
            TerminalAbortManifestLoad::Missing
        );

        tokio::fs::write(&path, b"not json").await.unwrap();
        assert_eq!(
            load_terminal_abort_manifest(
                &path,
                identity.provider,
                identity.uploader_identity_sha256
            )
            .await
            .unwrap(),
            TerminalAbortManifestLoad::MalformedJson
        );

        let valid = new_manifest(
            &identity,
            "objects/archive.bin".to_owned(),
            "upload-1".to_owned(),
        )
        .unwrap();
        let mut unsupported_version = valid.clone();
        unsupported_version.version = MANIFEST_VERSION + 1;
        let mut invalid_stored_value = valid.clone();
        invalid_stored_value.object_key = String::new();
        let mut wrong_provider = valid.clone();
        wrong_provider.provider = ProviderKind::IpfS3;
        let mut wrong_uploader = valid.clone();
        wrong_uploader.uploader_identity_sha256 = HASH_B.to_owned();
        for manifest in [
            unsupported_version,
            invalid_stored_value,
            wrong_provider,
            wrong_uploader,
        ] {
            write_manifest_atomic(&path, &manifest).await.unwrap();
            assert_eq!(
                load_terminal_abort_manifest(
                    &path,
                    identity.provider,
                    identity.uploader_identity_sha256
                )
                .await
                .unwrap(),
                TerminalAbortManifestLoad::Unverifiable
            );
        }

        write_manifest_atomic(&path, &valid).await.unwrap();
        assert!(matches!(
            load_terminal_abort_manifest(
                &path,
                identity.provider,
                identity.uploader_identity_sha256
            )
            .await
            .unwrap(),
            TerminalAbortManifestLoad::Verified(TerminalAbortManifest {
                object_key,
                upload_id,
            }) if object_key == "objects/archive.bin" && upload_id == "upload-1"
        ));
    }

    #[tokio::test]
    async fn remove_manifest_is_idempotent_and_removes_only_the_exact_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("archive.json");
        let sibling = temp.path().join("archive.json.tmp-sibling");
        let directory = temp.path().join("archive.json.parts");
        tokio::fs::write(&path, b"manifest").await.unwrap();
        tokio::fs::write(&sibling, b"sibling").await.unwrap();
        tokio::fs::create_dir(&directory).await.unwrap();

        remove_manifest(&path).await.unwrap();
        remove_manifest(&path).await.unwrap();
        assert!(!path.exists());
        assert!(sibling.is_file());
        assert!(directory.is_dir());
    }

    #[test]
    fn new_manifest_rejects_invalid_session_values() {
        let identity = standard_identity();
        assert!(new_manifest(&identity, String::new(), "upload-1".to_owned()).is_err());
        assert!(new_manifest(&identity, "objects/archive.bin".to_owned(), String::new()).is_err());

        let entries = ["entry.jpg".to_owned()];
        let requested_entries_sha256 = requested_entries_fingerprint(&entries);
        let zip_identity = ManifestIdentity {
            requested_entries_sha256: Some(&requested_entries_sha256),
            ..identity
        };
        assert!(new_manifest(
            &zip_identity,
            "objects/archive.bin".to_owned(),
            "upload-1".to_owned(),
        )
        .is_err());
    }

    fn standard_identity() -> ManifestIdentity<'static> {
        ManifestIdentity {
            provider: ProviderKind::S3,
            uploader_identity_sha256: HASH_A,
            logical_object_id: "gallery-123",
            object_sha256: HASH_A,
            object_len: 99,
            content_type: "image/jpeg",
            requested_entries_sha256: None,
        }
    }

    async fn assert_stale(path: &Path, identity: &ManifestIdentity<'_>, reason: ManifestMismatch) {
        assert!(matches!(
            load_manifest(path, identity).await.unwrap(),
            ManifestLoad::Stale { reason: actual, .. } if actual == reason
        ));
    }
}
