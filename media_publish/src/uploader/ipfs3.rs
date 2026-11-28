use super::s3::{
    default_multipart_image_threshold_mb, default_multipart_part_size_mb, default_s3_path_style,
    default_s3_request_timeout_sec, deserialize_multipart_part_size_mb, image_uses_multipart,
    multipart_part_size_bytes, parse_complete_multipart_upload_result,
    validate_complete_multipart_target,
};
use super::s3_multipart::{
    abort_upload_state as abort_multipart_upload_state, fingerprint_fields, put_object_stream,
    retry_put_object, sha256_hex, upload_multipart, zip_put_extension_is_explicitly_unsupported,
    CapabilityState, CompletionEvidence, CreateExtension, HeadRecovery, MultipartCapability,
    MultipartOperation, MultipartOutcome, MultipartUploadRequest, ProviderKind,
    IPFS3_DECOMPRESS_TAGGING,
};
use super::{
    detect_content_type, image_object_key, multipart_uploader_identity, required_config,
    validate_http_url, ImageUploadInput, ImageUploader, ZipArchiveUploadInput,
};
use crate::error::{Error, Result, UploadStateError};
use crate::telegraph::TelegraphImageUrlPair;
use async_trait::async_trait;
use reqwest::header::{HeaderName, HeaderValue};
use s3::creds::Credentials;
use s3::{Bucket, Region};
use serde::Deserialize;
use std::num::NonZeroU64;
use std::time::Duration;

#[derive(Debug, Clone, Deserialize)]
pub struct IpfS3UploaderConfig {
    #[serde(default)]
    pub endpoint_url: Option<String>,
    #[serde(default)]
    pub bucket: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub access_key_id: Option<String>,
    #[serde(default)]
    pub secret_access_key: Option<String>,
    #[serde(default)]
    pub gateway_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_gateway_url: Option<String>,
    #[serde(default = "default_ipfs_preview_rewrite_delay_sec")]
    pub preview_rewrite_delay_sec: u64,
    #[serde(default)]
    pub key_prefix: String,
    #[serde(default = "default_s3_path_style")]
    pub path_style: bool,
    #[serde(default = "default_s3_request_timeout_sec")]
    pub request_timeout_sec: NonZeroU64,
    #[serde(default = "default_multipart_image_threshold_mb")]
    pub multipart_image_threshold_mb: u64,
    #[serde(
        default = "default_multipart_part_size_mb",
        deserialize_with = "deserialize_multipart_part_size_mb"
    )]
    pub multipart_part_size_mb: u64,
    #[serde(default)]
    pub warm_public_gateway_after_upload: bool,
    #[serde(default = "default_ipfs3_zip_extract_enabled")]
    pub zip_extract_enabled: bool,
}

pub(super) fn default_ipfs_preview_rewrite_delay_sec() -> u64 {
    600
}

fn default_ipfs3_zip_extract_enabled() -> bool {
    true
}

impl Default for IpfS3UploaderConfig {
    fn default() -> Self {
        Self {
            endpoint_url: None,
            bucket: None,
            region: None,
            access_key_id: None,
            secret_access_key: None,
            gateway_url: None,
            preview_gateway_url: None,
            preview_rewrite_delay_sec: default_ipfs_preview_rewrite_delay_sec(),
            key_prefix: String::new(),
            path_style: default_s3_path_style(),
            request_timeout_sec: default_s3_request_timeout_sec(),
            multipart_image_threshold_mb: default_multipart_image_threshold_mb(),
            multipart_part_size_mb: default_multipart_part_size_mb(),
            warm_public_gateway_after_upload: false,
            zip_extract_enabled: default_ipfs3_zip_extract_enabled(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpfS3PreviewRewriteConfig {
    pub preview_gateway_url: String,
    pub public_gateway_url: String,
    pub delay_sec: u64,
}

impl IpfS3UploaderConfig {
    pub(super) fn required(&self) -> Result<ResolvedIpfS3UploaderConfig> {
        Ok(ResolvedIpfS3UploaderConfig {
            endpoint_url: validate_http_url(
                "image_upload.ipfs3.endpoint_url",
                &required_config("image_upload.ipfs3.endpoint_url", &self.endpoint_url)?,
            )?,
            bucket: required_config("image_upload.ipfs3.bucket", &self.bucket)?,
            region: required_config("image_upload.ipfs3.region", &self.region)?,
            access_key_id: required_config(
                "image_upload.ipfs3.access_key_id",
                &self.access_key_id,
            )?,
            secret_access_key: required_config(
                "image_upload.ipfs3.secret_access_key",
                &self.secret_access_key,
            )?,
            gateway_url: validate_http_url(
                "image_upload.ipfs3.gateway_url",
                &required_config("image_upload.ipfs3.gateway_url", &self.gateway_url)?,
            )?
            .trim_end_matches('/')
            .to_string(),
            preview_gateway_url: self
                .preview_gateway_url
                .as_ref()
                .map(|url| validate_http_url("image_upload.ipfs3.preview_gateway_url", url))
                .transpose()?
                .map(|url| url.trim_end_matches('/').to_string()),
            preview_rewrite_delay_sec: self.preview_rewrite_delay_sec,
            key_prefix: self.key_prefix.trim_matches('/').to_string(),
            path_style: self.path_style,
            request_timeout: Duration::from_secs(self.request_timeout_sec.get()),
            multipart_image_threshold_mb: self.multipart_image_threshold_mb,
            multipart_part_size: multipart_part_size_bytes(self.multipart_part_size_mb)?,
            warm_public_gateway_after_upload: self.warm_public_gateway_after_upload,
            zip_extract_enabled: self.zip_extract_enabled,
        })
    }
}

#[derive(Debug, Clone)]
pub(super) struct ResolvedIpfS3UploaderConfig {
    endpoint_url: String,
    bucket: String,
    region: String,
    access_key_id: String,
    secret_access_key: String,
    pub(super) gateway_url: String,
    pub(super) preview_gateway_url: Option<String>,
    pub(super) preview_rewrite_delay_sec: u64,
    key_prefix: String,
    path_style: bool,
    request_timeout: Duration,
    multipart_image_threshold_mb: u64,
    multipart_part_size: u64,
    warm_public_gateway_after_upload: bool,
    zip_extract_enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IpfS3ZipMultipartCompleteRoot {
    DecompressZip,
    Standard,
}

fn ipfs3_zip_multipart_complete_root(body: &[u8]) -> Result<IpfS3ZipMultipartCompleteRoot> {
    if body.iter().all(|byte| byte.is_ascii_whitespace()) {
        return Err(Error::Other(
            "invalid ipfS3 ZIP multipart Complete XML: empty response body".into(),
        ));
    }

    let mut reader = quick_xml::Reader::from_reader(body);
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Start(element))
            | Ok(quick_xml::events::Event::Empty(element)) => match element.name().as_ref() {
                "DecompressZipResult" => return Ok(IpfS3ZipMultipartCompleteRoot::DecompressZip),
                "CompleteMultipartUploadResult" => {
                    return Ok(IpfS3ZipMultipartCompleteRoot::Standard)
                }
                _ => {
                    return Err(Error::Other(
                        "invalid ipfS3 ZIP multipart Complete XML: unexpected root element".into(),
                    ));
                }
            },
            Ok(
                quick_xml::events::Event::Decl(_)
                | quick_xml::events::Event::Comment(_)
                | quick_xml::events::Event::PI(_)
                | quick_xml::events::Event::DocType(_),
            ) => {}
            Ok(quick_xml::events::Event::Text(text))
                if text.as_bytes().iter().all(u8::is_ascii_whitespace) => {}
            Ok(quick_xml::events::Event::Eof) | Ok(_) => {
                return Err(Error::Other(
                    "invalid ipfS3 ZIP multipart Complete XML: expected a result root element"
                        .into(),
                ));
            }
            Err(error) => {
                return Err(Error::Other(format!(
                    "invalid ipfS3 ZIP multipart Complete XML: {error}"
                )));
            }
        }
    }
}

pub(super) fn ipfs3_zip_extraction_prefix(object_key: &str) -> Result<String> {
    let archive_stem = object_key.strip_suffix(".zip").ok_or_else(|| {
        Error::Other(format!(
            "ipfS3 ZIP object key {object_key} does not end in .zip"
        ))
    })?;
    Ok(format!("{archive_stem}/"))
}

pub(super) fn ipfs3_cid_from_etag(etag: &str) -> Option<String> {
    let cid = etag.trim().trim_matches('"').trim();
    (!cid.is_empty()).then(|| cid.to_string())
}

pub struct IpfS3Uploader {
    pub(super) bucket: Box<Bucket>,
    config: ResolvedIpfS3UploaderConfig,
    gateway_http: reqwest::Client,
    multipart_http: reqwest::Client,
    pub(super) standard_multipart: MultipartCapability,
    pub(super) multipart_zip_extract: MultipartCapability,
    pub(super) single_put_zip_extract: MultipartCapability,
    uploader_identity_sha256: String,
}

pub(super) const ZIP_CENTRAL_HEADER_SIGNATURE: [u8; 4] = [0x50, 0x4b, 0x01, 0x02];
pub(super) const ZIP_CENTRAL_FIXED_HEADER_LEN: usize = 46;
pub(super) const ZIP_CENTRAL_FLAGS_OFFSET: usize = 8;
pub(super) const ZIP_CENTRAL_METHOD_OFFSET: usize = 10;
pub(super) const ZIP_CENTRAL_NAME_LEN_OFFSET: usize = 28;
pub(super) const ZIP_CENTRAL_EXTRA_LEN_OFFSET: usize = 30;
pub(super) const ZIP_CENTRAL_COMMENT_LEN_OFFSET: usize = 32;
const ZIP_LOCAL_HEADER_SIGNATURE: [u8; 4] = [0x50, 0x4b, 0x03, 0x04];
pub(super) const ZIP_LOCAL_FIXED_HEADER_LEN: usize = 30;
const ZIP_LOCAL_FLAGS_OFFSET: usize = 6;
const ZIP_LOCAL_METHOD_OFFSET: usize = 8;
const ZIP_LOCAL_CRC32_OFFSET: usize = 14;
pub(super) const ZIP_LOCAL_COMPRESSED_SIZE_OFFSET: usize = 18;
const ZIP_LOCAL_UNCOMPRESSED_SIZE_OFFSET: usize = 22;
const ZIP_LOCAL_NAME_LEN_OFFSET: usize = 26;
const ZIP_LOCAL_EXTRA_LEN_OFFSET: usize = 28;
pub(super) const ZIP_DATA_DESCRIPTOR_SIGNATURE: [u8; 4] = [0x50, 0x4b, 0x07, 0x08];
const ZIP_FLAG_ENCRYPTED: u16 = 1;
pub(super) const ZIP_FLAG_DATA_DESCRIPTOR: u16 = 1 << 3;
const ZIP_FLAG_UTF8: u16 = 1 << 11;
const ZIP_STREAM_RELEVANT_FLAGS: u16 =
    ZIP_FLAG_ENCRYPTED | ZIP_FLAG_DATA_DESCRIPTOR | ZIP_FLAG_UTF8;

struct IpfS3ZipCentralDirectoryEntry {
    flags: u16,
    method: u16,
    raw_name: Vec<u8>,
    header_start: u64,
}

struct IpfS3ZipArchiveEntry {
    central: IpfS3ZipCentralDirectoryEntry,
    crc32: u32,
    compressed_size: u64,
    uncompressed_size: u64,
}

struct IpfS3ZipLocalHeader {
    flags: u16,
    method: u16,
    crc32: u32,
    compressed_size: u32,
    uncompressed_size: u32,
    raw_name: Vec<u8>,
    extra: Vec<u8>,
    data_start: u64,
}

fn ipfs3_zip_u16_at(bytes: &[u8], offset: usize) -> Option<u16> {
    let end = offset.checked_add(2)?;
    let value: [u8; 2] = bytes.get(offset..end)?.try_into().ok()?;
    Some(u16::from_le_bytes(value))
}

fn ipfs3_zip_u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    let value: [u8; 4] = bytes.get(offset..end)?.try_into().ok()?;
    Some(u32::from_le_bytes(value))
}

fn ipfs3_zip_path_is_safe(value: &str, allow_empty: bool) -> bool {
    if value.is_empty() {
        return allow_empty;
    }
    let bytes = value.as_bytes();
    let has_windows_drive = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    if value.contains('\\') || value.starts_with('/') || has_windows_drive {
        return false;
    }
    if value
        .split('/')
        .any(|segment| segment == "." || segment == "..")
    {
        return false;
    }
    !value.trim_matches('/').is_empty()
}

fn ipfs3_zip_central_directory_entries<R: std::io::Read + std::io::Seek>(
    reader: &mut R,
    central_directory_start: u64,
    archive_len: usize,
) -> Option<Vec<IpfS3ZipCentralDirectoryEntry>> {
    let archive_size = reader.seek(std::io::SeekFrom::End(0)).ok()?;
    let mut offset = central_directory_start;
    let mut entries = Vec::new();
    let mut raw_names = std::collections::HashSet::new();

    loop {
        reader.seek(std::io::SeekFrom::Start(offset)).ok()?;
        let mut header = [0; ZIP_CENTRAL_FIXED_HEADER_LEN];
        reader.read_exact(&mut header[..4]).ok()?;
        if header[..4] != ZIP_CENTRAL_HEADER_SIGNATURE {
            break;
        }
        reader.read_exact(&mut header[4..]).ok()?;
        let fixed_end = offset.checked_add(ZIP_CENTRAL_FIXED_HEADER_LEN as u64)?;
        let name_len = ipfs3_zip_u16_at(&header, ZIP_CENTRAL_NAME_LEN_OFFSET)?;
        let extra_len = ipfs3_zip_u16_at(&header, ZIP_CENTRAL_EXTRA_LEN_OFFSET)?;
        let comment_len = ipfs3_zip_u16_at(&header, ZIP_CENTRAL_COMMENT_LEN_OFFSET)?;
        let variable_len = u64::from(name_len) + u64::from(extra_len) + u64::from(comment_len);
        let record_end = fixed_end.checked_add(variable_len)?;
        if record_end > archive_size {
            return None;
        }
        let mut raw_name = vec![0; usize::from(name_len)];
        reader.read_exact(&mut raw_name).ok()?;
        if !raw_names.insert(raw_name.clone()) {
            return None;
        }
        entries.push(IpfS3ZipCentralDirectoryEntry {
            flags: ipfs3_zip_u16_at(&header, ZIP_CENTRAL_FLAGS_OFFSET)?,
            method: ipfs3_zip_u16_at(&header, ZIP_CENTRAL_METHOD_OFFSET)?,
            raw_name,
            header_start: u64::from(ipfs3_zip_u32_at(&header, 42)?),
        });
        offset = record_end;
    }

    (entries.len() == archive_len).then_some(entries)
}

fn ipfs3_zip_local_header<R: std::io::Read + std::io::Seek>(
    reader: &mut R,
    header_start: u64,
) -> Option<IpfS3ZipLocalHeader> {
    reader.seek(std::io::SeekFrom::Start(header_start)).ok()?;
    let mut header = [0; ZIP_LOCAL_FIXED_HEADER_LEN];
    reader.read_exact(&mut header).ok()?;
    if header[..ZIP_LOCAL_HEADER_SIGNATURE.len()] != ZIP_LOCAL_HEADER_SIGNATURE {
        return None;
    }
    let flags = ipfs3_zip_u16_at(&header, ZIP_LOCAL_FLAGS_OFFSET)?;
    let method = ipfs3_zip_u16_at(&header, ZIP_LOCAL_METHOD_OFFSET)?;
    let crc32 = ipfs3_zip_u32_at(&header, ZIP_LOCAL_CRC32_OFFSET)?;
    let compressed_size = ipfs3_zip_u32_at(&header, ZIP_LOCAL_COMPRESSED_SIZE_OFFSET)?;
    let uncompressed_size = ipfs3_zip_u32_at(&header, ZIP_LOCAL_UNCOMPRESSED_SIZE_OFFSET)?;
    let name_len = ipfs3_zip_u16_at(&header, ZIP_LOCAL_NAME_LEN_OFFSET)?;
    let extra_len = ipfs3_zip_u16_at(&header, ZIP_LOCAL_EXTRA_LEN_OFFSET)?;
    let data_start = header_start
        .checked_add(ZIP_LOCAL_FIXED_HEADER_LEN as u64)?
        .checked_add(u64::from(name_len))?
        .checked_add(u64::from(extra_len))?;
    let mut raw_name = vec![0; usize::from(name_len)];
    let mut extra = vec![0; usize::from(extra_len)];
    reader.read_exact(&mut raw_name).ok()?;
    reader.read_exact(&mut extra).ok()?;
    Some(IpfS3ZipLocalHeader {
        flags,
        method,
        crc32,
        compressed_size,
        uncompressed_size,
        raw_name,
        extra,
        data_start,
    })
}

fn ipfs3_zip_data_descriptor_record_end<R: std::io::Read + std::io::Seek>(
    reader: &mut R,
    descriptor_start: u64,
    entry: &IpfS3ZipArchiveEntry,
) -> Option<u64> {
    reader
        .seek(std::io::SeekFrom::Start(descriptor_start))
        .ok()?;
    let mut payload = [0; 12];
    reader.read_exact(&mut payload[..4]).ok()?;
    let has_signature = payload[..4] == ZIP_DATA_DESCRIPTOR_SIGNATURE;
    if has_signature {
        reader.read_exact(&mut payload).ok()?;
    } else {
        reader.read_exact(&mut payload[4..]).ok()?;
    }
    if ipfs3_zip_u32_at(&payload, 0)? != entry.crc32
        || u64::from(ipfs3_zip_u32_at(&payload, 4)?) != entry.compressed_size
        || u64::from(ipfs3_zip_u32_at(&payload, 8)?) != entry.uncompressed_size
    {
        return None;
    }
    descriptor_start.checked_add(if has_signature { 16 } else { 12 })
}

fn ipfs3_zip_deflate_stream_is_compatible<R: std::io::Read>(
    reader: &mut R,
    compressed_size: u64,
) -> bool {
    use flate2::{Decompress, FlushDecompress, Status};

    let mut decompressor = Decompress::new(false);
    let mut input = [0; 8 * 1024];
    let mut output = [0; 8 * 1024];
    let mut remaining = compressed_size;
    let mut input_start = 0;
    let mut input_end = 0;

    loop {
        if input_start == input_end && remaining != 0 {
            input_end = remaining.min(input.len() as u64) as usize;
            if reader.read_exact(&mut input[..input_end]).is_err() {
                return false;
            }
            remaining -= input_end as u64;
            input_start = 0;
        }
        let total_in_before = decompressor.total_in();
        let total_out_before = decompressor.total_out();
        let Ok(status) = decompressor.decompress(
            &input[input_start..input_end],
            &mut output,
            FlushDecompress::None,
        ) else {
            return false;
        };
        input_start += (decompressor.total_in() - total_in_before) as usize;

        if status == Status::StreamEnd {
            return decompressor.total_in() == compressed_size;
        }
        if decompressor.total_in() == total_in_before
            && decompressor.total_out() == total_out_before
        {
            return false;
        }
    }
}

fn ipfs3_zip_local_extra_is_compatible(extra: &[u8]) -> bool {
    let mut offset = 0usize;
    while offset < extra.len() {
        let Some(header_end) = offset.checked_add(4) else {
            return false;
        };
        let Some(header) = extra.get(offset..header_end) else {
            return false;
        };
        let Some(field_id) = ipfs3_zip_u16_at(header, 0) else {
            return false;
        };
        let Some(field_len) = ipfs3_zip_u16_at(header, 2) else {
            return false;
        };
        let Some(field_end) = header_end.checked_add(usize::from(field_len)) else {
            return false;
        };
        let Some(field_data) = extra.get(header_end..field_end) else {
            return false;
        };
        if field_id == 0x7075
            || (field_id == 0x6375
                && (field_data.is_empty() || (field_data[0] == 1 && field_data.len() < 5)))
        {
            return false;
        }
        offset = field_end;
    }
    true
}

pub(super) fn ipfs3_zip_archive_is_compatible<R: std::io::Read + std::io::Seek>(
    reader: &mut R,
    requested_entry_names: &[String],
    extraction_prefix: &str,
) -> bool {
    let mut requested = std::collections::HashSet::new();
    if requested_entry_names
        .iter()
        .any(|name| !requested.insert(name.as_str()))
    {
        return false;
    }
    if !ipfs3_zip_path_is_safe(extraction_prefix, true) {
        return false;
    }

    let Ok(mut archive) = zip::ZipArchive::new(&mut *reader) else {
        return false;
    };
    let central_directory_start = archive.central_directory_start();
    let archive_len = archive.len();
    let mut metadata = Vec::with_capacity(archive_len);
    for index in 0..archive_len {
        let Ok(file) = archive.by_index_raw(index) else {
            return false;
        };
        metadata.push((
            file.name_raw().to_vec(),
            file.crc32(),
            file.compressed_size(),
            file.size(),
        ));
    }
    let reader = archive.into_inner();
    let Some(central_entries) =
        ipfs3_zip_central_directory_entries(reader, central_directory_start, archive_len)
    else {
        return false;
    };
    let mut entries = Vec::with_capacity(archive_len);
    for (central, (name, crc32, compressed_size, uncompressed_size)) in
        central_entries.into_iter().zip(metadata)
    {
        if name != central.raw_name {
            return false;
        }
        entries.push(IpfS3ZipArchiveEntry {
            central,
            crc32,
            compressed_size,
            uncompressed_size,
        });
    }
    entries.sort_unstable_by_key(|entry| entry.central.header_start);

    if entries
        .first()
        .is_none_or(|entry| entry.central.header_start != 0)
    {
        return false;
    }
    for (index, entry) in entries.iter().enumerate() {
        let Ok(raw_name) = std::str::from_utf8(&entry.central.raw_name) else {
            return false;
        };
        if !ipfs3_zip_path_is_safe(raw_name, false) || entry.central.flags & ZIP_FLAG_ENCRYPTED != 0
        {
            return false;
        }

        if entry.central.method != 0 && entry.central.method != 8 {
            return false;
        }
        let Some(local) = ipfs3_zip_local_header(reader, entry.central.header_start) else {
            return false;
        };
        let Ok(local_name) = std::str::from_utf8(&local.raw_name) else {
            return false;
        };
        let non_ascii_local_name = local.raw_name.iter().any(|byte| !byte.is_ascii());
        let local_uses_descriptor = local.flags & ZIP_FLAG_DATA_DESCRIPTOR != 0;
        if !ipfs3_zip_path_is_safe(local_name, false)
            || local.raw_name != entry.central.raw_name
            || local.flags & ZIP_FLAG_ENCRYPTED != 0
            || local.flags & ZIP_STREAM_RELEVANT_FLAGS
                != entry.central.flags & ZIP_STREAM_RELEVANT_FLAGS
            || local.method != entry.central.method
            || !ipfs3_zip_local_extra_is_compatible(&local.extra)
            || (non_ascii_local_name && local.flags & ZIP_FLAG_UTF8 == 0)
            || (local_uses_descriptor && local.method != 8)
        {
            return false;
        }

        let Some(data_end) = local.data_start.checked_add(entry.compressed_size) else {
            return false;
        };
        if entry.central.method == 8
            && !ipfs3_zip_deflate_stream_is_compatible(reader, entry.compressed_size)
        {
            return false;
        }
        let local_record_end = if local_uses_descriptor {
            let Some(record_end) = ipfs3_zip_data_descriptor_record_end(reader, data_end, entry)
            else {
                return false;
            };
            record_end
        } else {
            if local.crc32 != entry.crc32
                || u64::from(local.compressed_size) != entry.compressed_size
                || u64::from(local.uncompressed_size) != entry.uncompressed_size
            {
                return false;
            }
            data_end
        };
        let expected_next_start = match entries.get(index + 1) {
            Some(next) => next.central.header_start,
            None => central_directory_start,
        };
        if local_record_end != expected_next_start {
            return false;
        }
    }
    true
}

struct PreparedZipArchive {
    file: tokio::fs::File,
    len: u64,
    sha256: String,
}

fn prepare_zip_archive(
    path: &std::path::Path,
    entry_names: &[String],
    key_prefix: &str,
) -> Result<Option<(PreparedZipArchive, String)>> {
    use sha2::{Digest, Sha256};
    use std::io::{Read, Seek};

    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let mut digest = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    let mut read_len = 0_u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
        read_len += read as u64;
    }
    if read_len != len {
        return Err(Error::Other(
            "ZIP archive length changed during preflight".into(),
        ));
    }
    let sha256 = hex::encode(digest.finalize());
    let timestamp = chrono::Utc::now().format("%Y%m%d%H%M%S");
    let filename = format!("{timestamp}-archive-{}.zip", &sha256[..8]);
    let key = if key_prefix.is_empty() {
        filename
    } else {
        format!("{key_prefix}/{filename}")
    };
    let extraction_prefix = ipfs3_zip_extraction_prefix(&key)?;
    file.rewind()?;
    if !ipfs3_zip_archive_is_compatible(&mut file, entry_names, &extraction_prefix) {
        return Ok(None);
    }
    file.rewind()?;
    Ok(Some((
        PreparedZipArchive {
            file: tokio::fs::File::from_std(file),
            len,
            sha256,
        },
        key,
    )))
}

impl IpfS3Uploader {
    pub fn from_config(config: &IpfS3UploaderConfig) -> Result<Self> {
        let config = config.required()?;
        let credentials = Credentials::new(
            Some(&config.access_key_id),
            Some(&config.secret_access_key),
            None,
            None,
            None,
        )
        .map_err(|e| Error::Other(format!("invalid ipfS3 credentials: {e}")))?;
        let region = Region::Custom {
            region: config.region.clone(),
            endpoint: config.endpoint_url.clone(),
        };
        let mut bucket = Bucket::new(&config.bucket, region, credentials)
            .and_then(|bucket| bucket.with_request_timeout(config.request_timeout))
            .map_err(|e| Error::Other(format!("failed to build ipfS3 bucket client: {e}")))?;
        if config.path_style {
            bucket = bucket.with_path_style();
        }
        let gateway_http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()?;
        let multipart_http = reqwest::Client::builder()
            .timeout(config.request_timeout)
            .build()?;
        let uploader_identity_sha256 = multipart_uploader_identity(
            ProviderKind::IpfS3,
            &config.endpoint_url,
            &config.region,
            &config.bucket,
            config.path_style,
        )?;
        Ok(Self {
            bucket,
            config,
            gateway_http,
            multipart_http,
            standard_multipart: MultipartCapability::default(),
            multipart_zip_extract: MultipartCapability::default(),
            single_put_zip_extract: MultipartCapability::default(),
            uploader_identity_sha256,
        })
    }

    fn warm_public_gateway(&self, cid: &str) {
        if !self.config.warm_public_gateway_after_upload {
            return;
        }

        let url = format!("{}/{}", self.config.gateway_url, cid);
        let gateway_http = self.gateway_http.clone();
        tokio::spawn(async move {
            match gateway_http.head(&url).send().await {
                Ok(resp) if !resp.status().is_success() => {
                    tracing::debug!("IPFS gateway warmup returned {} for {}", resp.status(), url);
                }
                Ok(_) => {}
                Err(e) => tracing::debug!("IPFS gateway warmup failed for {}: {}", url, e),
            }
        });
    }

    fn url_pair_for_cid(&self, cid: &str) -> TelegraphImageUrlPair {
        let public_url = format!("{}/{cid}", self.config.gateway_url);
        let preview_url = self
            .config
            .preview_gateway_url
            .as_ref()
            .map(|gateway| format!("{gateway}/{cid}"))
            .unwrap_or_else(|| public_url.clone());
        TelegraphImageUrlPair {
            preview_url,
            public_url,
            cid: Some(cid.to_owned()),
        }
    }

    async fn upload_image_single_put(
        &self,
        key: &str,
        bytes: &[u8],
        content_type: &str,
    ) -> Result<String> {
        let response = retry_put_object(|| {
            self.bucket
                .put_object_with_content_type(key, bytes, content_type)
        })
        .await
        .map_err(|e| Error::Other(format!("ipfS3 put_object failed for key {key}: {e}")))?;
        let status = response.status_code();
        if !(200..300).contains(&status) {
            return Err(Error::Api {
                message: format!("ipfS3 put_object returned {status} for key {key}"),
                status,
            });
        }
        let etag = response.as_str().map_err(|e| {
            Error::Other(format!(
                "ipfS3 put_object for key {key} returned non-UTF-8 ETag: {e}"
            ))
        })?;
        ipfs3_cid_from_etag(etag).ok_or_else(|| {
            Error::Other(format!(
                "ipfS3 put_object for key {key} returned no ETag (CID); \
                 cannot build public URL"
            ))
        })
    }

    fn cid_from_multipart_completion(
        &self,
        object_key: &str,
        evidence: &CompletionEvidence,
    ) -> Result<String> {
        let etag = match evidence {
            CompletionEvidence::Response(response) => {
                let result = parse_complete_multipart_upload_result(response.bytes())?;
                validate_complete_multipart_target(&result, &self.config.bucket, object_key)?;
                result.etag
            }
            CompletionEvidence::RecoveredHead { etag: Some(etag) } => etag.clone(),
            CompletionEvidence::RecoveredHead { etag: None } => {
                return Err(Error::Other(
                    "ipfS3 multipart recovery returned no ETag (CID) evidence".into(),
                ));
            }
        };
        ipfs3_cid_from_etag(&etag).ok_or_else(|| {
            Error::Other("ipfS3 multipart Complete response had no ETag (CID)".into())
        })
    }

    pub async fn upload_images_with_url_pairs(
        &self,
        images: &[ImageUploadInput<'_>],
    ) -> Result<Vec<TelegraphImageUrlPair>> {
        let mut urls = Vec::with_capacity(images.len());
        for (index, image) in images.iter().enumerate() {
            let key = image_object_key(&self.config.key_prefix, index + 1, image);
            let content_type = detect_content_type(image.bytes);
            if !image_uses_multipart(image.bytes.len(), self.config.multipart_image_threshold_mb)
                || self.standard_multipart.state() == CapabilityState::Unsupported
            {
                let cid = self
                    .upload_image_single_put(&key, image.bytes, &content_type)
                    .await?;
                self.warm_public_gateway(&cid);
                urls.push(self.url_pair_for_cid(&cid));
                continue;
            }

            let (manifest_path, logical_object_id) = match image.resume_context {
                Some(context) => (Some(context.manifest_path), context.logical_object_id),
                None => (None, key.as_str()),
            };
            match upload_multipart(
                self.bucket.as_ref(),
                &self.multipart_http,
                MultipartUploadRequest {
                    provider: ProviderKind::IpfS3,
                    uploader_identity_sha256: &self.uploader_identity_sha256,
                    candidate_object_key: key.clone(),
                    logical_object_id,
                    reader: Box::new(std::io::Cursor::new(image.bytes)),
                    object_len: image.bytes.len() as u64,
                    object_sha256: sha256_hex(image.bytes),
                    part_size: self.config.multipart_part_size,
                    content_type: &content_type,
                    manifest_path,
                    create_extension: CreateExtension::None,
                    head_recovery: HeadRecovery::IpfS3ImageByLengthAndEtag,
                },
            )
            .await?
            {
                MultipartOutcome::Unsupported { .. } => {
                    self.standard_multipart.mark_unsupported();
                    let cid = self
                        .upload_image_single_put(&key, image.bytes, &content_type)
                        .await?;
                    self.warm_public_gateway(&cid);
                    urls.push(self.url_pair_for_cid(&cid));
                }
                MultipartOutcome::Completed(completed) => {
                    let cid = self.cid_from_multipart_completion(
                        &completed.object_key,
                        &completed.evidence,
                    )?;
                    let pair = self.url_pair_for_cid(&cid);
                    self.standard_multipart.mark_supported();
                    completed.remove_manifest().await?;
                    self.warm_public_gateway(&cid);
                    urls.push(pair);
                }
            }
        }
        Ok(urls)
    }

    async fn upload_zip_archive_single_put(
        &self,
        key: &str,
        extraction_prefix: &str,
        archive: &ZipArchiveUploadInput<'_>,
        prepared: &PreparedZipArchive,
    ) -> Result<Option<Vec<TelegraphImageUrlPair>>> {
        let mut upload_bucket = self.bucket.clone();
        upload_bucket.add_query("decompress-zip", extraction_prefix);
        upload_bucket.extra_headers.insert(
            HeaderName::from_static("x-amz-tagging"),
            HeaderValue::from_static(IPFS3_DECOMPRESS_TAGGING),
        );

        let response = retry_put_object(|| {
            put_object_stream(
                upload_bucket.as_ref(),
                key,
                &prepared.file,
                prepared.len,
                &prepared.sha256,
                "application/zip",
            )
        })
        .await
        .map_err(|error| {
            Error::Other(format!(
                "ipfS3 ZIP put_object failed for key {key}: {error}"
            ))
        })?;
        let status = response.status_code();
        let body = response.bytes();
        if (200..300).contains(&status) && body.iter().all(|byte| byte.is_ascii_whitespace()) {
            self.single_put_zip_extract.mark_unsupported();
            return Ok(None);
        }
        if zip_put_extension_is_explicitly_unsupported(status, body) {
            self.single_put_zip_extract.mark_unsupported();
            return Ok(None);
        }
        if !(200..300).contains(&status) {
            return Err(Error::Api {
                message: format!("ipfS3 ZIP put_object returned {status} for key {key}"),
                status,
            });
        }

        let extract_result = parse_ipfs3_zip_extract_result(body).map_err(|error| {
            Error::Other(format!(
                "ipfS3 ZIP put_object for key {key} returned {error}"
            ))
        })?;
        let cids = ipfs3_zip_entry_cids(extraction_prefix, archive.entry_names, extract_result)?;
        self.single_put_zip_extract.mark_supported();
        let Some(cids) = cids else {
            return Ok(None);
        };
        let pairs = cids.iter().map(|cid| self.url_pair_for_cid(cid)).collect();
        for cid in &cids {
            self.warm_public_gateway(cid);
        }
        Ok(Some(pairs))
    }

    pub async fn upload_zip_archive_with_url_pairs(
        &self,
        archive: ZipArchiveUploadInput<'_>,
    ) -> Result<Option<Vec<TelegraphImageUrlPair>>> {
        if !self.config.zip_extract_enabled {
            return Ok(None);
        }
        if archive.entry_names.is_empty() {
            return Ok(Some(Vec::new()));
        }
        if self.single_put_zip_extract.state() == CapabilityState::Unsupported {
            return Ok(None);
        }

        let path = archive.path.to_path_buf();
        let entry_names = archive.entry_names.to_vec();
        let key_prefix = self.config.key_prefix.clone();
        let prepared = tokio::task::spawn_blocking(move || {
            prepare_zip_archive(&path, &entry_names, &key_prefix)
        })
        .await
        .map_err(|error| Error::Other(format!("ZIP preflight task failed: {error}")))??;
        let Some((mut prepared, key)) = prepared else {
            return Ok(None);
        };
        let extraction_prefix = ipfs3_zip_extraction_prefix(&key)?;
        if self.standard_multipart.state() == CapabilityState::Unsupported
            || self.multipart_zip_extract.state() == CapabilityState::Unsupported
        {
            return self
                .upload_zip_archive_single_put(&key, &extraction_prefix, &archive, &prepared)
                .await;
        }

        let requested_entries_sha256 = fingerprint_fields(archive.entry_names);
        let (manifest_path, logical_object_id) = match archive.resume_context {
            Some(context) => (Some(context.manifest_path), context.logical_object_id),
            None => (None, key.as_str()),
        };
        match upload_multipart(
            self.bucket.as_ref(),
            &self.multipart_http,
            MultipartUploadRequest {
                provider: ProviderKind::IpfS3,
                uploader_identity_sha256: &self.uploader_identity_sha256,
                candidate_object_key: key.clone(),
                logical_object_id,
                reader: Box::new(&mut prepared.file),
                object_len: prepared.len,
                object_sha256: prepared.sha256.clone(),
                part_size: self.config.multipart_part_size,
                content_type: "application/zip",
                manifest_path,
                create_extension: CreateExtension::IpfS3DecompressZip {
                    requested_entries_sha256,
                },
                head_recovery: HeadRecovery::Never,
            },
        )
        .await?
        {
            MultipartOutcome::Unsupported { operation } => {
                if operation == MultipartOperation::Create {
                    self.multipart_zip_extract.mark_unsupported();
                } else {
                    self.standard_multipart.mark_unsupported();
                }
                self.upload_zip_archive_single_put(&key, &extraction_prefix, &archive, &prepared)
                    .await
            }
            MultipartOutcome::Completed(completed) => match &completed.evidence {
                CompletionEvidence::Response(response) => {
                    match ipfs3_zip_multipart_complete_root(response.bytes())? {
                        IpfS3ZipMultipartCompleteRoot::DecompressZip => {
                            let result = parse_ipfs3_zip_extract_result(response.bytes())?;
                            let completed_prefix =
                                ipfs3_zip_extraction_prefix(&completed.object_key)?;
                            let cids = ipfs3_zip_entry_cids(
                                &completed_prefix,
                                archive.entry_names,
                                result,
                            )?;
                            self.standard_multipart.mark_supported();
                            self.multipart_zip_extract.mark_supported();
                            let pairs = cids.as_ref().map(|cids| {
                                cids.iter()
                                    .map(|cid| self.url_pair_for_cid(cid))
                                    .collect::<Vec<_>>()
                            });
                            completed.remove_manifest().await?;
                            if let Some(cids) = cids {
                                for cid in &cids {
                                    self.warm_public_gateway(cid);
                                }
                            }
                            Ok(pairs)
                        }
                        IpfS3ZipMultipartCompleteRoot::Standard => {
                            let result = parse_complete_multipart_upload_result(response.bytes())?;
                            validate_complete_multipart_target(
                                &result,
                                &self.config.bucket,
                                &completed.object_key,
                            )?;
                            let completed_prefix =
                                ipfs3_zip_extraction_prefix(&completed.object_key)?;
                            self.standard_multipart.mark_supported();
                            self.multipart_zip_extract.mark_unsupported();
                            completed.remove_manifest().await?;
                            self.upload_zip_archive_single_put(
                                &completed.object_key,
                                &completed_prefix,
                                &archive,
                                &prepared,
                            )
                            .await
                        }
                    }
                }
                CompletionEvidence::RecoveredHead { .. } => Err(Error::Other(
                    "ipfS3 ZIP multipart upload unexpectedly recovered completion by Head".into(),
                )),
            },
        }
    }
}

#[async_trait]
impl ImageUploader for IpfS3Uploader {
    fn supports_zip_archive_upload(&self) -> bool {
        self.config.zip_extract_enabled
    }

    async fn abort_upload_state(
        &self,
        uploads_dir: &std::path::Path,
    ) -> std::result::Result<(), UploadStateError> {
        abort_multipart_upload_state(
            self.bucket.as_ref(),
            ProviderKind::IpfS3,
            &self.uploader_identity_sha256,
            uploads_dir,
        )
        .await
    }

    async fn upload_images(&self, images: &[ImageUploadInput<'_>]) -> Result<Vec<String>> {
        Ok(IpfS3Uploader::upload_images_with_url_pairs(self, images)
            .await?
            .into_iter()
            .map(|pair| pair.preview_url)
            .collect())
    }

    async fn upload_images_with_url_pairs(
        &self,
        images: &[ImageUploadInput<'_>],
    ) -> Result<Vec<TelegraphImageUrlPair>> {
        IpfS3Uploader::upload_images_with_url_pairs(self, images).await
    }

    async fn upload_zip_archive_with_url_pairs(
        &self,
        archive: ZipArchiveUploadInput<'_>,
    ) -> Result<Option<Vec<TelegraphImageUrlPair>>> {
        IpfS3Uploader::upload_zip_archive_with_url_pairs(self, archive).await
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename = "DecompressZipResult")]
pub(super) struct IpfS3ZipExtractResult {
    #[serde(rename = "ArchiveKey")]
    _archive_key: String,
    #[serde(rename = "ArchiveETag")]
    _archive_etag: String,
    #[serde(rename = "ArchiveSize")]
    _archive_size: u64,
    #[serde(rename = "ExtractedCount")]
    extracted_count: usize,
    #[serde(rename = "FailedCount")]
    failed_count: usize,
    #[serde(rename = "Entries", default)]
    entries: IpfS3ZipExtractEntries,
    #[serde(rename = "Failures", default)]
    failures: IpfS3ZipExtractFailures,
}

#[derive(Debug, Default, Deserialize)]
struct IpfS3ZipExtractEntries {
    #[serde(rename = "Entry", default)]
    entries: Vec<IpfS3ZipExtractEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename = "Entry")]
struct IpfS3ZipExtractEntry {
    #[serde(rename = "Key")]
    key: String,
    #[serde(rename = "ETag")]
    etag: String,
    #[serde(rename = "Size")]
    _size: u64,
}

#[derive(Debug, Default, Deserialize)]
struct IpfS3ZipExtractFailures {
    #[serde(rename = "Failure", default)]
    failures: Vec<IpfS3ZipExtractFailure>,
}

#[derive(Debug, Deserialize)]
#[serde(rename = "Failure")]
struct IpfS3ZipExtractFailure {
    #[serde(rename = "EntryName")]
    entry_name: String,
    #[serde(rename = "Code")]
    _code: String,
    #[serde(rename = "Message")]
    _message: String,
}

fn validate_ipfs3_zip_extract_result_xml(body: &[u8]) -> Result<()> {
    const ROOT_ELEMENT: &str = "DecompressZipResult";

    let mut reader = quick_xml::Reader::from_reader(body);
    let mut buffer = Vec::new();
    let root_is_empty = loop {
        buffer.clear();
        match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Start(element)) => {
                if element.name().as_ref() != ROOT_ELEMENT {
                    return Err(Error::Other(
                        "invalid DecompressZipResult XML: expected DecompressZipResult root element"
                            .into(),
                    ));
                }
                break false;
            }
            Ok(quick_xml::events::Event::Empty(element)) => {
                if element.name().as_ref() != ROOT_ELEMENT {
                    return Err(Error::Other(
                        "invalid DecompressZipResult XML: expected DecompressZipResult root element"
                            .into(),
                    ));
                }
                break true;
            }
            Ok(
                quick_xml::events::Event::Decl(_)
                | quick_xml::events::Event::Comment(_)
                | quick_xml::events::Event::PI(_)
                | quick_xml::events::Event::DocType(_),
            ) => {}
            Ok(quick_xml::events::Event::Text(text))
                if text.as_bytes().iter().all(u8::is_ascii_whitespace) => {}
            Ok(quick_xml::events::Event::Eof) | Ok(_) => {
                return Err(Error::Other(
                    "invalid DecompressZipResult XML: expected DecompressZipResult root element"
                        .into(),
                ));
            }
            Err(error) => {
                return Err(Error::Other(format!(
                    "invalid DecompressZipResult XML: {error}"
                )));
            }
        }
    };

    if !root_is_empty {
        buffer.clear();
        reader
            .read_to_end(quick_xml::name::QName(ROOT_ELEMENT))
            .map_err(|error| Error::Other(format!("invalid DecompressZipResult XML: {error}")))?;
    }

    loop {
        buffer.clear();
        match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Eof) => return Ok(()),
            Ok(quick_xml::events::Event::Comment(_) | quick_xml::events::Event::PI(_)) => {}
            Ok(quick_xml::events::Event::Text(text))
                if text.as_bytes().iter().all(u8::is_ascii_whitespace) => {}
            Ok(_) => {
                return Err(Error::Other(
                    "invalid DecompressZipResult XML: trailing XML content after root element"
                        .into(),
                ));
            }
            Err(error) => {
                return Err(Error::Other(format!(
                    "invalid DecompressZipResult XML: {error}"
                )));
            }
        }
    }
}

pub(super) fn parse_ipfs3_zip_extract_result(body: &[u8]) -> Result<IpfS3ZipExtractResult> {
    if body.iter().all(|byte| byte.is_ascii_whitespace()) {
        return Err(Error::Other(
            "invalid DecompressZipResult XML: empty response body".into(),
        ));
    }

    validate_ipfs3_zip_extract_result_xml(body)?;
    quick_xml::de::from_reader(std::io::Cursor::new(body))
        .map_err(|e| Error::Other(format!("invalid DecompressZipResult XML: {e}")))
}

pub(super) fn ipfs3_zip_entry_cids(
    extraction_prefix: &str,
    entry_names: &[String],
    result: IpfS3ZipExtractResult,
) -> Result<Option<Vec<String>>> {
    if result.extracted_count != result.entries.entries.len() {
        return Err(Error::Other(format!(
            "ipfS3 ZIP extraction ExtractedCount {} does not match {} entries",
            result.extracted_count,
            result.entries.entries.len()
        )));
    }
    if result.failed_count != result.failures.failures.len() {
        return Err(Error::Other(format!(
            "ipfS3 ZIP extraction FailedCount {} does not match {} failures",
            result.failed_count,
            result.failures.failures.len()
        )));
    }
    let requested_names = entry_names
        .iter()
        .map(String::as_str)
        .collect::<std::collections::HashSet<_>>();
    if requested_names.len() != entry_names.len() {
        return Ok(None);
    }

    let mut entry_cids = std::collections::HashMap::new();
    for entry in result.entries.entries {
        let cid = entry.etag.trim().trim_matches('"').trim();
        if cid.is_empty() {
            let key_kind = if entry
                .key
                .strip_prefix(extraction_prefix)
                .is_some_and(|name| requested_names.contains(name))
            {
                "requested extraction key"
            } else {
                "extraction key"
            };
            return Err(Error::Other(format!(
                "ipfS3 ZIP extraction {key_kind} {} returned an empty CID",
                entry.key,
            )));
        }
        entry_cids.insert(entry.key, cid.to_string());
    }
    let mut cids = Vec::with_capacity(entry_names.len());

    for entry_name in entry_names {
        let key = format!("{extraction_prefix}{entry_name}");
        let Some(cid) = entry_cids.get(&key) else {
            return Ok(None);
        };
        cids.push(cid.clone());
    }

    if result
        .failures
        .failures
        .iter()
        .any(|failure| requested_names.contains(failure.entry_name.as_str()))
    {
        return Ok(None);
    }

    Ok(Some(cids))
}
