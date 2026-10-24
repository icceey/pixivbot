use super::{is_safe_session_value, ProviderKind, PART_SIZE};
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

pub(super) struct TerminalAbortManifest {
    pub(super) object_key: String,
    pub(super) upload_id: String,
}

pub(super) enum TerminalAbortManifestLoad {
    Verified(TerminalAbortManifest),
    Missing,
    MalformedJson,
    Unverifiable(&'static str),
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
) -> Result<TerminalAbortManifestLoad, crate::UploadStateError> {
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(TerminalAbortManifestLoad::Missing);
        }
        Err(error) => return Err(crate::UploadStateError::Io(error.kind())),
    };
    let manifest: MultipartManifest = match serde_json::from_slice(&bytes) {
        Ok(manifest) => manifest,
        Err(_) => return Ok(TerminalAbortManifestLoad::MalformedJson),
    };
    let mismatch = if manifest.version != MANIFEST_VERSION {
        Some("multipart manifest version is unsupported")
    } else if validate_stored_values(&manifest).is_err() {
        Some("multipart manifest contains invalid stored values")
    } else if manifest.provider != provider {
        Some("multipart manifest provider does not match configured uploader")
    } else if manifest.uploader_identity_sha256 != uploader_identity_sha256 {
        Some("multipart manifest uploader identity does not match configured uploader")
    } else {
        None
    };
    if let Some(reason) = mismatch {
        return Ok(TerminalAbortManifestLoad::Unverifiable(reason));
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
    if !is_safe_session_value(&object_key) || !is_safe_session_value(&upload_id) {
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
        .any(|value| !is_safe_session_value(value))
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
        .any(|value| !is_safe_session_value(value))
        || !is_lowercase_sha256(&manifest.uploader_identity_sha256)
        || !is_lowercase_sha256(&manifest.object_sha256)
        || manifest
            .requested_entries_sha256
            .as_deref()
            .is_some_and(|value| !is_lowercase_sha256(value))
        || manifest
            .zip_extraction_prefix
            .as_deref()
            .is_some_and(|value| !is_safe_session_value(value))
    {
        return Err(ManifestMismatch::InvalidStoredValue);
    }
    Ok(())
}

fn is_lowercase_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use crate::s3_multipart::fingerprint_fields;

    #[test]
    fn fingerprints_disambiguate_field_boundaries_and_requested_entry_order() {
        let split_one = fingerprint_fields(&["ab", "c"]);
        let split_two = fingerprint_fields(&["a", "bc"]);
        assert_ne!(split_one, split_two);
        assert_ne!(
            fingerprint_fields(&["a".to_owned(), "bc".to_owned()]),
            fingerprint_fields(&["bc".to_owned(), "a".to_owned()])
        );
    }
}
