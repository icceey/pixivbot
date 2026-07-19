# ipfS3 ZIP Fallback Compatibility Implementation Plan

> **For agentic workers:** Use the subagent-driven-development skill to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make ipfS3 ZIP extraction a lossless optional optimization by preflighting the complete archive and falling back to EH per-image uploads for deterministic ZIP incompatibility or incomplete requested extraction results.

**Architecture:** Keep `ZipArchiveUploadInput` and `ImageUploader` unchanged. Add a private, byte-level compatibility gate inside `eh_client::telegraph::IpfS3Uploader` before request construction: scan the physical central directory to detect names hidden by `ZipArchive` deduplication, then use `by_index_raw` for unique-entry metadata and validate each complete local header/name. Make the private result mapper distinguish complete success (`Some`), deterministic fallback (`None`), and protocol failure (`Err`); `EhUploadWorker` continues to own the existing per-image fallback and only stops normalizing archive names.

**Tech Stack:** Rust 1.94, `zip` 8.6.0, `quick-xml` 0.38.4, `rust-s3` 0.37.2, Tokio, Wiremock, SeaORM in-memory SQLite tests.

**Global Constraints:**
- Preserve `ZipArchiveUploadInput<'a>` and `ImageUploader::upload_zip_archive_with_url_pairs(&self, archive: ZipArchiveUploadInput<'_>) -> Result<Option<Vec<TelegraphImageUrlPair>>>`.
- Use the existing `eh_client` dependency `zip = { version = "8.6.0", default-features = false }`; do not enable features or add dependencies.
- Treat `zip::CompressionMethod::STORE` and `zip::CompressionMethod::DEFLATE` as the only supported central methods. With `default-features = false`, `DEFLATE` is `Unsupported(8)`, so do not name or match a `Deflated` variant.
- Before indexed iteration, scan physical central-directory records from `ZipArchive::central_directory_start()` using the 46-byte fixed header, signature `PK\x01\x02`, and checked name/extra/comment lengths at offsets 28/30/32. Reject duplicate raw central names, and require the physical record count to equal `archive.len()` even when scanning stops at the first non-central signature.
- After the physical scan passes, inspect every unique entry with `ZipArchive::by_index_raw`; do not decompress entries during preflight.
- Read each complete local header from the original bytes using `ZipFile::header_start()`: require the 30-byte fixed block and signature `PK\x03\x04`, read flags/method/name length/extra length at offsets 6/8/26/28, and check all fixed/name/extra bounds and conversions.
- Require each local raw name to be UTF-8, satisfy the same path rules as its central `name_raw()`, and match the central raw name byte-for-byte. Do not rely on `ZipFileData::find_data_start`, because it does not compare local and central names.
- Match ipfS3 `master@276de042b29030195349fe91ac5ae8e944dcd591`: reject non-UTF-8 raw names, empty/root-only/absolute/Windows-drive/backslash names, exact `.` or `..` path segments, duplicate physical central names, central or local encryption, unsupported or central/local-mismatched methods, and Stored entries using local flag bit 3.
- Apply the same path safety rules to the generated `decompress-zip` extraction prefix before constructing `ReqwestRequest`.
- Preflight the complete archive, including directory and non-image entries; requested image names remain an ordered subset.
- Preserve archive name spelling from `ZipFile::name()` in `EhUploadWorker`; never replace `\` with `/`.
- Keep malformed ZIP-extract XML, wrong roots, count mismatches, empty requested CIDs, transport errors, non-2xx responses, authentication failures, and request construction failures as `Err`.
- Return `Ok(None)` without a ZIP PUT for preflight incompatibility or duplicate requested names, and return `Ok(None)` for missing/failed requested extraction results.
- Ignore failures only when they concern unrequested entries and every requested entry has a non-empty successful CID.
- Do not reuse partial ZIP results, alter EH queue/retry/archive-delivery state, add configuration, clean server-side objects, change non-ipfS3 uploaders, or change ordinary ipfS3 image uploads.
- All ZIP transport tests must pass structurally real ZIP bytes; no test may use the placeholder body `b"zip bytes"`.
- Do not install software. Do not run `git commit`, `git push`, `git tag`, or another git write unless the user explicitly authorizes the orchestrator to do so.

---

## Baseline and file map

- Approved specification: `docs/superpowers/specs/2026-07-20-ipfs3-zip-fallback-compatibility-design.md` at commit `f7d3eed`.
- Expected branch: `fix/ipfs3-zip-fallback-compatibility`; discovery found a clean worktree.
- Modify `eh_client/src/telegraph.rs:798-1225`: private ZIP preflight, uploader early fallback, extraction-result outcome mapping.
- Modify `eh_client/src/telegraph.rs:2453-2938`: real ZIP fixture builder and focused preflight/transport/result tests.
- Modify `src/scheduler/eh_engine.rs:1226-1245`: preserve ZIP entry names and correct the path comment.
- Modify `src/scheduler/eh_engine.rs:1384-1425`: correct the stale root-CID comment; retain the existing `None` fallthrough.
- Modify `src/scheduler/eh_engine.rs:2098-2121,3958-4059`: named ZIP fixture helper, configurable mock uploader, and ZIP-to-per-image fallback integration test.
- Create no source modules, migrations, configuration fields, or public documentation changes.

## Execution order

1. Task 1 establishes the physical-central plus complete-local-header compatibility boundary and guarantees zero transport on deterministic incompatibility.
2. Task 2 changes only the post-response classifier and adapts the private caller.
3. Task 3 proves the unchanged public `Ok(None)` contract reaches the existing EH per-image path with unmodified names.

### Task 1: Private complete-ZIP preflight and zero-request fallback

**Files:**
- Modify: `eh_client/src/telegraph.rs:919-989`
- Test: `eh_client/src/telegraph.rs:2453-2938`

**Interfaces:**
- Consumes: unchanged `ZipArchiveUploadInput<'a> { filename: &'a str, bytes: &'a [u8], entry_names: &'a [String] }`; `zip::ZipArchive`, `ZipArchive::{central_directory_start, by_index_raw}`, and `ZipFile::{name_raw, compression, encrypted, header_start}`.
- Produces: private `fn ipfs3_zip_central_directory_is_complete_and_unique(archive_bytes: &[u8], central_directory_start: u64, archive_len: usize) -> bool`, private `fn ipfs3_zip_local_header<'a>(bytes: &'a [u8], header_start: u64) -> Option<(u16, u16, &'a [u8])>`, and private `fn ipfs3_zip_archive_is_compatible(archive_bytes: &[u8], requested_entry_names: &[String], extraction_prefix: &str) -> bool`; unchanged uploader method returning `Ok(None)` before request construction when compatibility is false.

- [ ] **Step 1: Add a structurally real ZIP fixture builder to the colocated test module**

Add a small manual builder so tests can independently control local and central raw names, methods, flags, and data descriptors while still emitting local records, physical central-directory records, and EOCD. Use this concrete shape:

```rust
const ZIP_FIXTURE_DATA: &[u8] = b"hello";
const ZIP_FIXTURE_DEFLATED_DATA: &[u8] = &[0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x07, 0x00];

#[derive(Clone, Copy)]
struct ZipEntryFixture<'a> {
    local_name: &'a [u8],
    central_name: &'a [u8],
    data: &'a [u8],
    local_method: u16,
    central_method: u16,
    local_flags: u16,
    central_flags: u16,
}

impl<'a> ZipEntryFixture<'a> {
    fn stored(name: &'a [u8]) -> Self {
        Self {
            local_name: name,
            central_name: name,
            data: ZIP_FIXTURE_DATA,
            local_method: 0,
            central_method: 0,
            local_flags: 0,
            central_flags: 0,
        }
    }

    fn deflated(name: &'a [u8]) -> Self {
        Self {
            local_name: name,
            central_name: name,
            data: ZIP_FIXTURE_DATA,
            local_method: 8,
            central_method: 8,
            local_flags: 0,
            central_flags: 0,
        }
    }
}

fn push_zip_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn push_zip_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn zip_fixture_crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}

fn zip_fixture_encoded_data<'a>(entry: ZipEntryFixture<'a>) -> &'a [u8] {
    if entry.local_method == 8 && entry.data == ZIP_FIXTURE_DATA {
        ZIP_FIXTURE_DEFLATED_DATA
    } else {
        entry.data
    }
}

fn zip_fixture(entries: &[ZipEntryFixture<'_>]) -> Vec<u8> {
    let mut output = Vec::new();
    let mut local_offsets = Vec::with_capacity(entries.len());

    for entry in entries {
        let encoded = zip_fixture_encoded_data(*entry);
        let crc = zip_fixture_crc32(entry.data);
        let uses_descriptor = entry.local_flags & (1 << 3) != 0;
        local_offsets.push(output.len() as u32);

        push_zip_u32(&mut output, 0x0403_4b50);
        push_zip_u16(&mut output, 20);
        push_zip_u16(&mut output, entry.local_flags);
        push_zip_u16(&mut output, entry.local_method);
        push_zip_u16(&mut output, 0);
        push_zip_u16(&mut output, 0);
        push_zip_u32(&mut output, if uses_descriptor { 0 } else { crc });
        push_zip_u32(
            &mut output,
            if uses_descriptor { 0 } else { encoded.len() as u32 },
        );
        push_zip_u32(
            &mut output,
            if uses_descriptor { 0 } else { entry.data.len() as u32 },
        );
        push_zip_u16(&mut output, entry.local_name.len() as u16);
        push_zip_u16(&mut output, 0);
        output.extend_from_slice(entry.local_name);
        output.extend_from_slice(encoded);

        if uses_descriptor {
            push_zip_u32(&mut output, 0x0807_4b50);
            push_zip_u32(&mut output, crc);
            push_zip_u32(&mut output, encoded.len() as u32);
            push_zip_u32(&mut output, entry.data.len() as u32);
        }
    }

    let central_offset = output.len() as u32;
    for (entry, local_offset) in entries.iter().zip(local_offsets) {
        let encoded = zip_fixture_encoded_data(*entry);
        push_zip_u32(&mut output, 0x0201_4b50);
        push_zip_u16(&mut output, 20);
        push_zip_u16(&mut output, 20);
        push_zip_u16(&mut output, entry.central_flags);
        push_zip_u16(&mut output, entry.central_method);
        push_zip_u16(&mut output, 0);
        push_zip_u16(&mut output, 0);
        push_zip_u32(&mut output, zip_fixture_crc32(entry.data));
        push_zip_u32(&mut output, encoded.len() as u32);
        push_zip_u32(&mut output, entry.data.len() as u32);
        push_zip_u16(&mut output, entry.central_name.len() as u16);
        push_zip_u16(&mut output, 0);
        push_zip_u16(&mut output, 0);
        push_zip_u16(&mut output, 0);
        push_zip_u16(&mut output, 0);
        push_zip_u32(&mut output, 0);
        push_zip_u32(&mut output, local_offset);
        output.extend_from_slice(entry.central_name);
    }

    let central_size = output.len() as u32 - central_offset;
    push_zip_u32(&mut output, 0x0605_4b50);
    push_zip_u16(&mut output, 0);
    push_zip_u16(&mut output, 0);
    push_zip_u16(&mut output, entries.len() as u16);
    push_zip_u16(&mut output, entries.len() as u16);
    push_zip_u32(&mut output, central_size);
    push_zip_u32(&mut output, central_offset);
    push_zip_u16(&mut output, 0);
    output
}

fn duplicate_physical_name_zip_fixture() -> Vec<u8> {
    zip_fixture(&[
        ZipEntryFixture {
            local_name: b"../page.jpg",
            central_name: b"page.jpg",
            ..ZipEntryFixture::stored(b"page.jpg")
        },
        ZipEntryFixture::stored(b"page.jpg"),
    ])
}
```

This fixture deliberately uses raw Deflate bytes for method 8 and never asks the feature-disabled `zip` crate to decompress them. Separate local/central fields are required because `ZipFileData::find_data_start` reads the local fixed block only to locate data and does not compare local and central names; `duplicate_physical_name_zip_fixture` also emits two physical central records with the same raw name so `ZipArchive` retains only the later `IndexMap` value.

- [ ] **Step 2: Write preflight tests before the compatibility helper exists**

Add these exact tests:

```rust
#[test]
fn ipfs3_zip_preflight_accepts_stored_deflate_and_safe_prefixes() {
    let bytes = zip_fixture(&[
        ZipEntryFixture::stored(b"page001.jpg"),
        ZipEntryFixture::deflated(b"dir/page002.png"),
        ZipEntryFixture::stored(b"metadata/"),
    ]);
    let requested = vec!["page001.jpg".to_string(), "dir/page002.png".to_string()];

    assert!(ipfs3_zip_archive_is_compatible(
        &bytes,
        &requested,
        "eh/archive/"
    ));
}

#[test]
fn ipfs3_zip_preflight_rejects_incompatible_entries_and_prefixes() {
    let unsafe_names: &[&[u8]] = &[
        b"",
        b"/",
        b"/absolute.jpg",
        b"C:/drive.jpg",
        b"dir\\page.jpg",
        b"a/./page.jpg",
        b"a/../page.jpg",
        b"\xff.jpg",
    ];
    for name in unsafe_names {
        let bytes = zip_fixture(&[ZipEntryFixture::stored(name)]);
        assert!(!ipfs3_zip_archive_is_compatible(
            &bytes,
            &["page.jpg".to_string()],
            "eh/archive/"
        ));
    }

    let requested = vec!["page.jpg".to_string()];
    let encrypted = zip_fixture(&[ZipEntryFixture {
        central_flags: 1,
        ..ZipEntryFixture::stored(b"page.jpg")
    }]);
    let unsupported = zip_fixture(&[ZipEntryFixture {
        local_method: 12,
        central_method: 12,
        ..ZipEntryFixture::stored(b"page.jpg")
    }]);
    let mismatched = zip_fixture(&[ZipEntryFixture {
        local_method: 8,
        central_method: 0,
        ..ZipEntryFixture::stored(b"page.jpg")
    }]);
    let stored_descriptor = zip_fixture(&[ZipEntryFixture {
        local_flags: 1 << 3,
        central_flags: 1 << 3,
        ..ZipEntryFixture::stored(b"page.jpg")
    }]);
    let unsafe_unrequested = zip_fixture(&[
        ZipEntryFixture::stored(b"page.jpg"),
        ZipEntryFixture::stored(b"../notes.txt"),
    ]);

    for bytes in [
        encrypted,
        unsupported,
        mismatched,
        stored_descriptor,
        unsafe_unrequested,
    ] {
        assert!(!ipfs3_zip_archive_is_compatible(
            &bytes,
            &requested,
            "eh/archive/"
        ));
    }

    let valid = zip_fixture(&[ZipEntryFixture::stored(b"page.jpg")]);
    for prefix in ["/absolute/", "C:/drive/", "dir\\prefix/", "a/./", "a/../", "/"] {
        assert!(!ipfs3_zip_archive_is_compatible(
            &valid,
            &requested,
            prefix
        ));
    }
    assert!(!ipfs3_zip_archive_is_compatible(
        &valid,
        &["page.jpg".to_string(), "page.jpg".to_string()],
        "eh/archive/"
    ));
}

#[test]
fn ipfs3_zip_preflight_rejects_incompatible_local_names_and_flags() {
    let requested = vec!["page.jpg".to_string()];
    let entries = [
        ZipEntryFixture {
            local_name: b"\xff.jpg",
            ..ZipEntryFixture::stored(b"page.jpg")
        },
        ZipEntryFixture {
            local_name: b"../page.jpg",
            ..ZipEntryFixture::stored(b"page.jpg")
        },
        ZipEntryFixture {
            local_name: b"dir\\page.jpg",
            ..ZipEntryFixture::stored(b"page.jpg")
        },
        ZipEntryFixture {
            local_name: b"other.jpg",
            ..ZipEntryFixture::stored(b"page.jpg")
        },
        ZipEntryFixture {
            local_flags: 1,
            ..ZipEntryFixture::stored(b"page.jpg")
        },
    ];

    for entry in entries {
        let bytes = zip_fixture(&[entry]);
        assert!(!ipfs3_zip_archive_is_compatible(
            &bytes,
            &requested,
            "eh/archive/"
        ));
    }
}

#[test]
fn ipfs3_zip_preflight_rejects_duplicate_physical_central_names() {
    let bytes = duplicate_physical_name_zip_fixture();

    assert!(!ipfs3_zip_archive_is_compatible(
        &bytes,
        &["page.jpg".to_string()],
        "eh/archive/"
    ));
}

#[test]
fn ipfs3_zip_central_scan_rejects_early_stop_before_archive_len() {
    let mut bytes = zip_fixture(&[
        ZipEntryFixture::stored(b"one.jpg"),
        ZipEntryFixture::stored(b"two.jpg"),
    ]);
    let archive = zip::ZipArchive::new(std::io::Cursor::new(bytes.as_slice())).unwrap();
    let central_start = archive.central_directory_start();
    let archive_len = archive.len();
    drop(archive);
    let second_record_start = usize::try_from(central_start).unwrap()
        + ZIP_CENTRAL_FIXED_HEADER_LEN
        + b"one.jpg".len();
    bytes[second_record_start..second_record_start + 4].copy_from_slice(b"STOP");

    assert!(!ipfs3_zip_central_directory_is_complete_and_unique(
        &bytes,
        central_start,
        archive_len,
    ));
}
```

Add this Wiremock transport test so incompatibility is proven before request construction:

```rust
#[tokio::test]
async fn ipfs3_zip_archive_upload_preflight_fallback_sends_no_put() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let mut config = complete_ipfs3_config(&server.uri(), "https://public.example/ipfs/");
    config.zip_extract_enabled = true;
    let uploader = IpfS3Uploader::from_config(&config).unwrap();
    let zip_bytes = zip_fixture(&[ZipEntryFixture::stored(b"dir\\page.jpg")]);
    let entries = vec!["dir\\page.jpg".to_string()];

    let result = uploader
        .upload_zip_archive_with_url_pairs(ZipArchiveUploadInput {
            filename: "gallery.zip",
            bytes: &zip_bytes,
            entry_names: &entries,
        })
        .await
        .unwrap();

    assert!(result.is_none());
}

#[tokio::test]
async fn ipfs3_zip_archive_upload_duplicate_physical_name_sends_no_put() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let mut config = complete_ipfs3_config(&server.uri(), "https://public.example/ipfs/");
    config.zip_extract_enabled = true;
    let uploader = IpfS3Uploader::from_config(&config).unwrap();
    let zip_bytes = duplicate_physical_name_zip_fixture();
    let entries = vec!["page.jpg".to_string()];

    let result = uploader
        .upload_zip_archive_with_url_pairs(ZipArchiveUploadInput {
            filename: "gallery.zip",
            bytes: &zip_bytes,
            entry_names: &entries,
        })
        .await
        .unwrap();

    assert!(result.is_none());
}
```

- [ ] **Step 3: Run every new test and record the RED result**

Run each command separately:

```powershell
cargo test -p eh_client telegraph::tests::ipfs3_zip_preflight_accepts_stored_deflate_and_safe_prefixes -- --exact
cargo test -p eh_client telegraph::tests::ipfs3_zip_preflight_rejects_incompatible_entries_and_prefixes -- --exact
cargo test -p eh_client telegraph::tests::ipfs3_zip_preflight_rejects_incompatible_local_names_and_flags -- --exact
cargo test -p eh_client telegraph::tests::ipfs3_zip_preflight_rejects_duplicate_physical_central_names -- --exact
cargo test -p eh_client telegraph::tests::ipfs3_zip_central_scan_rejects_early_stop_before_archive_len -- --exact
cargo test -p eh_client telegraph::tests::ipfs3_zip_archive_upload_preflight_fallback_sends_no_put -- --exact
cargo test -p eh_client telegraph::tests::ipfs3_zip_archive_upload_duplicate_physical_name_sends_no_put -- --exact
```

Expected: each command fails to compile because `ipfs3_zip_archive_is_compatible` and the physical-central/local-name checks are not defined; the transport tests additionally have no early preflight fallback implementation.

- [ ] **Step 4: Implement the private path/local-header/archive compatibility checks**

Place the helpers immediately before `impl IpfS3Uploader` or immediately before `archive_object_key`; keep all symbols private. The minimum implementation must have this control flow and constants:

```rust
const ZIP_CENTRAL_HEADER_SIGNATURE: [u8; 4] = [0x50, 0x4b, 0x01, 0x02];
const ZIP_CENTRAL_FIXED_HEADER_LEN: usize = 46;
const ZIP_CENTRAL_NAME_LEN_OFFSET: usize = 28;
const ZIP_CENTRAL_EXTRA_LEN_OFFSET: usize = 30;
const ZIP_CENTRAL_COMMENT_LEN_OFFSET: usize = 32;
const ZIP_LOCAL_HEADER_SIGNATURE: [u8; 4] = [0x50, 0x4b, 0x03, 0x04];
const ZIP_LOCAL_FIXED_HEADER_LEN: usize = 30;
const ZIP_LOCAL_FLAGS_OFFSET: usize = 6;
const ZIP_LOCAL_METHOD_OFFSET: usize = 8;
const ZIP_LOCAL_NAME_LEN_OFFSET: usize = 26;
const ZIP_LOCAL_EXTRA_LEN_OFFSET: usize = 28;

fn ipfs3_zip_u16_at(bytes: &[u8], offset: usize) -> Option<u16> {
    let end = offset.checked_add(2)?;
    let value: [u8; 2] = bytes.get(offset..end)?.try_into().ok()?;
    Some(u16::from_le_bytes(value))
}

fn ipfs3_zip_path_is_safe(value: &str, allow_empty: bool) -> bool {
    if value.is_empty() {
        return allow_empty;
    }
    let bytes = value.as_bytes();
    let has_windows_drive =
        bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    if value.contains('\\') || value.starts_with('/') || has_windows_drive {
        return false;
    }
    if value.split('/').any(|segment| segment == "." || segment == "..") {
        return false;
    }
    !value.trim_matches('/').is_empty()
}

fn ipfs3_zip_central_directory_is_complete_and_unique(
    archive_bytes: &[u8],
    central_directory_start: u64,
    archive_len: usize,
) -> bool {
    let Ok(mut offset) = usize::try_from(central_directory_start) else {
        return false;
    };
    let mut physical_count = 0usize;
    let mut raw_names = std::collections::HashSet::new();

    loop {
        let Some(signature_end) = offset.checked_add(ZIP_CENTRAL_HEADER_SIGNATURE.len()) else {
            return false;
        };
        let Some(signature) = archive_bytes.get(offset..signature_end) else {
            return false;
        };
        if signature != ZIP_CENTRAL_HEADER_SIGNATURE {
            break;
        }
        let Some(fixed_end) = offset.checked_add(ZIP_CENTRAL_FIXED_HEADER_LEN) else {
            return false;
        };
        let Some(header) = archive_bytes.get(offset..fixed_end) else {
            return false;
        };

        let Some(name_len) = ipfs3_zip_u16_at(header, ZIP_CENTRAL_NAME_LEN_OFFSET) else {
            return false;
        };
        let Some(extra_len) = ipfs3_zip_u16_at(header, ZIP_CENTRAL_EXTRA_LEN_OFFSET) else {
            return false;
        };
        let Some(comment_len) = ipfs3_zip_u16_at(header, ZIP_CENTRAL_COMMENT_LEN_OFFSET) else {
            return false;
        };
        let name_len = usize::from(name_len);
        let Some(name_end) = fixed_end.checked_add(name_len) else {
            return false;
        };
        let Some(variable_len) = name_len
            .checked_add(usize::from(extra_len))
            .and_then(|len| len.checked_add(usize::from(comment_len)))
        else {
            return false;
        };
        let Some(record_end) = fixed_end.checked_add(variable_len) else {
            return false;
        };
        if archive_bytes.get(offset..record_end).is_none() {
            return false;
        }
        let Some(raw_name) = archive_bytes.get(fixed_end..name_end) else {
            return false;
        };
        if !raw_names.insert(raw_name) {
            return false;
        }
        let Some(next_count) = physical_count.checked_add(1) else {
            return false;
        };
        physical_count = next_count;
        offset = record_end;
    }

    physical_count == archive_len
}

fn ipfs3_zip_local_header<'a>(
    bytes: &'a [u8],
    header_start: u64,
) -> Option<(u16, u16, &'a [u8])> {
    let start = usize::try_from(header_start).ok()?;
    let fixed_end = start.checked_add(ZIP_LOCAL_FIXED_HEADER_LEN)?;
    let header = bytes.get(start..fixed_end)?;
    if header[..ZIP_LOCAL_HEADER_SIGNATURE.len()] != ZIP_LOCAL_HEADER_SIGNATURE {
        return None;
    }
    let flags = ipfs3_zip_u16_at(header, ZIP_LOCAL_FLAGS_OFFSET)?;
    let method = ipfs3_zip_u16_at(header, ZIP_LOCAL_METHOD_OFFSET)?;
    let name_len = usize::from(ipfs3_zip_u16_at(header, ZIP_LOCAL_NAME_LEN_OFFSET)?);
    let extra_len = usize::from(ipfs3_zip_u16_at(header, ZIP_LOCAL_EXTRA_LEN_OFFSET)?);
    let name_end = fixed_end.checked_add(name_len)?;
    let local_record_end = name_end.checked_add(extra_len)?;
    let local_name = bytes.get(fixed_end..name_end)?;
    bytes.get(name_end..local_record_end)?;
    Some((flags, method, local_name))
}

fn ipfs3_zip_archive_is_compatible(
    archive_bytes: &[u8],
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

    let cursor = std::io::Cursor::new(archive_bytes);
    let Ok(mut archive) = zip::ZipArchive::new(cursor) else {
        return false;
    };
    if !ipfs3_zip_central_directory_is_complete_and_unique(
        archive_bytes,
        archive.central_directory_start(),
        archive.len(),
    ) {
        return false;
    }
    for index in 0..archive.len() {
        let Ok(file) = archive.by_index_raw(index) else {
            return false;
        };
        let Ok(raw_name) = std::str::from_utf8(file.name_raw()) else {
            return false;
        };
        if !ipfs3_zip_path_is_safe(raw_name, false) || file.encrypted() {
            return false;
        }

        let central_method = file.compression();
        let expected_local_method = if central_method == zip::CompressionMethod::STORE {
            0
        } else if central_method == zip::CompressionMethod::DEFLATE {
            8
        } else {
            return false;
        };
        let Some((local_flags, local_method, local_raw_name)) =
            ipfs3_zip_local_header(archive_bytes, file.header_start())
        else {
            return false;
        };
        let Ok(local_name) = std::str::from_utf8(local_raw_name) else {
            return false;
        };
        let local_is_encrypted = local_flags & 1 != 0;
        let stored_uses_descriptor =
            local_method == 0 && local_flags & (1 << 3) != 0;
        if !ipfs3_zip_path_is_safe(local_name, false)
            || local_raw_name != file.name_raw()
            || local_is_encrypted
            || local_method != expected_local_method
            || stored_uses_descriptor
        {
            return false;
        }
    }
    true
}
```

Physical-central scan, parsing, complete-local-header, local-name, or bounds failures are deterministic preflight incompatibilities (`false`), not network/protocol errors. The physical scan must occur before `by_index_raw`, because `ZipArchiveMetadata.files` deduplicates equal raw names in its `IndexMap`; the unique-entry loop still uses `by_index_raw` so method 8 and encrypted fixtures can be classified without decoder/password failures.

- [ ] **Step 5: Gate request construction and convert all archive transport tests to real ZIP bytes**

In `IpfS3Uploader::upload_zip_archive_with_url_pairs`, derive `key` and `extraction_prefix` as today, then place this before cloning the bucket or constructing `Command::PutObject`:

```rust
if !ipfs3_zip_archive_is_compatible(
    archive.bytes,
    archive.entry_names,
    &extraction_prefix,
) {
    return Ok(None);
}
```

In all of these existing tests, create a concrete fixture such as `let zip_bytes = zip_fixture(&[ZipEntryFixture::stored(b"page001.jpg")]);`, pass `bytes: &zip_bytes`, and update any `body_bytes` matcher to `body_bytes(zip_bytes.clone())`:

- `default_zip_archive_upload_capability_returns_none`
- `ipfs3_zip_archive_upload_disabled_returns_none_without_put`
- `ipfs3_zip_archive_upload_signs_query_and_uses_ordered_entry_cids`
- `ipfs3_zip_archive_upload_rejects_malformed_xml`
- `ipfs3_zip_archive_upload_rejects_non_success_status`

For the signed-query test, the fixture must contain `page001.jpg`, `dir/page002.png`, and `notes.txt`, matching the responder keys. For malformed-XML and non-success tests, use one Stored `page001.jpg` entry so preflight passes and the intended response branch is reached.

- [ ] **Step 6: Run every new test and the affected transport tests for GREEN**

Run the same seven commands from Step 3; expected: each reports one passed test. The duplicate-central unit and transport tests prove that two physical records collapsed to one `ZipArchive` entry are rejected before any PUT, and the direct central-scan test proves a non-central signature cannot hide unscanned entries through early termination.

Then run:

```powershell
cargo test -p eh_client telegraph::tests::default_zip_archive_upload_capability_returns_none -- --exact
cargo test -p eh_client telegraph::tests::ipfs3_zip_archive_upload_disabled_returns_none_without_put -- --exact
cargo test -p eh_client telegraph::tests::ipfs3_zip_archive_upload_signs_query_and_uses_ordered_entry_cids -- --exact
cargo test -p eh_client telegraph::tests::ipfs3_zip_archive_upload_rejects_malformed_xml -- --exact
cargo test -p eh_client telegraph::tests::ipfs3_zip_archive_upload_rejects_non_success_status -- --exact
```

Expected: all five existing transport commands pass; both new preflight transport tests confirm zero received PUTs, while valid fixtures still exercise signed query, malformed XML, and non-2xx behavior.

**Review boundary:** Review only the private physical-central scan, complete local-header/name preflight, fixture integrity, and request ordering. Public input/trait types and Cargo manifests must be unchanged.

### Task 2: Three-way extraction-result classification

**Files:**
- Modify: `eh_client/src/telegraph.rs:975-988,1063-1072,1169-1225`
- Test: `eh_client/src/telegraph.rs:2514-2686`

**Interfaces:**
- Consumes: parsed `IpfS3ZipExtractResult`, exact `extraction_prefix`, and requested names in archive order.
- Produces: private `fn ipfs3_zip_entry_cids(extraction_prefix: &str, entry_names: &[String], result: IpfS3ZipExtractResult) -> Result<Option<Vec<String>>>`; `Some` means all requested entries succeeded, `None` means per-image fallback, and `Err` remains a retryable protocol failure.

- [ ] **Step 1: Rewrite/add result-classification tests with explicit outcomes**

Make these exact test changes:

1. Keep `ipfs3_zip_extract_result_maps_exact_keys_in_requested_order_and_last_key_wins`, but unwrap both layers and retain `assert_eq!(cids, ["cid-two", "cid-last"])`.
2. Rename `ipfs3_zip_extract_result_rejects_reported_failures` to `ipfs3_zip_extract_result_falls_back_for_failed_requested_entry` and assert `Ok(None)` for requested names `page.jpg` and `failed.jpg` when only `page.jpg` succeeds and `failed.jpg` appears in `Failures`.
3. Add `ipfs3_zip_extract_result_ignores_unrequested_failure_when_requested_entries_succeed`:

```rust
#[test]
fn ipfs3_zip_extract_result_ignores_unrequested_failure_when_requested_entries_succeed() {
    let result = parse_ipfs3_zip_extract_result(&ipfs3_zip_extract_result_xml(
        &[("extract/page.jpg", "cid-page")],
        &[("notes.txt", "EntryReadFailed", "bad metadata")],
        1,
        1,
    ))
    .unwrap();

    let cids = ipfs3_zip_entry_cids(
        "extract/",
        &["page.jpg".to_string()],
        result,
    )
    .unwrap();
    assert_eq!(cids, Some(vec!["cid-page".to_string()]));
}
```

4. Rename `ipfs3_zip_extract_result_rejects_missing_requested_key` to `ipfs3_zip_extract_result_falls_back_for_missing_requested_key` and assert `unwrap().is_none()`.
5. Rename `ipfs3_zip_extract_result_rejects_duplicate_requested_names` to `ipfs3_zip_extract_result_falls_back_for_duplicate_requested_names` and assert `unwrap().is_none()`.
6. Keep `ipfs3_zip_extract_result_rejects_declared_count_inconsistencies`, `ipfs3_zip_extract_result_rejects_empty_requested_entry_cid`, and malformed/wrong-root tests as errors.

Use these concrete bodies for the three fallback cases:

```rust
#[test]
fn ipfs3_zip_extract_result_falls_back_for_failed_requested_entry() {
    let result = parse_ipfs3_zip_extract_result(&ipfs3_zip_extract_result_xml(
        &[("extract/page.jpg", "cid-page")],
        &[("failed.jpg", "ExtractFailed", "bad archive")],
        1,
        1,
    ))
    .unwrap();

    let cids = ipfs3_zip_entry_cids(
        "extract/",
        &["page.jpg".to_string(), "failed.jpg".to_string()],
        result,
    )
    .unwrap();
    assert!(cids.is_none());
}

#[test]
fn ipfs3_zip_extract_result_falls_back_for_missing_requested_key() {
    let result = parse_ipfs3_zip_extract_result(&ipfs3_zip_extract_result_xml(
        &[("extract/page.jpg", "cid-page")],
        &[],
        1,
        0,
    ))
    .unwrap();

    let cids = ipfs3_zip_entry_cids(
        "extract/",
        &["missing.jpg".to_string()],
        result,
    )
    .unwrap();
    assert!(cids.is_none());
}

#[test]
fn ipfs3_zip_extract_result_falls_back_for_duplicate_requested_names() {
    let result = parse_ipfs3_zip_extract_result(&ipfs3_zip_extract_result_xml(
        &[("extract/page.jpg", "cid-page")],
        &[],
        1,
        0,
    ))
    .unwrap();

    let cids = ipfs3_zip_entry_cids(
        "extract/",
        &["page.jpg".to_string(), "page.jpg".to_string()],
        result,
    )
    .unwrap();
    assert!(cids.is_none());
}
```

- [ ] **Step 2: Run each changed/new classification test for RED**

```powershell
cargo test -p eh_client telegraph::tests::ipfs3_zip_extract_result_maps_exact_keys_in_requested_order_and_last_key_wins -- --exact
cargo test -p eh_client telegraph::tests::ipfs3_zip_extract_result_falls_back_for_failed_requested_entry -- --exact
cargo test -p eh_client telegraph::tests::ipfs3_zip_extract_result_ignores_unrequested_failure_when_requested_entries_succeed -- --exact
cargo test -p eh_client telegraph::tests::ipfs3_zip_extract_result_falls_back_for_missing_requested_key -- --exact
cargo test -p eh_client telegraph::tests::ipfs3_zip_extract_result_falls_back_for_duplicate_requested_names -- --exact
```

Expected: compilation/assertion failures because the mapper still returns `Result<Vec<String>>` and rejects every reported failure, missing key, and duplicate request.

- [ ] **Step 3: Implement `Result<Option<Vec<String>>>` without weakening structural validation**

Change only the private mapper. Preserve count checks first, preserve the last-response-entry-wins `HashMap::insert`, then use this ordering:

```rust
fn ipfs3_zip_entry_cids(
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

    let mut requested_names = std::collections::HashSet::new();
    for entry_name in entry_names {
        if !requested_names.insert(entry_name.as_str()) {
            return Ok(None);
        }
    }

    let mut entry_cids = std::collections::HashMap::new();
    for entry in result.entries.entries {
        entry_cids.insert(entry.key, entry.etag);
    }

    let mut cids = Vec::with_capacity(entry_names.len());
    for entry_name in entry_names {
        let key = format!("{extraction_prefix}{entry_name}");
        let Some(cid) = entry_cids.get(&key) else {
            return Ok(None);
        };
        let cid = cid.trim().trim_matches('"').trim();
        if cid.is_empty() {
            return Err(Error::Other(format!(
                "ipfS3 ZIP extraction requested extraction key {key} returned an empty CID"
            )));
        }
        cids.push(cid.to_string());
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
```

The successful-CID loop intentionally precedes requested-failure classification so a present but empty requested CID remains `Err`. Prefix unused response fields as `_code` and `_message` while retaining their explicit `#[serde(rename = "Code")]` and `#[serde(rename = "Message")]` attributes; this preserves strict deserialization and avoids dead-code warnings under `-D warnings`.

- [ ] **Step 4: Adapt the uploader without warming or reusing partial results**

Replace the current direct assignment of the mapper's `Vec<String>` result with:

```rust
let Some(cids) =
    ipfs3_zip_entry_cids(&extraction_prefix, archive.entry_names, extract_result)?
else {
    return Ok(None);
};
```

Only map/warm the CIDs after this `Some` match. This prevents partial ZIP CIDs from being warmed or passed onward before the caller falls back.

- [ ] **Step 5: Run classification tests and strict error regressions for GREEN**

Run all five commands from Step 2; expected: each passes.

Then run each preserved error contract:

```powershell
cargo test -p eh_client telegraph::tests::ipfs3_zip_extract_result_rejects_declared_count_inconsistencies -- --exact
cargo test -p eh_client telegraph::tests::ipfs3_zip_extract_result_rejects_empty_requested_entry_cid -- --exact
cargo test -p eh_client telegraph::tests::ipfs3_zip_extract_result_rejects_empty_and_malformed_xml -- --exact
cargo test -p eh_client telegraph::tests::ipfs3_zip_extract_result_rejects_wrong_root_element -- --exact
cargo test -p eh_client telegraph::tests::ipfs3_zip_archive_upload_rejects_malformed_xml -- --exact
```

Expected: all commands pass; counts/XML/empty requested CID are still errors, while only incomplete requested coverage or a requested extraction failure yields `None`.

**Review boundary:** Verify the three outcomes at the private mapper boundary and that failure records for unrequested entries cannot suppress complete requested success.

### Task 3: Preserve EH raw names and prove ZIP `None` reaches per-image success

**Files:**
- Modify: `src/scheduler/eh_engine.rs:1226-1245`
- Modify: `src/scheduler/eh_engine.rs:1384-1425`
- Test: `src/scheduler/eh_engine.rs:2098-2121,3958-4059`

**Interfaces:**
- Consumes: unchanged `ImageUploader::upload_zip_archive_with_url_pairs(&self, archive: ZipArchiveUploadInput<'_>) -> Result<Option<Vec<TelegraphImageUrlPair>>>`; `None` from Tasks 1-2.
- Produces: `collect_uploadable_zip_entry_names(zip_path: &std::path::Path) -> anyhow::Result<Vec<String>>` returning the ordered `ZipFile::name()` spelling without slash replacement; an integration proof that `None` calls `upload_images_with_url_pairs` once per image and reaches `STATUS_UPLOADED`.

- [ ] **Step 1: Make the ZIP-first mock capable of deterministic fallback**

Extend the existing `ZipFirstMockUploader` without changing its default ZIP-success behavior:

```rust
#[derive(Default)]
struct ZipFirstMockUploader {
    zip_calls: std::sync::atomic::AtomicUsize,
    image_calls: std::sync::atomic::AtomicUsize,
    seen_entries: std::sync::Mutex<Vec<String>>,
    zip_fallback: bool,
}
```

Change its `upload_images` implementation to return one usable URL per input while counting calls:

```rust
async fn upload_images(
    &self,
    images: &[ImageUploadInput<'_>],
) -> eh_client::Result<Vec<String>> {
    self.image_calls
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    Ok(images
        .iter()
        .map(|image| {
            format!(
                "https://fallback.example/{}",
                image.filename.replace('\\', "-").replace('/', "-")
            )
        })
        .collect())
}
```

At the start of its ZIP method, after recording `archive.entry_names`, add:

```rust
if self.zip_fallback {
    return Ok(None);
}
```

Add a helper beside `create_test_zip` for explicit names:

```rust
fn create_test_zip_with_names(path: &std::path::Path, names: &[&str]) {
    let file = std::fs::File::create(path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    for (index, name) in names.iter().enumerate() {
        zip.start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(format!("fake_image_data_{index}").as_bytes())
            .unwrap();
    }
    zip.finish().unwrap();
}
```

- [ ] **Step 2: Add the fallback integration test**

Add `test_upload_worker_falls_back_to_per_image_when_zip_uploader_returns_none` beside `test_upload_worker_prefers_zip_archive_uploader` with this complete setup and assertions:

```rust
#[tokio::test]
async fn test_upload_worker_falls_back_to_per_image_when_zip_uploader_returns_none() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    setup_chat(&repo, -100, true).await;
    let tg_server = MockServer::start().await;
    mock_telegraph_create_page(&tg_server).await;
    let temp_dir = tempfile::tempdir().unwrap();
    let zip_path = temp_dir.path().join("zip_fallback.zip");
    create_test_zip_with_names(&zip_path, &["dir\\page000.jpg", "page001.jpg"]);
    let zip_path_str = zip_path.to_string_lossy().to_string();
    let entry = insert_queue_entry(
        &repo,
        -100,
        701,
        "tok",
        "Fallback Title",
        true,
        STATUS_DOWNLOADED,
        Some(&zip_path_str),
        None,
    )
    .await;
    let uploader = Arc::new(ZipFirstMockUploader {
        zip_fallback: true,
        ..Default::default()
    });
    let worker = EhUploadWorker::new(
        Arc::clone(&repo),
        make_notifier(&tg_server),
        make_telegraph_client(&tg_server),
        uploader.clone(),
        None,
        Arc::new(make_config()),
    );

    worker.tick().await.unwrap();

    assert_eq!(
        uploader.zip_calls.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(
        uploader.image_calls.load(std::sync::atomic::Ordering::SeqCst),
        2
    );
    assert_eq!(
        *uploader.seen_entries.lock().unwrap(),
        vec!["dir\\page000.jpg".to_string(), "page001.jpg".to_string()]
    );
    let model = eh_download_queue::Entity::find_by_id(entry.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(model.status, STATUS_UPLOADED);
    assert!(model.telegraph_url.is_some());
}
```

- [ ] **Step 3: Run the new integration test for RED**

```powershell
cargo test -p pixivbot scheduler::eh_engine::integration_tests::test_upload_worker_falls_back_to_per_image_when_zip_uploader_returns_none -- --exact
```

Expected: assertion failure because `collect_uploadable_zip_entry_names` currently records `dir/page000.jpg` instead of `dir\page000.jpg`; before the mock change, the per-image branch also cannot produce URLs.

- [ ] **Step 4: Remove slash normalization and correct both stale comments**

In `collect_uploadable_zip_entry_names`, retain the existing image/directory filtering but replace the normalized assignment with the raw `ZipFile::name()` spelling:

```rust
let name = file.name();
if !file.is_dir() && is_uploadable_zip_image_name(&name.to_lowercase()) {
    names.push(name.to_string());
}
```

Replace the function comment at current lines 1226-1231 with:

```rust
/// Collect the entry names of uploadable image files inside a ZIP archive,
/// preserving archive order and the entry spelling exposed by `ZipFile::name()`.
/// Non-image entries (directories, metadata, thumbnails) are skipped here but
/// remain present in the archive for an uploader's complete-archive preflight.
```

Replace the ZIP-first comment at current lines 1393-1395 with:

```rust
// ZIP-first path: if the configured uploader can accept the whole archive,
// upload it once and build Telegraph URLs from each returned extraction CID.
// `None` deliberately falls through to the existing per-image upload path.
```

Do not change the control flow after `if let Some(url_pairs)`: the existing fallthrough is the desired fallback and must retain its queue/retry semantics.

- [ ] **Step 5: Run worker tests for GREEN**

```powershell
cargo test -p pixivbot scheduler::eh_engine::integration_tests::test_upload_worker_falls_back_to_per_image_when_zip_uploader_returns_none -- --exact
cargo test -p pixivbot scheduler::eh_engine::integration_tests::test_upload_worker_prefers_zip_archive_uploader -- --exact
cargo test -p pixivbot scheduler::eh_engine::integration_tests::test_upload_worker_full_flow -- --exact
cargo test -p pixivbot scheduler::eh_engine::integration_tests::test_upload_worker_no_images_fails -- --exact
```

Expected: all four commands pass. The new test records one ZIP attempt, two per-image calls, unmodified backslash spelling, and final uploaded state; the existing ZIP-success test still records zero per-image calls.

**Review boundary:** Verify only name preservation, comments, mock behavior, and the existing `None` fallthrough; no scheduler state transition or retry logic may change.

## Final verification, review, and version-control checkpoints

- [ ] **Focused regression sweep**

```powershell
cargo test -p eh_client telegraph::tests::ipfs3_zip
cargo test -p pixivbot scheduler::eh_engine::integration_tests::test_upload_worker_
```

Expected: both commands exit 0; all ipfS3 ZIP parser/preflight/transport tests and EH upload-worker tests pass.

- [ ] **Formatting and whitespace review**

```powershell
cargo fmt --all
cargo fmt --all -- --check
git diff --check
git status --short --branch
git diff -- "eh_client/src/telegraph.rs" "src/scheduler/eh_engine.rs"
```

Expected: format check and `git diff --check` exit 0; status lists only the plan and the two scoped Rust files; diff shows no public trait/input, config, Cargo, queue-state, or cleanup changes.

- [ ] **Full repository gate**

```powershell
make ci
```

Expected: `fmt-check`, Clippy with warnings denied, workspace check, workspace tests, and release build all succeed, ending with `All CI checks passed!`. Do not enable `ffmpeg-codec` tests beyond what the repository's `make ci` target already does.

- [ ] **Adversarial review checklist**

Confirm from the final diff and test output:

1. Preflight starts at `ZipArchive::central_directory_start()`, parses each physical 46-byte central header with checked name/extra/comment lengths, and rejects duplicate raw names before indexed iteration.
2. A non-central signature may stop the physical scan only when `physical_count == archive.len()`; the early-stop regression test proves a shorter scan is rejected.
3. After physical uniqueness/count checks pass, preflight iterates every unique ZIP entry with `by_index_raw`, including unrequested files/directories.
4. Each local probe covers the complete 30-byte fixed header plus its declared name and extra fields with checked bounds.
5. Both central and local raw names must be UTF-8 and satisfy the same path rules, and the local bytes must exactly equal central `name_raw()` bytes; `ZipFileData::find_data_start` and `name()` cannot bypass these checks.
6. Prefix and both entry-name path checks match ipfS3 commit `276de042`: backslash, absolute, drive, root-only, `.` segment, and `..` segment rejection.
7. Both central and local methods are 0 or 8 and equal; method 8 is compared through `CompressionMethod::DEFLATE`.
8. Central or local encryption and Stored plus local bit 3 return `Ok(None)` before request construction; duplicate physical names also produce zero PUTs even when the overwritten earlier entry has an unsafe local name.
9. Duplicate requested names return `None` both before transport and defensively in the mapper.
10. Count/XML/empty requested CID failures remain `Err`; missing or failed requested entries return `None`; unrequested failures do not discard complete requested results, and no partial CIDs are warmed or passed to the worker.
11. EH passes backslashes unchanged and a ZIP `None` reaches `STATUS_UPLOADED` through per-image uploads.
12. No new config, dependency, migration, cleanup request, public interface, or queue/retry behavior appears.

- [ ] **Authorized commit checkpoint — orchestrator only**

Do not execute these commands without explicit user authorization. After authorization, inspect and stage only the intended files:

```powershell
git status --short --branch
git diff --check
git diff -- "docs/superpowers/plans/2026-07-20-ipfs3-zip-fallback-compatibility.md" "eh_client/src/telegraph.rs" "src/scheduler/eh_engine.rs"
git add -- "docs/superpowers/plans/2026-07-20-ipfs3-zip-fallback-compatibility.md" "eh_client/src/telegraph.rs" "src/scheduler/eh_engine.rs"
git diff --cached --check
git diff --cached --stat
git commit -m "fix: fall back from incompatible ipfs3 zip uploads" -m "Preflight ZIP compatibility and classify incomplete extraction results for per-image fallback."
git status --short --branch
```

Expected: one semantic `fix:` commit containing only the plan and two scoped Rust files; the post-commit worktree is clean.

- [ ] **Authorized push checkpoint — orchestrator only**

Do not execute without separate explicit push authorization. Verify the current branch and commit first, then push without force:

```powershell
git branch --show-current
git log -1 --oneline
git push -u origin fix/ipfs3-zip-fallback-compatibility
git status --short --branch
```

Expected: branch is `fix/ipfs3-zip-fallback-compatibility`, the new commit is the reviewed implementation commit, push succeeds without force, and local status reports clean/up to date.

## Plan self-review

- Spec and critic coverage: Task 1 covers physical central-directory uniqueness/count validation before `ZipArchive` deduplication, complete local-header/name parity, complete-archive and prefix compatibility, and zero-request fallback; Task 2 covers all `Some`/`None`/`Err` classifications; Task 3 covers raw-name preservation, corrected comments, and end-to-end EH fallback.
- Deferred markers: none; every task names concrete symbols, files, tests, commands, expected RED/GREEN evidence, and implementation control flow.
- Type consistency: new private helpers consistently use `ipfs3_zip_central_directory_is_complete_and_unique(archive_bytes: &[u8], central_directory_start: u64, archive_len: usize) -> bool`, `ipfs3_zip_local_header<'a>(bytes: &'a [u8], header_start: u64) -> Option<(u16, u16, &'a [u8])>`, and `ipfs3_zip_archive_is_compatible(archive_bytes: &[u8], requested_entry_names: &[String], extraction_prefix: &str) -> bool`. The changed private mapper is `ipfs3_zip_entry_cids(extraction_prefix: &str, entry_names: &[String], result: IpfS3ZipExtractResult) -> Result<Option<Vec<String>>>`; its sole production caller unwraps `Some` before mapping to `TelegraphImageUrlPair`, while public APIs remain unchanged.
- Scope consistency: only the plan plus `eh_client/src/telegraph.rs` and `src/scheduler/eh_engine.rs` are intended to change.
- Receipt status: `waiting for receipt`; the prior `[REJECT]` receipt is invalidated by this revision, and formal plan-critic redispatch and approval are owned by the orchestrator.
