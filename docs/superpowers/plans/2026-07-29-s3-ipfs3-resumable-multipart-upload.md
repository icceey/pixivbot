# S3/ipfS3 Resumable Multipart Upload Implementation Plan

> **For agentic workers:** Use the subagent-driven-development skill to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add crash-resumable, server-reconciled single-object multipart uploads for eligible S3/ipfS3 images and enabled ipfS3 ZIP extraction while preserving strict compatibility fallbacks and EH artifact lifecycle semantics.

**Architecture:** Add a private `eh_client::s3_multipart` module tree: `manifest.rs` owns credential-free session identity and atomic persistence, `list_parts.rs` owns signed paginated reconciliation and S3 error parsing, and `mod.rs` owns the sequential Create/List/Upload/Complete/Abort state machine. `telegraph.rs` remains the provider adapter for object keys, capability caches, PutObject fallbacks, public URLs/CIDs, gateway warmup, and ZIP result parsing; `EhUploadWorker` supplies stable per-archive resume paths and `ArchiveArtifacts` owns upload-state cleanup independently from download multipart state.

**Tech Stack:** Rust 1.94, Tokio, rust-s3 0.37.2, reqwest 0.12.28, quick-xml 0.38.4, serde/serde_json, tempfile 3, sha2 0.10.9, wiremock 0.6, SeaORM in-memory SQLite tests.

**Global Constraints:**
- Rust stays pinned to 1.94 and all dependency changes must remain compatible with the existing workspace and container build.
- Multipart image threshold defaults to 8 MiB for both S3 and ipfS3; `0` disables image multipart; comparison is inclusive and MiB conversion uses saturating arithmetic.
- Multipart part size is fixed at 8 MiB; uploads are sequential; at most 10,000 parts are allowed and the limit is checked before CreateMultipartUpload.
- `IpfS3UploaderConfig::zip_extract_enabled` remains an explicit operator opt-out and defaults to `true`; when false, ZIP-first upload is skipped.
- Resume is scoped to one ZIP or one image object; do not persist URLs for earlier images in a multi-image EH task.
- The local manifest stores no credentials, signed URLs, object bytes, or locally asserted completed-part ETags; ListParts is authoritative.
- Add only `sha2` to `eh_client`; do not add AWS SDK crates, a database migration, parallel part upload, task-level URL persistence, SSE, a general upload queue, or a server-side orphan sweeper.
- Do not use HTTP OPTIONS for capability discovery.
- Only explicit S3 codes `NotImplemented`, `UnsupportedOperation`, or operation-specific `MethodNotAllowed` may trigger multipart fallback; authentication, generic 4xx, network, timeout, malformed protocol, and 5xx failures remain errors.
- `decompress-zip` is added only to a dedicated CreateMultipartUpload bucket clone; UploadPart, ListParts, CompleteMultipartUpload, and AbortMultipartUpload always use the unmodified base bucket and carry no `decompress-*` query.
- A standard CompleteMultipartUpload XML result disables only ipfS3 multipart ZIP extraction; it must not disable standard/image multipart.
- Preserve existing ZIP preflight, strict `DecompressZipResult` parsing, requested archive order, per-entry CID semantics, preview/public gateway behavior, PutObject behavior, Telegraph splitting, and notification behavior.
- Download cleanup continues to remove only `.zip.part` and `.zip.parts`; active/retryable startup cleanup preserves `.zip.uploads`.
- Never read, print, copy, or commit `config.toml`; use only `config.toml.example` as the public configuration reference.
- All default tests stay offline with wiremock.
- Implementation subagents must not run `git add`, `git commit`, `git push`, `git tag`, or another git write. Suggested boundaries below are for the orchestrator to apply only after explicit user authorization.

---

## Approved source and current-state map

- Authoritative design: `docs/superpowers/specs/2026-07-29-s3-ipfs3-resumable-multipart-upload-design.md` at commit `5d02cea`.
- `eh_client/src/telegraph.rs:312-505` owns S3/ipfS3 config and `ImageUploadInput` / `ZipArchiveUploadInput`; `:728-796` and `:1185-1407` contain current single-Put S3, ipfS3 image, and ipfS3 ZIP paths.
- `eh_client/src/telegraph.rs:1410-1625` already strictly parses `DecompressZipResult`; keep that parser provider-side.
- `eh_client/src/archive_download/artifacts.rs:5-70` currently models `.zip`, `.zip.part`, and `.zip.parts`; `remove_multipart_state()` must retain its existing meaning.
- `src/scheduler/eh_engine.rs:1486-1595` creates ZIP/per-image uploader inputs; every image is currently uploaded in a one-element slice, so a per-attempt uploader index is not stable.
- `src/db/repo/eh_download_queue.rs:3332-3381` groups startup artifacts and preserves active final ZIPs while deleting stale download multipart state.
- rust-s3 0.37.2 provides `Bucket::initiate_multipart_upload`, `Bucket::complete_multipart_upload`, `Bucket::abort_upload`, `Bucket::head_object`, and `Bucket::presign_get`. Its `put_multipart_chunk` automatically aborts on non-2xx, so this plan uses low-level `ReqwestRequest` for UploadPart to preserve resumability and explicit fallback classification.
- `Cargo.lock` already contains `sha2 0.10.9` transitively; adding it to `eh_client/Cargo.toml` makes direct use explicit.

## Locked file map

| File | Responsibility |
|---|---|
| `eh_client/src/s3_multipart/mod.rs` | **Create.** Constants, shared request/outcome/capability types, credential-free fingerprint helpers, and the Create/reconcile/upload/Complete/Abort/HEAD state machine with colocated engine tests. |
| `eh_client/src/s3_multipart/manifest.rs` | **Create.** Versioned session schema, identity validation, atomic write/load/remove, and manifest tests. |
| `eh_client/src/s3_multipart/list_parts.rs` | **Create.** Short-lived presigned GET, pagination, ListParts XML validation, S3 `<Error>` parsing/classification, URL-sanitized transport errors, and tests. |
| `eh_client/src/lib.rs` | Register private `s3_multipart`; re-export only public resume context alongside existing uploader APIs. |
| `eh_client/src/telegraph.rs` | Add config defaults/threshold selection, public resume input, uploader capability state, shared-engine adapters, standard Complete parser, and preserve provider result/fallback behavior. Do not move multipart state-machine internals here. |
| `eh_client/src/archive_download/artifacts.rs` | Add `.zip.uploads`, family recognition, dedicated upload-state removal, and include it only in whole-family removal. |
| `eh_client/Cargo.toml` | Add direct `sha2 = "0.10.9"`. |
| `Cargo.lock` | Record `sha2` as a direct dependency of `eh_client` if Cargo changes the package dependency list. |
| `src/scheduler/eh_engine.rs` | Derive stable ZIP/image contexts, preserve uploadable archive order, and perform success/cancel/permanent/final cleanup. |
| `src/db/repo/eh_download_queue.rs` | Recognize `.zip.uploads`, preserve it for active/retryable rows, and remove it for genuine orphan families. |
| `config.toml.example` | Document defaults, `0`, lazy detection, exact fallback boundary, permissions, and Create-only ipfS3 ZIP extension behavior. |

No migration, entity, AWS SDK, Telegraph page model, notifier, or unrelated provider file is modified.

## Locked interfaces

Use these names and signatures consistently across tasks:

```rust
// eh_client/src/telegraph.rs — public caller-supplied context.
#[derive(Debug, Clone, Copy)]
pub struct UploadResumeContext<'a> {
    pub manifest_path: &'a std::path::Path,
    pub logical_object_id: &'a str,
}

pub struct ImageUploadInput<'a> {
    pub filename: &'a str,
    pub bytes: &'a [u8],
    pub resume_context: Option<UploadResumeContext<'a>>,
}

pub struct ZipArchiveUploadInput<'a> {
    pub filename: &'a str,
    pub bytes: &'a [u8],
    pub entry_names: &'a [String],
    pub resume_context: Option<UploadResumeContext<'a>>,
}
```

```rust
// eh_client/src/s3_multipart/mod.rs — all symbols remain pub(crate).
pub(crate) const PART_SIZE: usize = 8 * 1024 * 1024;
pub(crate) const MAX_PARTS: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProviderKind { S3, IpfS3 }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MultipartOperation { Create, ListParts, UploadPart, Complete, Abort, Head }

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CreateExtension {
    None,
    IpfS3DecompressZip { requested_entries_sha256: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HeadRecovery {
    S3ByLength,
    IpfS3ImageByLengthAndEtag,
    Never,
}

pub(crate) struct MultipartUploadRequest<'a> {
    pub provider: ProviderKind,
    pub uploader_identity_sha256: &'a str,
    pub candidate_object_key: String,
    pub logical_object_id: &'a str,
    pub bytes: &'a [u8],
    pub content_type: &'a str,
    pub manifest_path: Option<&'a std::path::Path>,
    pub create_extension: CreateExtension,
    pub head_recovery: HeadRecovery,
}

pub(crate) enum CompletionEvidence {
    Response(s3::request::ResponseData),
    RecoveredHead { etag: Option<String> },
}

pub(crate) struct CompletedUpload {
    pub object_key: String,
    pub evidence: CompletionEvidence,
    manifest_path: Option<std::path::PathBuf>,
}

impl CompletedUpload {
    pub(crate) async fn remove_manifest(&self) -> crate::Result<()>;
}

pub(crate) enum MultipartOutcome {
    Completed(CompletedUpload),
    Unsupported { operation: MultipartOperation },
}

pub(crate) async fn upload_multipart(
    bucket: &s3::Bucket,
    http: &reqwest::Client,
    request: MultipartUploadRequest<'_>,
) -> crate::Result<MultipartOutcome>;
```

`CompletedUpload::remove_manifest()` is called only after provider-specific response parsing and final URL/CID construction succeed. If Complete transport may have succeeded but parsing or manifest removal fails, the manifest remains available for the next ListParts/NoSuchUpload/HEAD decision.

```rust
// Process-local tri-state cache; AtomicU8 means no async lock is held across I/O.
#[derive(Debug, Default)]
pub(crate) struct MultipartCapability(std::sync::atomic::AtomicU8);

impl MultipartCapability {
    pub(crate) fn state(&self) -> CapabilityState;
    pub(crate) fn mark_supported(&self);
    pub(crate) fn mark_unsupported(&self);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CapabilityState { Unknown, Supported, Unsupported }

pub(crate) fn uploader_identity_fingerprint(
    provider: ProviderKind,
    endpoint: &str,
    region: &str,
    bucket: &str,
    path_style: bool,
) -> String;

pub(crate) fn requested_entries_fingerprint(entry_names: &[String]) -> String;
```

## Dependency order and safe parallel waves

1. **Wave 1 (parallel):** Task 1 (public config/input contract) and Task 2 (artifact family) touch disjoint production files except that only Task 1 owns `lib.rs`.
2. **Wave 2 (serial integration):** Task 3 creates the module/manifest contract; Task 4 then adds ListParts and updates the shared module declaration.
3. **Wave 3 (serial state machine):** Tasks 5, 6, and 7 build happy-path transfer, resume reconciliation, then replacement/HEAD/strict fallback behavior.
4. **Wave 4 (serial provider adapters):** Task 8 integrates S3/ipfS3 images; Task 9 adds ipfS3 ZIP multipart compatibility on the same `telegraph.rs` surface.
5. **Wave 5 (serial lifecycle):** Task 10 threads stable EH contexts and success cleanup; Task 11 completes cancellation/permanent/orphan cleanup and runs full verification.

The orchestrator must integrate and run each task's GREEN command before starting a dependent task. Tasks sharing `telegraph.rs`, `s3_multipart/mod.rs`, or `eh_engine.rs` are not safe for simultaneous file edits.

---

### Task 1: Lock configuration, threshold selection, and the public resume-input contract

**Files:**
- Modify/Test: `eh_client/src/telegraph.rs:312-505,2217-2445,2456-4257`
- Modify: `eh_client/src/lib.rs:12-18`
- Modify: `config.toml.example:84-132`

**Interfaces:**
- Consumes: existing `S3UploaderConfig`, `IpfS3UploaderConfig`, `ImageUploadInput<'a>`, `ZipArchiveUploadInput<'a>`, and every existing struct-literal call site found by `rg "ImageUploadInput \\{|ZipArchiveUploadInput \\{" eh_client src`.
- Produces: `UploadResumeContext<'a>` and the exact extended input signatures in **Locked interfaces**; `multipart_image_threshold_mb: u64` in both public configs and resolved configs; private `fn image_uses_multipart(byte_len: usize, threshold_mb: u64) -> bool`.

- [ ] **Step 1: Write failing default and threshold-boundary tests**

Add these focused tests to `telegraph.rs` before changing the structs:

```rust
#[test]
fn s3_and_ipfs3_multipart_defaults_are_explicit() {
    let s3 = S3UploaderConfig::default();
    let ipfs3 = IpfS3UploaderConfig::default();
    assert_eq!(s3.multipart_image_threshold_mb, 8);
    assert_eq!(ipfs3.multipart_image_threshold_mb, 8);
    assert!(s3.path_style);
    assert!(ipfs3.path_style);
    assert!(ipfs3.zip_extract_enabled);
}

#[test]
fn multipart_image_threshold_is_inclusive_zero_disables_and_conversion_saturates() {
    assert!(!image_uses_multipart(8 * 1024 * 1024 - 1, 8));
    assert!(image_uses_multipart(8 * 1024 * 1024, 8));
    assert!(!image_uses_multipart(usize::MAX, 0));
    assert_eq!(multipart_image_threshold_bytes(u64::MAX), Some(u64::MAX));
}

#[test]
fn serde_defaults_enable_ipfs3_zip_and_set_eight_mib_thresholds() {
    let s3: S3UploaderConfig = serde_json::from_value(serde_json::json!({})).unwrap();
    let ipfs3: IpfS3UploaderConfig = serde_json::from_value(serde_json::json!({})).unwrap();
    assert_eq!(s3.multipart_image_threshold_mb, 8);
    assert_eq!(ipfs3.multipart_image_threshold_mb, 8);
    assert!(ipfs3.zip_extract_enabled);
}
```

Replace the old `ipfs3_zip_extract_disabled_by_default` expectation with an enabled-by-default test and retain an explicit-false test that proves `supports_zip_archive_upload()` is false.

- [ ] **Step 2: Run the config tests and capture RED**

Run:

```powershell
cargo test -p eh_client --lib telegraph::tests::s3_and_ipfs3_multipart_defaults_are_explicit -- --exact
cargo test -p eh_client --lib telegraph::tests::multipart_image_threshold_is_inclusive_zero_disables_and_conversion_saturates -- --exact
cargo test -p eh_client --lib telegraph::tests::serde_defaults_enable_ipfs3_zip_and_set_eight_mib_thresholds -- --exact
```

Expected: compilation fails because the threshold fields/helpers do not exist and the current derived `Default` leaves `zip_extract_enabled` false.

- [ ] **Step 3: Implement explicit defaults and saturating threshold selection**

Remove derived `Default` from the two configs, add the field to both public and resolved structs, and use these exact helpers:

```rust
const DEFAULT_MULTIPART_IMAGE_THRESHOLD_MB: u64 = 8;

fn default_multipart_image_threshold_mb() -> u64 {
    DEFAULT_MULTIPART_IMAGE_THRESHOLD_MB
}

fn default_ipfs3_zip_extract_enabled() -> bool {
    true
}

fn multipart_image_threshold_bytes(threshold_mb: u64) -> Option<u64> {
    (threshold_mb != 0).then(|| threshold_mb.saturating_mul(1024 * 1024))
}

fn image_uses_multipart(byte_len: usize, threshold_mb: u64) -> bool {
    multipart_image_threshold_bytes(threshold_mb).is_some_and(|threshold| {
        u64::try_from(byte_len).unwrap_or(u64::MAX) >= threshold
    })
}
```

Annotate both threshold fields with `#[serde(default = "default_multipart_image_threshold_mb")]`, annotate `zip_extract_enabled` with `#[serde(default = "default_ipfs3_zip_extract_enabled")]`, and implement `Default` explicitly so Rust construction and deserialization agree. Existing defaults remain: optional strings `None`, prefixes empty, path style true, preview delay 600, and gateway warmup false.

- [ ] **Step 4: Extend uploader inputs and update every existing caller**

Add `UploadResumeContext` and the two `resume_context` fields exactly as shown in **Locked interfaces**. Re-export `UploadResumeContext` from `eh_client/src/lib.rs`.

Add `resume_context: None` to every existing direct/test struct literal in `eh_client/src/telegraph.rs` and `src/scheduler/eh_engine.rs`; Task 10 replaces the two scheduler `None` values with stable contexts. Do not add constructors that hide the context.

Update `complete_s3_config()` and `complete_ipfs3_config()` fixtures with threshold 8 and set ZIP false only in tests specifically exercising opt-out.

- [ ] **Step 5: Document the public configuration contract**

In both S3 and ipfS3 example blocks, add `multipart_image_threshold_mb = 8` comments that state `0` disables image multipart, exactly-threshold images use multipart, and smaller images retain PutObject. Replace the stale ZIP “leave disabled” text with default-enabled/operator-opt-out text.

Add one shared comment paragraph covering:

```text
Multipart support is detected lazily on a real eligible object. The endpoint needs
CreateMultipartUpload, UploadPart, ListParts (s3:ListMultipartUploadParts on AWS),
CompleteMultipartUpload, AbortMultipartUpload, and HEAD for ambiguous lost Complete
responses. Only explicit unsupported-operation errors use PutObject fallback;
permission/authentication and transient failures are reported instead of downgraded.
```

For ipfS3, state that `decompress-zip` is signed only on multipart Create and never on UploadPart/ListParts/Complete/Abort.

- [ ] **Step 6: Run GREEN and a compile-surface check**

Run:

```powershell
cargo test -p eh_client --lib telegraph::tests::s3_and_ipfs3_multipart_defaults_are_explicit -- --exact
cargo test -p eh_client --lib telegraph::tests::multipart_image_threshold_is_inclusive_zero_disables_and_conversion_saturates -- --exact
cargo test -p eh_client --lib telegraph::tests::serde_defaults_enable_ipfs3_zip_and_set_eight_mib_thresholds -- --exact
cargo check -p eh_client --all-targets
git diff --check -- config.toml.example
```

Expected: all three tests pass, all old input literals compile with explicit `None`, and the example has no whitespace errors.

- [ ] **Step 7: Stop at the review boundary; do not commit**

Suggested orchestrator commit after authorization: `feat(eh_client): configure resumable multipart uploads`.

---

### Task 2: Give upload state an independent archive-artifact lifecycle

**Files:**
- Modify/Test: `eh_client/src/archive_download/artifacts.rs:5-150`

**Interfaces:**
- Consumes: existing `ArchiveArtifacts::{new,from_member,remove_multipart_state,remove_all}`.
- Produces: `pub fn uploads_dir(&self) -> &Path`, `pub async fn remove_upload_state(&self) -> Result<()>`; `.zip.uploads` recognition; unchanged `remove_multipart_state()` semantics.

- [ ] **Step 1: Write failing derivation and isolation tests**

Extend the existing artifact tests:

```rust
#[tokio::test]
async fn upload_state_has_an_independent_lifecycle() {
    let temp = tempfile::tempdir().unwrap();
    let artifacts = ArchiveArtifacts::new(temp.path().join("gallery.zip"));
    tokio::fs::write(artifacts.final_zip(), b"zip").await.unwrap();
    tokio::fs::write(artifacts.assembly_scratch(), b"partial").await.unwrap();
    tokio::fs::create_dir_all(artifacts.parts_dir()).await.unwrap();
    tokio::fs::create_dir_all(artifacts.uploads_dir()).await.unwrap();
    tokio::fs::write(artifacts.uploads_dir().join("archive.json"), b"manifest")
        .await
        .unwrap();

    artifacts.remove_multipart_state().await.unwrap();
    assert!(artifacts.final_zip().exists());
    assert!(artifacts.uploads_dir().exists());
    assert!(!artifacts.assembly_scratch().exists());
    assert!(!artifacts.parts_dir().exists());

    artifacts.remove_upload_state().await.unwrap();
    artifacts.remove_upload_state().await.unwrap();
    assert!(!artifacts.uploads_dir().exists());
    assert!(artifacts.final_zip().exists());
}
```

Also add `.zip.uploads` to `archive_artifacts_derive_stable_family_paths_and_members` and add it to `archive_artifacts_remove_all_recursively_and_idempotently`.

- [ ] **Step 2: Run RED**

Run:

```powershell
cargo test -p eh_client --lib archive_download::artifacts::tests::upload_state_has_an_independent_lifecycle -- --exact
```

Expected: compilation fails because `uploads_dir()` and `remove_upload_state()` do not exist.

- [ ] **Step 3: Implement the fourth family member without changing download cleanup**

Add `uploads_dir: PathBuf`, derive it with `final_zip.with_extension("zip.uploads")`, recognize the `.zip.uploads` suffix in `from_member()`, and implement:

```rust
pub fn uploads_dir(&self) -> &Path {
    &self.uploads_dir
}

pub async fn remove_upload_state(&self) -> Result<()> {
    remove_dir_if_present(&self.uploads_dir).await
}
```

Keep `remove_multipart_state()` byte-for-byte equivalent in responsibility: it removes only assembly scratch and download parts. Extend `remove_all()` to attempt final ZIP, assembly scratch, parts directory, and uploads directory before returning the first error.

- [ ] **Step 4: Run GREEN and adjacent artifact tests**

Run:

```powershell
cargo test -p eh_client --lib archive_download::artifacts::tests -- --nocapture
cargo test -p eh_client --lib archive_download::manifest::tests -- --nocapture
```

Expected: artifact tests prove isolation/idempotence and existing download manifest tests remain green.

- [ ] **Step 5: Stop at the review boundary; do not commit**

Suggested orchestrator commit after authorization: `feat(eh_client): isolate archive upload state`.

---

### Task 3: Persist credential-free multipart session identity atomically

**Files:**
- Create/Test: `eh_client/src/s3_multipart/manifest.rs`
- Create: `eh_client/src/s3_multipart/mod.rs`
- Modify: `eh_client/src/lib.rs:1-7`
- Modify: `eh_client/Cargo.toml:9-25`
- Modify: `Cargo.lock`

**Interfaces:**
- Consumes: `ProviderKind`, fixed `PART_SIZE`, optional manifest path from Task 1, and the tempfile flush/`sync_all`/persist pattern in `archive_download/manifest.rs:119-146`.
- Produces: `MultipartManifest`, `ManifestIdentity`, `ManifestLoad`, `load_manifest`, `write_manifest_atomic`, `remove_manifest`, `sha256_hex`, `fingerprint_fields`, and `requested_entries_fingerprint` for Tasks 4-10.

- [ ] **Step 1: Write failing manifest round-trip, invalidation, and secret-exclusion tests**

Create `manifest.rs` tests that use this schema and API:

```rust
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

pub(super) struct ManifestIdentity<'a> {
    pub(super) provider: ProviderKind,
    pub(super) uploader_identity_sha256: &'a str,
    pub(super) logical_object_id: &'a str,
    pub(super) object_sha256: &'a str,
    pub(super) object_len: u64,
    pub(super) content_type: &'a str,
    pub(super) requested_entries_sha256: Option<&'a str>,
}
```

Required tests and assertions:

- `manifest_round_trip_is_atomic_and_contains_only_session_identity`: write a complete manifest twice to the same `archive.json`, load it, and assert `ManifestLoad::Valid(replacement.clone())`; decode the persisted JSON back to `MultipartManifest`; assert its text contains `upload-2` and the expected 64-character object hash but contains none of `AKIA_TEST`, `secret-test-value`, `X-Amz-`, a unique object-byte sentinel, or `"etag"`; enumerate the parent and assert its only entry is `archive.json`.
- `manifest_identity_rejects_provider_uploader_logical_object_content_and_zip_changes`: begin with one valid manifest and independently mutate provider, uploader hash, logical ID, object hash, object length, content type, part size, ZIP prefix, and ordered-entry hash; assert the exact corresponding `ManifestMismatch` each time.
- `fingerprints_are_length_delimited_deterministic_and_credential_free`: assert `fingerprint_fields(&["ab", "c"]) != fingerprint_fields(&["a", "bc"])`, repeated calls are equal, the result has 64 lowercase hexadecimal characters, and a fingerprint built without credential arguments cannot contain supplied credential sentinel strings.

- [ ] **Step 2: Run RED**

Run:

```powershell
cargo test -p eh_client --lib s3_multipart::manifest::tests -- --nocapture
```

Expected: compilation fails because the private module and manifest API do not exist.

- [ ] **Step 3: Register the private module and add SHA-256 helpers**

Add `mod s3_multipart;` to `eh_client/src/lib.rs` and `sha2 = "0.10.9"` to normal dependencies. In `mod.rs`, declare `mod manifest;`, constants, `ProviderKind`, and length-delimited hashing:

```rust
pub(crate) fn fingerprint_fields(fields: &[&str]) -> String {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    for field in fields {
        hash.update(u64::try_from(field.len()).unwrap_or(u64::MAX).to_be_bytes());
        hash.update(field.as_bytes());
    }
    format!("{:x}", hash.finalize())
}
```

`sha256_hex(bytes)` hashes raw object bytes. `requested_entries_fingerprint(entry_names)` hashes ordered names with the same length-prefix rule, so order and boundaries are significant. `uploader_identity_fingerprint(provider, endpoint, region, bucket, path_style)` delegates to `fingerprint_fields` with exactly those normalized values and has no credential parameters.

- [ ] **Step 4: Implement closed manifest recovery states and validation**

Use these exact recovery categories:

```rust
pub(super) enum ManifestLoad {
    Missing,
    Valid(MultipartManifest),
    Stale { manifest: MultipartManifest, reason: ManifestMismatch },
    MalformedJson,
}

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
```

`ManifestIdentity` contains provider, uploader fingerprint, logical ID, object SHA-256/length, content type, and optional ordered-entry fingerprint. Validation also requires non-empty object key/upload ID, exact fixed part size, and this ZIP invariant:

```rust
let expected_prefix = manifest.object_key
    .strip_suffix(".zip")
    .map(|stem| format!("{stem}/"));
```

For ZIP mode, both stored prefix and requested-entry fingerprint must match; for standard mode both must be absent. I/O errors remain `Err` and are not converted to purgeable states.

- [ ] **Step 5: Implement atomic write and idempotent exact-file removal**

`write_manifest_atomic(path, manifest)` must create only the parent directory, serialize pretty JSON, create a tempfile in that parent with prefix `manifest.json.tmp-`, write all bytes, flush, `sync_all`, and `persist(path)` inside `spawn_blocking`. `remove_manifest(path)` removes only that JSON file and ignores NotFound; it does not remove sibling image manifests or the `.zip.uploads` directory.

Expose a constructor that derives ZIP prefix from the chosen stable object key:

```rust
pub(super) async fn load_manifest(
    path: &std::path::Path,
    identity: &ManifestIdentity<'_>,
) -> crate::Result<ManifestLoad>;

pub(super) fn new_manifest(
    identity: &ManifestIdentity<'_>,
    object_key: String,
    upload_id: String,
) -> crate::Result<MultipartManifest>;

pub(super) async fn write_manifest_atomic(
    path: &std::path::Path,
    manifest: &MultipartManifest,
) -> crate::Result<()>;

pub(super) async fn remove_manifest(path: &std::path::Path) -> crate::Result<()>;
```

- [ ] **Step 6: Run GREEN and dependency checks**

Run:

```powershell
cargo test -p eh_client --lib s3_multipart::manifest::tests -- --nocapture
cargo check -p eh_client --all-targets
```

Expected: all manifest tests pass; direct `sha2` use compiles; no secret-bearing value is serialized.

- [ ] **Step 7: Stop at the review boundary; do not commit**

Suggested orchestrator commit after authorization: `feat(eh_client): persist multipart upload sessions`.

---

### Task 4: Implement paginated, signed, and sanitized ListParts reconciliation

**Files:**
- Create/Test: `eh_client/src/s3_multipart/list_parts.rs`
- Modify: `eh_client/src/s3_multipart/mod.rs`

**Interfaces:**
- Consumes: `MultipartOperation` from the locked module contract; `Bucket::presign_get(path, expiry_secs, Some(query))`; reqwest `Error::without_url`; quick-xml serde support.
- Produces: `CompletedPart { part_number: u32, etag: String, size: u64 }`, `MultipartFailure`, `list_all_parts`, and shared strict S3 response/error classification used by Task 5, including HTTP 200 Complete responses whose XML root is `<Error>`.

- [ ] **Step 1: Write failing XML, pagination, and sanitization tests**

Create these offline tests with the stated exact assertions:

- `list_parts_follows_markers_and_preserves_quoted_etags`: mount two XML pages, call `list_all_parts`, assert returned part numbers `[1, 2, 3]`, assert the first literal ETag remains `"cid-one"`, and inspect received requests to assert markers `0` then `2`.
- `list_parts_rejects_bucket_key_upload_id_marker_duplicates_empty_etags_and_nonprogress`: run one fixture per mismatched identity/pagination value; assert identity/pagination failures are `Protocol`, while duplicate number, number 0/10001, and whitespace ETag are `InvalidInventory`.
- `list_parts_classifies_only_explicit_codes_as_unsupported`: feed `NotImplemented`, `UnsupportedOperation`, XML `MethodNotAllowed`, bare 405, `AccessDenied`, and `NoSuchUpload`; assert the first three are `Unsupported`, bare 405/auth are `Service`, and the final code is `NoSuchUpload`.
- `list_parts_transport_errors_strip_presigned_url_and_credentials`: force a connection close after signing, render the returned error, and assert it contains none of the wiremock URL, `X-Amz-`, `Credential`, `Signature`, the concrete object-key sentinel, access-key sentinel, or secret sentinel.

The pagination fixture must return page 1 with `IsTruncated=true`, `NextPartNumberMarker=2`, and parts 1/2, then page 2 with marker 2 and part 3. Assert every request has `uploadId`, `part-number-marker`, `max-parts=1000`, SigV4 query fields, and no `decompress-*` query.

- [ ] **Step 2: Run RED**

Run:

```powershell
cargo test -p eh_client --lib s3_multipart::list_parts::tests -- --nocapture
```

Expected: compilation fails because `list_parts` and its types are undefined.

- [ ] **Step 3: Define strict service/protocol failure types**

Add `mod list_parts;` and implement:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CompletedPart {
    pub(super) part_number: u32,
    pub(super) etag: String,
    pub(super) size: u64,
}

pub(super) enum MultipartFailure {
    Unsupported { operation: MultipartOperation, status: u16, code: String },
    NoSuchUpload { operation: MultipartOperation },
    InvalidInventory(String),
    Service { operation: MultipartOperation, status: u16, code: Option<String> },
    Protocol(String),
    Client(crate::Error),
}

pub(super) async fn list_all_parts(
    http: &reqwest::Client,
    bucket: &s3::Bucket,
    key: &str,
    upload_id: &str,
) -> std::result::Result<Vec<CompletedPart>, MultipartFailure>;

pub(super) fn classify_response(
    operation: MultipartOperation,
    status: u16,
    body: &[u8],
) -> std::result::Result<(), MultipartFailure>;

pub(super) fn classify_embedded_s3_error(
    operation: MultipartOperation,
    status: u16,
    body: &[u8],
) -> Option<MultipartFailure>;

pub(super) fn classify_s3_error(
    operation: MultipartOperation,
    error: s3::error::S3Error,
) -> MultipartFailure;
```

Deserialize ListParts with an internal `ListPartsResult { bucket, key, upload_id, part_number_marker, next_part_number_marker, max_parts, is_truncated, parts }`, where each part is `{ part_number, etag, size }` and fields use their exact AWS XML names. Deserialize errors with `S3ErrorBody { code, message }` under root `Error`.

Parse only a structurally valid `<Error><Code>…</Code><Message>…</Message></Error>` as an S3 code. `MethodNotAllowed` is unsupported only when that exact parsed code is present for Create/ListParts/UploadPart/Complete; a bare 405 is `Service`. `NoSuchUpload` is its own category. `classify_s3_error` matches `S3Error::HttpFailWithBody` through the same body classifier, converts `S3Error::Reqwest(error)` to `Error::Http(error.without_url())`, and wraps other non-service client errors with operation plus their safe display text. Never include service response body, signed URL, access key, signature, or credentials in a displayed error; retain operation/status/code only.

`classify_embedded_s3_error` inspects only enough XML to identify the root. A non-`Error` root returns `None` without imposing provider-specific success parsing. An `Error` root must parse strictly as `S3ErrorBody` and returns the same `Unsupported`/`NoSuchUpload`/`Service` classification even when HTTP status is 200; a malformed document that has already identified `Error` as its root returns `Protocol`. This is required because CompleteMultipartUpload may report a failure inside an HTTP 200 response.

- [ ] **Step 4: Implement signed page fetching without retaining the URL**

Use a 60-second expiry and a fresh local query map for every page:

```rust
let query = std::collections::HashMap::from([
    ("uploadId".to_owned(), upload_id.to_owned()),
    ("part-number-marker".to_owned(), marker.to_string()),
    ("max-parts".to_owned(), "1000".to_owned()),
]);
let signed_url = bucket.presign_get(key, 60, Some(query)).await
    .map_err(|_| MultipartFailure::Client(crate::Error::Other(
        format!("failed to presign {operation:?} request"),
    )))?;
let response = http.get(&signed_url).send().await
    .map_err(|error| MultipartFailure::Client(crate::Error::Http(error.without_url())))?;
drop(signed_url);
```

Read status/body, classify non-2xx first, then deserialize `ListPartsResult`. Do not trace the URL or response body.

- [ ] **Step 5: Validate every page and aggregate globally**

Require exact response bucket (`bucket.name`), key, upload ID, requested marker, and `MaxParts=1000`; require each part number in `1..=10_000`, non-empty trimmed ETag, globally unique part number, and a strictly progressing `NextPartNumberMarker` in `1..=10_000` when truncated. Classify duplicate/out-of-range/empty-ETag inventory as `InvalidInventory` so Task 7 can abort and replace the session; classify bucket/key/upload-ID/pagination inconsistency as `Protocol`, which remains an error. Do not impose a page-count limit: an S3-compatible service may return fewer than 1,000 parts per page. Termination is guaranteed by strict marker progress, the 10,000 marker/part-number ceiling, and global uniqueness. Return parts sorted by number; size-vs-object validation remains Task 6's engine responsibility.

Add `list_parts_accepts_more_than_ten_small_pages`: serve 11 one-part pages total. Requests with markers 0 through 9 receive truncated pages whose next markers are 1 through 10; the request with marker 10 receives the non-truncated page containing part 11. Assert all 11 parts are returned. This prevents reintroducing a fixed ten-page assumption.

- [ ] **Step 6: Run GREEN**

Run:

```powershell
cargo test -p eh_client --lib s3_multipart::list_parts::tests -- --nocapture
```

Expected: pagination and validation pass; unsupported codes are strict; sanitized error strings contain neither `X-Amz-`, `Credential`, `Signature`, access key, nor the request URL.

- [ ] **Step 7: Stop at the review boundary; do not commit**

Suggested orchestrator commit after authorization: `feat(eh_client): reconcile multipart parts with s3`.

---

### Task 5: Build the standard sequential multipart happy path

**Files:**
- Modify/Test: `eh_client/src/s3_multipart/mod.rs`
- Consume unchanged: `eh_client/src/s3_multipart/manifest.rs`
- Consume unchanged: `eh_client/src/s3_multipart/list_parts.rs`

**Interfaces:**
- Consumes: manifest/ListParts APIs from Tasks 3-4, rust-s3 `initiate_multipart_upload`, low-level `ReqwestRequest`, `Command::PutObject` + `Multipart`, `complete_multipart_upload`, and `abort_upload`.
- Produces: the exact `MultipartUploadRequest`, `CompletionEvidence`, `CompletedUpload`, `MultipartOutcome`, and `upload_multipart` contract locked above; a complete fresh-session happy path.

- [ ] **Step 1: Write failing preflight and happy-path wiremock tests**

Add four concrete tests:

- `multipart_rejects_more_than_ten_thousand_parts_before_create`: call the pure `part_count(MAX_PARTS * PART_SIZE + 1)`, assert the exact 10,000-limit error, and mount a catch-all HTTP mock with expected count zero.
- `fresh_multipart_creates_lists_uploads_sequential_parts_and_completes_sorted`: use `2 * PART_SIZE + 17` deterministic bytes and inspect all received requests as described below.
- `ipfs_cid_etags_are_xml_escaped_only_when_complete_is_serialized`: return UploadPart ETags `"bafy&one"` and `"bafy<two>"`, parse the captured Complete XML with quick-xml, and assert decoded values equal those exact literals and appear in part-number order.
- `manifest_persistence_failure_aborts_created_session_before_uploading_parts`: create a regular file at the would-be manifest parent path, use `<that-file>/archive.json` as the manifest path so `create_dir_all` fails reliably on Windows and POSIX, then assert Create succeeds, exactly one Abort is sent through the unmodified base Bucket, no UploadPart/Complete is sent, the persistence error is returned, and no manifest exists.

The happy test uses `2 * PART_SIZE + 17` bytes, asserts exactly one Create POST, one empty ListParts GET immediately after Create, PUT parts 1/2/3 in order, and one Complete POST whose parsed XML contains parts `[1,2,3]`. It also asserts no OPTIONS request and no `decompress-*` query.

- [ ] **Step 2: Run RED**

Run:

```powershell
cargo test -p eh_client --lib s3_multipart::tests::fresh_multipart_creates_lists_uploads_sequential_parts_and_completes_sorted -- --exact
```

Expected: compilation fails because the engine contract is not implemented.

- [ ] **Step 3: Define engine types and deterministic partitioning**

Implement all locked types plus:

```rust
fn part_count(len: usize) -> crate::Result<usize> {
    if len == 0 {
        return Err(crate::Error::Other("multipart upload object is empty".into()));
    }
    let count = len.div_ceil(PART_SIZE);
    if count > MAX_PARTS {
        return Err(crate::Error::Other(format!(
            "multipart upload needs {count} parts, exceeding the 10000-part limit"
        )));
    }
    Ok(count)
}

fn expected_part_size(object_len: usize, part_number: u32) -> Option<usize> {
    let part_number = usize::try_from(part_number).ok()?;
    let start = part_number.checked_sub(1)?.checked_mul(PART_SIZE)?;
    (start < object_len).then(|| (object_len - start).min(PART_SIZE))
}
```

Part numbers are one-based; every non-final slice is exactly `PART_SIZE`; the final slice is `1..=PART_SIZE`.

- [ ] **Step 4: Create the actual session and persist it before any part upload**

For a fresh object, choose `candidate_object_key`, clone the base bucket only when `CreateExtension::IpfS3DecompressZip`, add only `decompress-zip=<object-key-without-.zip>/` to that clone, and call `initiate_multipart_upload`. Validate returned key equals the chosen key and upload ID is non-empty.

When `manifest_path` is present, call `write_manifest_atomic` immediately. If persistence fails, call Abort on the unmodified base bucket as a best-effort cleanup and return the persistence error. Without a context, retain the session only in memory and abort it on any subsequent error because it cannot be resumed across calls.

Immediately call `list_all_parts`; an empty result establishes provisional standard support and the same session continues.

- [ ] **Step 5: Upload parts through a non-auto-aborting low-level request**

Do not use `Bucket::put_multipart_chunk` or `response_data(true)`. Use one raw `ReqwestRequest::response()` call so there is no rust-s3 retry, no automatic Abort, and no loss of a non-2xx S3 XML body:

```rust
use s3::request::Request;

let command = s3::command::Command::PutObject {
    content: chunk,
    content_type,
    custom_headers: None,
    multipart: Some(s3::command::Multipart::new(part_number, upload_id)),
};
let request = s3::request::tokio_backend::ReqwestRequest::new(bucket, key, command).await?;
let response = request.response().await.map_err(|error| {
    list_parts::classify_s3_error(MultipartOperation::UploadPart, error)
})?;
```

Read status and clone the literal ETag header before consuming any body. For non-2xx, consume the original response bytes once and classify their S3 XML through Task 4's shared parser. For success, require a non-empty UTF-8 `ETag` header and preserve its literal quoted/opaque value. Sanitize `reqwest::Error` from both sending and body reads with `without_url()`. Do not trim quotes or require MD5 shape. The response-loss test must prove there is exactly one PUT from the engine call; retry happens only when the outer EH attempt invokes the engine again and ListParts reconciles the server.

- [ ] **Step 6: Complete with sorted literal ETags and return unfinished provider evidence**

Sort by part number. Before constructing each `s3::serde_types::Part`, XML-escape the literal ETag exactly once with `quick_xml::escape::escape`; rust-s3's `Part` formatter then emits safe XML. Call `complete_multipart_upload` on the unmodified base bucket and require 2xx. Before returning success evidence, call `classify_embedded_s3_error(MultipartOperation::Complete, status, response.bytes())`; route an HTTP 200 `<Error>` through the same unsupported/service/NoSuchUpload handling as a non-2xx S3 error. Do not otherwise parse, normalize, or reject the successful body in the engine.

Return `MultipartOutcome::Completed(CompletedUpload { object_key, evidence: Response(response), manifest_path })` without deleting the manifest. Implement `CompletedUpload::remove_manifest()` as the exact-file removal from Task 3.

- [ ] **Step 7: Run GREEN and inspect the real HTTP surface**

Run:

```powershell
cargo test -p eh_client --lib s3_multipart::tests::multipart_rejects_more_than_ten_thousand_parts_before_create -- --exact
cargo test -p eh_client --lib s3_multipart::tests::fresh_multipart_creates_lists_uploads_sequential_parts_and_completes_sorted -- --exact
cargo test -p eh_client --lib s3_multipart::tests::ipfs_cid_etags_are_xml_escaped_only_when_complete_is_serialized -- --exact
cargo test -p eh_client --lib s3_multipart::tests::manifest_persistence_failure_aborts_created_session_before_uploading_parts -- --exact
```

Expected: tests pass with one session, deterministic sequential requests, valid sorted Complete XML, no OPTIONS, no extension query on later operations, and a Create whose unpersistable session is aborted before any part transfer.

- [ ] **Step 8: Stop at the review boundary; do not commit**

Suggested orchestrator commit after authorization: `feat(eh_client): add standard multipart transfer engine`.

---

### Task 6: Resume from server-authoritative parts after failure or restart

**Files:**
- Modify/Test: `eh_client/src/s3_multipart/mod.rs`
- Consume unchanged: `eh_client/src/s3_multipart/manifest.rs`
- Consume unchanged: `eh_client/src/s3_multipart/list_parts.rs`

**Interfaces:**
- Consumes: valid stored `MultipartManifest`, sorted `Vec<CompletedPart>`, deterministic partitioning, and `CompletedUpload` from Task 5.
- Produces: valid-manifest reuse, exact server-part validation, missing-part upload, preserved manifests on retryable failures, and process-reconstruction resume behavior.

- [ ] **Step 1: Write the interrupted-three-part acceptance test**

The test must:

1. Use `2 * PART_SIZE + 17` bytes and a temp manifest path.
2. First call: Create/ListParts succeeds, parts 1/2 return ETags, part 3 connection fails; assert the call errors and manifest remains.
3. Reconstruct both `Bucket` and client-facing engine inputs.
4. Second call: ListParts returns parts 1/2 with exact sizes; assert no Create request and no PUT for parts 1/2, exactly one PUT for part 3, then Complete.
5. Assert `CompletedUpload.object_key` is the manifest's original key even though the second call supplies a different timestamp-shaped candidate key.
6. Call `remove_manifest()` and assert only that JSON disappears.

Name it `multipart_restart_lists_server_parts_and_uploads_only_the_missing_tail`.

- [ ] **Step 2: Write response-loss and stale-local-claim tests**

Add `accepted_part_with_lost_response_is_not_retransmitted_when_listed`: count a first-call PUT whose response closes, then have retry ListParts report that part and assert its total PUT count remains one. Add `local_manifest_never_skips_a_part_absent_from_list_parts`: seed only session identity in the manifest, return an empty ListParts response, and assert every deterministic part is PUT; the manifest has no ETag field that can suppress transfer.

The first call's PUT responder records the body and closes the response; the retry ListParts includes its ETag/size. Assert one total PUT for that part across calls.

- [ ] **Step 3: Run RED**

Run:

```powershell
cargo test -p eh_client --lib s3_multipart::tests::multipart_restart_lists_server_parts_and_uploads_only_the_missing_tail -- --exact
cargo test -p eh_client --lib s3_multipart::tests::accepted_part_with_lost_response_is_not_retransmitted_when_listed -- --exact
```

Expected: current engine always starts fresh or uploads every part, so request-count assertions fail.

- [ ] **Step 4: Load and validate before deciding the object key**

Build `ManifestIdentity` from request provider/uploader/logical ID/content SHA-256/length/content type/fixed part size/ZIP entry fingerprint. For `ManifestLoad::Valid`, use stored object key/upload ID and never use the candidate key. For `Missing` or `MalformedJson`, create a fresh session; remove malformed JSON before Create.

For `Stale` with the same uploader identity and a readable key/upload ID, Task 7 decides safe abort/replacement. For an uploader-identity mismatch, remove the local manifest without sending Abort to the current endpoint.

- [ ] **Step 5: Treat ListParts as the sole completed-part authority**

Validate every listed part against deterministic partitioning:

```rust
fn reconcile_parts(
    object_len: usize,
    listed: Vec<CompletedPart>,
) -> std::result::Result<std::collections::BTreeMap<u32, String>, ReconcileInvalid>;
```

Reject duplicate (defense in depth), zero/out-of-range number, empty ETag, non-final size other than exactly 8 MiB, and final size not equal to the exact remaining bytes. Use a `BTreeMap` so Complete order is strict. Upload every absent part; replace the map value with the ETag returned by any re-upload.

- [ ] **Step 6: Preserve resumable state on retryable transfer failures**

When a manifest path exists, network errors, timeouts, 5xx, authentication errors, malformed XML, and missing successful UploadPart ETags return an error without aborting or deleting the valid manifest. Without a context, best-effort Abort before returning because no next call can recover the session.

- [ ] **Step 7: Run GREEN plus pagination integration**

Run:

```powershell
cargo test -p eh_client --lib s3_multipart::tests::multipart_restart_lists_server_parts_and_uploads_only_the_missing_tail -- --exact
cargo test -p eh_client --lib s3_multipart::tests::accepted_part_with_lost_response_is_not_retransmitted_when_listed -- --exact
cargo test -p eh_client --lib s3_multipart::tests::local_manifest_never_skips_a_part_absent_from_list_parts -- --exact
cargo test -p eh_client --lib s3_multipart::list_parts::tests::list_parts_follows_markers_and_preserves_quoted_etags -- --exact
```

Expected: only missing/uncertain parts transfer, original object identity survives reconstruction, and paginated ETags feed sorted Complete.

- [ ] **Step 8: Stop at the review boundary; do not commit**

Suggested orchestrator commit after authorization: `feat(eh_client): resume multipart uploads from list parts`.

---

### Task 7: Enforce strict capability fallback, one replacement, and lost-Complete HEAD recovery

**Files:**
- Modify/Test: `eh_client/src/s3_multipart/mod.rs`
- Modify/Test: `eh_client/src/s3_multipart/list_parts.rs`

**Interfaces:**
- Consumes: `MultipartFailure`, valid/stale manifests, `HeadRecovery`, and active session IDs from Tasks 3-6.
- Produces: `MultipartCapability`, strict `Unsupported` outcomes, replacement budget, invalid-session cleanup, and provider-specific HEAD evidence.

- [ ] **Step 1: Write table-driven strict-classification tests**

Cover Create, ListParts, UploadPart, and Complete separately. For each operation, explicit XML codes `NotImplemented`, `UnsupportedOperation`, and `MethodNotAllowed` must produce `MultipartOutcome::Unsupported { operation }`; active sessions must receive one Abort and the manifest must be removed. Complete must cover both a normal non-2xx S3 error response and HTTP 200 with root `<Error>`.

Apply malformed-success tests at the layer that owns each protocol: Create and ListParts test malformed 2xx XML in the shared layer; UploadPart tests missing, non-UTF-8, and whitespace-only ETag while accepting an arbitrary unused success body; Complete passes every non-`Error` 2xx body through as raw `CompletionEvidence`, with malformed/wrong-root/trailing provider results rejected by Tasks 8-9. For each operation, test raw 400/403/405, `AccessDenied`, `InvalidAccessKeyId`, 500/503, malformed rooted `<Error>`, and timeout/connection reset. Add HTTP 200 `<Error><Code>AccessDenied</Code>…</Error>` for Complete and assert it is a service error, not provider evidence or fallback. Every case must return `Err`, must not return `Unsupported`, must not send PutObject fallback (provider tests add that assertion in Tasks 8-9), and must leave a resumable valid manifest when a session exists.

Name the aggregate tests:

```rust
multipart_only_classifies_explicit_operation_codes_as_unsupported
multipart_transient_auth_and_malformed_failures_never_become_unsupported
multipart_complete_classifies_http_200_error_roots_before_provider_parsing
multipart_never_sends_options
```

- [ ] **Step 2: Write replacement-budget tests**

Add tests for:

- valid manifest + ListParts `NoSuchUpload` + confirmed missing HEAD -> one new Create;
- second `NoSuchUpload` in the same call -> error and no third session;
- `InvalidInventory` duplicate/out-of-range/empty-ETag parts and engine-detected wrong-size parts -> Abort old, clear manifest, one replacement;
- malformed local JSON -> remove local file and create one session without attempting an unknown remote Abort;
- stale same-uploader manifest after content change -> Abort old key/upload ID before one replacement;
- stale uploader-identity manifest -> no Abort to the new endpoint, remove local file, one fresh Create.

- [ ] **Step 3: Write lost-Complete HEAD tests for all recovery modes**

Use an existing valid manifest whose ListParts returns `NoSuchUpload`. `s3_head_matching_length_recovers_lost_complete` asserts no Create and `RecoveredHead { etag: None }`. `ipfs3_image_head_requires_matching_length_and_nonempty_cid_etag` asserts a quoted non-empty CID is preserved in `RecoveredHead`. `zip_head_never_guesses_entry_cids_and_starts_one_replacement` uses `HeadRecovery::Never`, asserts the HEAD archive ETag never enters completion evidence, and observes exactly one replacement Create.

Also assert 403, malformed/missing required HEAD headers, length mismatch, and non-404 non-2xx are errors. A confirmed 404 permits the one replacement.

- [ ] **Step 4: Run RED**

Run:

```powershell
cargo test -p eh_client --lib s3_multipart::tests::multipart_only_classifies_explicit_operation_codes_as_unsupported -- --exact
cargo test -p eh_client --lib s3_multipart::tests::multipart_complete_classifies_http_200_error_roots_before_provider_parsing -- --exact
cargo test -p eh_client --lib s3_multipart::tests::s3_head_matching_length_recovers_lost_complete -- --exact
cargo test -p eh_client --lib s3_multipart::tests::zip_head_never_guesses_entry_cids_and_starts_one_replacement -- --exact
```

Expected: fallback/replacement/HEAD branches are absent and tests fail.

- [ ] **Step 5: Implement tri-state capability storage**

Map `Unknown=0`, `Supported=1`, `Unsupported=2` in `MultipartCapability`; use Acquire loads and Release stores. This type stores only process-local optimization state. The engine itself returns evidence/outcomes; provider adapters decide which cache to update.

- [ ] **Step 6: Implement explicit unsupported cleanup and replacement rules**

On an explicit unsupported operation: best-effort Abort an active session using the unmodified base bucket, remove the matching local manifest, and return `Unsupported`. Abort failure must not hide the primary explicit unsupported result.

Maintain `replacement_used: bool` for one `upload_multipart` call. Catch both `MultipartFailure::InvalidInventory` and `reconcile_parts` wrong-size failures as replaceable inventory corruption. For either inventory corruption or same-uploader stale identity, require Abort success or `NoSuchUpload` before clearing/replacing; a transient/auth Abort failure returns error and leaves the manifest for retry. `NoSuchUpload` itself needs no Abort. Once the budget is used, a second invalid-session/reconciliation result returns an error naming the operation but no signed URL/body.

- [ ] **Step 7: Implement HEAD decision after NoSuchUpload**

Call `bucket.head_object(&object_key)` only for a previously valid manifest. Interpret status strictly:

- `404`: confirmed missing; remove manifest and use the one replacement;
- `2xx` + exact `content_length`: S3 returns `RecoveredHead { etag: None }`;
- `2xx` + exact length + non-empty trimmed `e_tag`: ipfS3 image returns the literal ETag;
- `HeadRecovery::Never`: even matching ZIP HEAD cannot recover entry CIDs; remove old manifest and use one replacement;
- every other status or missing/malformed required header: error, no guessing and no replacement.

Do not HEAD after malformed JSON, initial Create failure, or generic ListParts failure.

- [ ] **Step 8: Run GREEN and all shared-module tests**

Run:

```powershell
cargo test -p eh_client --lib s3_multipart:: -- --nocapture
```

Expected: strict fallback, one replacement, all HEAD policies, URL sanitization, manifest rules, and standard/resume flows pass offline.

- [ ] **Step 9: Stop at the review boundary; do not commit**

Suggested orchestrator commit after authorization: `feat(eh_client): classify and recover multipart sessions`.

---

### Task 8: Integrate thresholded S3 and ipfS3 image multipart without changing URL semantics

**Files:**
- Modify/Test: `eh_client/src/telegraph.rs:728-802,1185-1298,1410-1691,2417-2690`
- Consume unchanged: `eh_client/src/s3_multipart/mod.rs`

**Interfaces:**
- Consumes: Task 1 thresholds/resume contexts; Task 7 engine/capability API; existing object-key, public URL, CID URL-pair, and warmup helpers.
- Produces: image selection/fallback adapters for both uploaders; strict standard Complete XML parser; process-local standard capability per uploader.

- [ ] **Step 1: Write threshold selection tests on real wiremock requests**

For both S3 and ipfS3 write request tests proving:

- threshold `0`: an 8 MiB image sends exactly one PutObject and no Create;
- threshold `1`: `1 MiB - 1` sends PutObject;
- threshold `1`: exactly 1 MiB selects multipart (one 1 MiB final part is valid because it is final);
- default 8 MiB retains existing small-image PutObject tests.

Use distinct Create (`POST ?uploads`), UploadPart (`PUT ?partNumber&uploadId`), Complete (`POST ?uploadId`), and ordinary PUT matchers so the test cannot pass by request-method coincidence.

- [ ] **Step 2: Write S3 resume/result/fallback tests**

Add `s3_large_image_resumes_and_returns_existing_public_url_shape` with one stable temp context across two uploader instances; assert the second instance reuses the manifest key and returns the existing encoded CDN URL shape. Add `s3_each_explicit_multipart_operation_unsupported_uses_exactly_one_put_object`; table-drive parsed `NotImplemented` at Create, ListParts, UploadPart, and Complete, and in every case assert one ordinary PUT total plus the required Abort for sessions that were created. Add `s3_auth_transient_and_malformed_multipart_errors_do_not_put_fallback`; table-drive `AccessDenied`, 503, connection close, and malformed XML, asserting an error and zero ordinary PUT requests.

The first test asserts URL encoding still uses `public_base_url + object_key`, and the manifest is removed only after the URL exists.

- [ ] **Step 3: Write ipfS3 image Complete-result tests**

Return a standard Complete XML body containing a quoted CID ETag and assert preview/public URLs plus optional warmup match current behavior. Add malformed XML, wrong root, empty ETag, and HEAD-recovered CID tests. None may apply MD5 validation.

- [ ] **Step 4: Run RED**

Run:

```powershell
cargo test -p eh_client --lib telegraph::tests::s3_large_image_resumes_and_returns_existing_public_url_shape -- --exact
cargo test -p eh_client --lib telegraph::tests::s3_each_explicit_multipart_operation_unsupported_uses_exactly_one_put_object -- --exact
cargo test -p eh_client --lib telegraph::tests::ipfs3_image_multipart_complete_uses_cid_etag_and_existing_gateways -- --exact
```

Expected: current uploaders only send PutObject and tests fail request expectations.

- [ ] **Step 5: Keep adapter state small and credential-free**

Add to each uploader:

```rust
multipart_http: reqwest::Client,
standard_multipart: MultipartCapability,
uploader_identity_sha256: String,
```

Rename ipfS3's existing ten-second `http` field to `gateway_http` and keep it dedicated to gateway warmup. Build `multipart_http` with `reqwest::Client::builder().timeout(std::time::Duration::from_secs(60)).build()?`. Compute uploader fingerprint from provider literal, normalized endpoint, region, bucket, and path-style boolean only.

- [ ] **Step 6: Implement provider-side standard Complete parsing**

Add a strict private parser for root `CompleteMultipartUploadResult` with optional `Location` and required non-empty `Bucket`, `Key`, and `ETag`. Validate no trailing XML content using the same root-validation discipline as `DecompressZipResult`; validate returned bucket and key against the configured bucket and stable manifest key. S3 parses and validates this result before producing its public URL. For ipfS3 image, additionally trim whitespace and surrounding quotes from ETag and require a non-empty CID. `RecoveredHead` bypasses XML parsing but must satisfy Task 7's provider-specific HEAD requirements.

- [ ] **Step 7: Route eligible images through the shared engine**

For every image:

1. If threshold says PutObject or standard capability is `Unsupported`, call the unchanged PutObject helper path.
2. Otherwise pass candidate key, content type, bytes, context path/logical ID, `CreateExtension::None`, and provider-specific `HeadRecovery` to `upload_multipart`.
3. On `Unsupported`, mark only this uploader's standard capability unsupported and call exactly one unchanged PutObject.
4. On `Completed`, parse/build provider URL data; mark standard supported only after a real successful Complete (or valid HEAD recovery); call `remove_manifest()` last.
5. Return operational errors unchanged; do not mark unsupported and do not PutObject fallback.

Do not share capability state between `S3Uploader` and `IpfS3Uploader` instances.

- [ ] **Step 8: Run GREEN and existing image regressions**

Run:

```powershell
cargo test -p eh_client --lib telegraph::tests::s3_ -- --nocapture
cargo test -p eh_client --lib telegraph::tests::ipfs3_uploader_ -- --nocapture
cargo test -p eh_client --lib telegraph::tests::ipfs3_image_ -- --nocapture
```

Expected: threshold boundaries, resume, strict fallback, CID parsing, gateway pairs/warmup, and legacy small PutObject behavior all pass.

- [ ] **Step 9: Stop at the review boundary; do not commit**

Suggested orchestrator commit after authorization: `feat(eh_client): resume s3-backed image uploads`.

---

### Task 9: Add ipfS3 multipart ZIP Create-only extension and standard-result fallback

**Files:**
- Modify/Test: `eh_client/src/telegraph.rs:798-1407,1410-1625,3440-4257`
- Modify/Test: `eh_client/src/s3_multipart/mod.rs`

**Interfaces:**
- Consumes: existing ZIP compatibility/parser/entry-mapping helpers, Task 8 standard Complete parser, and `CreateExtension::IpfS3DecompressZip`.
- Produces: ipfS3 ZIP multipart-first flow, dedicated ZIP capability cache, exact Create-only extension behavior, single-Put ZIP compatibility fallback, and ordered entry URL pairs.

- [ ] **Step 1: Write the complete multipart ZIP request-contract test**

Use a structurally real ZIP fixture with at least two requested entries and more than one multipart part. Assert:

- Create POST has signed `uploads` and exactly one `decompress-zip=<archive-stem>/`;
- Create does not send `decompress-zip-result` or SSE headers;
- every UploadPart PUT, ListParts GET, Complete POST, and any Abort DELETE has no query name starting `decompress-`;
- UploadPart/ListParts CID ETags are emitted in ascending Complete part order and XML decodes to the literal original CIDs;
- Complete returns `DecompressZipResult`; URLs use requested entry CIDs in archive order and never archive/root CID.

Name it `ipfs3_zip_multipart_uses_decompress_query_only_on_create_and_returns_ordered_entry_cids`.

- [ ] **Step 2: Write ZIP resume identity and response-classification tests**

Add tests proving:

- reconstruction reuses exact object key/extraction prefix and requested-entry fingerprint;
- changed requested entry name/order invalidates and replaces the session once;
- malformed `DecompressZipResult`, transport/auth/5xx, and malformed protocol remain errors;
- deterministic preflight incompatibility still returns `Ok(None)` before any multipart request;
- valid extraction XML with missing/failed requested entries returns `Ok(None)` to existing per-image fallback after manifest cleanup.

- [ ] **Step 3: Write the standard-Complete compatibility fallback regression**

First ZIP multipart Complete returns a valid `CompleteMultipartUploadResult`, not `DecompressZipResult`. Assert:

1. standard multipart capability becomes supported;
2. only ZIP-extraction multipart capability becomes unsupported;
3. completed ZIP manifest is removed;
4. one existing single PutObject with `decompress-zip` obtains `DecompressZipResult` and ordered entry CIDs;
5. a subsequent eligible ipfS3 image still uses multipart, proving standard/image multipart was not disabled.

Name it `ipfs3_standard_complete_disables_only_zip_extension_and_keeps_image_multipart`.

- [ ] **Step 4: Run RED**

Run:

```powershell
cargo test -p eh_client --lib telegraph::tests::ipfs3_zip_multipart_uses_decompress_query_only_on_create_and_returns_ordered_entry_cids -- --exact
cargo test -p eh_client --lib telegraph::tests::ipfs3_standard_complete_disables_only_zip_extension_and_keeps_image_multipart -- --exact
```

Expected: current ZIP path sends one PutObject and the multipart request expectations fail.

- [ ] **Step 5: Add a ZIP-extraction capability cache without changing trait capability**

Add `multipart_zip_extract: MultipartCapability` to `IpfS3Uploader`. `supports_zip_archive_upload()` continues to return the operator's `zip_extract_enabled`; a process-cached multipart ZIP `Unsupported` still uses the existing single-Put ZIP extension and therefore must not make the trait return false.

- [ ] **Step 6: Route compatible enabled ZIPs through multipart first**

Keep current guards in order: explicit false -> `Ok(None)`, empty requested list -> empty success, deterministic ZIP preflight -> `Ok(None)`. Then:

- if standard or ZIP-extraction multipart cache is unsupported, call the unchanged single-Put ZIP helper;
- otherwise call the engine with `CreateExtension::IpfS3DecompressZip { requested_entries_sha256 }` and `HeadRecovery::Never`;
- engine derives/persists the exact extraction prefix from the selected stable object key;
- explicit unsupported on the extension-bearing Create marks only multipart ZIP extraction unsupported; explicit unsupported on query-free ListParts/UploadPart/Complete marks standard multipart unsupported; either case calls one single-Put ZIP fallback and does not alter the unrelated cache;
- no later request receives the clone's extension query.

Extract the existing single-Put body into a private helper rather than duplicating it:

```rust
async fn upload_zip_archive_single_put(
    &self,
    key: &str,
    extraction_prefix: &str,
    archive: &ZipArchiveUploadInput<'_>,
) -> Result<Option<Vec<TelegraphImageUrlPair>>>;
```

- [ ] **Step 7: Classify Complete roots before choosing capability/fallback**

For `CompletionEvidence::Response`:

- root `DecompressZipResult`: strict parse, mark standard supported and ZIP extraction supported, map entry CIDs in requested order, remove manifest after mapping/URL construction, return `Some` or deterministic `None`;
- root `CompleteMultipartUploadResult`: strict parse, mark standard supported and ZIP extraction unsupported, remove manifest, call single-Put ZIP helper; do not delete the already completed archive object because delete permission is outside scope;
- any other/malformed/trailing XML: return error and retain manifest; do not single-Put fallback.

For ZIP, `RecoveredHead` is unreachable by contract; treat it as an internal error if observed.

- [ ] **Step 8: Run GREEN and all ZIP regressions**

Run:

```powershell
cargo test -p eh_client --lib telegraph::tests::ipfs3_zip_ -- --nocapture
cargo test -p eh_client --lib telegraph::tests::default_zip_archive_upload_capability_returns_none -- --exact
```

Expected: new multipart ZIP tests and every existing preflight/parser/fallback test pass; only Create carries `decompress-zip`; image multipart remains enabled after standard ZIP Complete fallback.

- [ ] **Step 9: Stop at the review boundary; do not commit**

Suggested orchestrator commit after authorization: `feat(eh_client): resume ipfs3 zip extraction uploads`.

---

### Task 10: Thread stable EH resume contexts and clean successful upload state

**Files:**
- Modify/Test: `src/scheduler/eh_engine.rs:11-15,1325-1352,1486-1650,4382-4605`

**Interfaces:**
- Consumes: `UploadResumeContext`, `ArchiveArtifacts::uploads_dir/remove_upload_state`, and unchanged `ImageUploader` methods.
- Produces: `.zip.uploads/archive.json`, zero-based `.zip.uploads/image-<uploadable-order>.json`, stable logical IDs, and success cleanup after Telegraph DB persistence.

- [ ] **Step 1: Extend the mock uploader to record owned context values**

Because uploader inputs borrow paths/IDs, copy them into test-owned records:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
struct SeenResumeContext {
    manifest_path: std::path::PathBuf,
    logical_object_id: String,
}
```

Update `ZipFirstMockUploader` or add a focused recorder that captures ZIP and image contexts before returning. Existing non-S3 mock uploaders must continue accepting contexts without using them.

- [ ] **Step 2: Write failing stable-context tests**

Add `upload_worker_passes_stable_zip_resume_context` and assert the recorder sees exactly `<zip>.uploads/archive.json` plus logical ID `archive`. Add `upload_worker_uses_original_uploadable_order_for_image_contexts`; place metadata around two images, run two attempts, and assert both attempts see only `image-0.json`/`image-1.json` with matching logical IDs. Add `successful_telegraph_upload_removes_upload_state_only`; seed `archive.json`, complete page creation/DB transition, and assert `.zip.uploads` is absent while the final ZIP remains for publish.

The image test ZIP contains a directory/non-image before and between images. Run two worker attempts against the same ZIP and assert identical paths/IDs. Expected exact names are `image-0.json`, `image-1.json`; these are zero-based indices among uploadable entries, not physical ZIP indices and not one-element uploader batch indices.

- [ ] **Step 3: Run RED**

Run:

```powershell
cargo test -p pixivbot scheduler::eh_engine::tests::upload_worker_passes_stable_zip_resume_context -- --exact
cargo test -p pixivbot scheduler::eh_engine::tests::upload_worker_uses_original_uploadable_order_for_image_contexts -- --exact
```

Expected: scheduler currently passes `None`, so recorded contexts are missing.

- [ ] **Step 4: Attach ZIP context to the archive-first call**

After resolving `zip_path`, construct `ArchiveArtifacts::new(zip_path)`. For ZIP-first:

```rust
let manifest_path = artifacts.uploads_dir().join("archive.json");
let resume_context = UploadResumeContext {
    manifest_path: &manifest_path,
    logical_object_id: "archive",
};
```

Pass `Some(resume_context)` with the existing bytes and ordered `entry_names`.

- [ ] **Step 5: Carry stable uploadable order through the blocking ZIP reader**

Add `uploadable_order: usize` to `ZipImageData`. Enumerate the already-filtered `uploadable_image_indices`:

```rust
for (uploadable_order, archive_index) in uploadable_image_indices.into_iter().enumerate() {
    let mut file = archive.by_index(archive_index).context("Failed to read zip entry")?;
    let mut data = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut data)
        .context("Failed to read image from zip")?;
    let filename = std::path::Path::new(file.name())
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("image.jpg")
        .to_owned();
    if image_tx.blocking_send(ZipImageData { filename, data, uploadable_order }).is_err() {
        return Ok(());
    }
}
```

For each received image, derive:

```rust
let logical_object_id = format!("image-{}", image.uploadable_order);
let manifest_path = artifacts.uploads_dir().join(format!("{logical_object_id}.json"));
```

Pass references through `UploadResumeContext` for that awaited one-image call.

- [ ] **Step 6: Remove upload state only after Telegraph result is durable**

In `create_telegraph_page_for_entry`, keep `mark_eh_download_uploaded_with_rewrite` first. After it succeeds, derive `ArchiveArtifacts::new(zip_path)` from `entry.zip_path` and call `remove_upload_state()`. If removal fails after the DB transition, log a warning and return success so the completed page is not recreated; startup/final cleanup can remove the residue. Do not remove the final ZIP here because publish may still need it.

- [ ] **Step 7: Run GREEN and worker regressions**

Run:

```powershell
cargo test -p pixivbot scheduler::eh_engine::tests::upload_worker_passes_stable_zip_resume_context -- --exact
cargo test -p pixivbot scheduler::eh_engine::tests::upload_worker_uses_original_uploadable_order_for_image_contexts -- --exact
cargo test -p pixivbot scheduler::eh_engine::tests::successful_telegraph_upload_removes_upload_state_only -- --exact
cargo test -p pixivbot scheduler::eh_engine::tests::test_upload_worker_ -- --nocapture
```

Expected: contexts are stable across attempts, successful Telegraph persistence removes `.zip.uploads` but preserves `.zip`, and ZIP-first/per-image fallback tests remain green.

- [ ] **Step 8: Stop at the review boundary; do not commit**

Suggested orchestrator commit after authorization: `feat(scheduler): persist eh upload resume contexts`.

---

### Task 11: Complete cancellation, permanent-failure, publish, orphan cleanup, and final verification

**Files:**
- Modify/Test: `src/scheduler/eh_engine.rs:40-59,1393-1443,1450-1484,1652-1662,1719-1939,4862-5150`
- Modify/Test: `src/db/repo/eh_download_queue.rs:3332-3381,4153-4264`
- Verify: `eh_client/src/archive_download/mod.rs`
- Verify: `eh_client/src/client.rs`
- Verify: `config.toml.example`

**Interfaces:**
- Consumes: complete `ArchiveArtifacts` API, scheduler queue transitions, and repository active-status set.
- Produces: exact lifecycle matrix below, focused cleanup tests, full regression and `make ci` evidence.

- [ ] **Step 1: Write failing artifact-lifecycle tests before changing cleanup paths**

Add/extend tests to prove this exact matrix:

| Event | Final ZIP | `.zip.part` / `.zip.parts` | `.zip.uploads` |
|---|---|---|---|
| upload retry or chat deferral | preserve | preserve existing behavior | preserve |
| Telegraph upload success | preserve until publish | unchanged | remove |
| permanent Telegraph failure with archive fallback | preserve | unchanged | remove |
| permanent upload failure without fallback | remove whole family | remove | remove |
| cancellation observed by upload/publish worker | preserve immediate ZIP behavior | unchanged | remove |
| final publish success or permanent publish failure | remove whole family | remove | remove |
| active/retryable startup row with final ZIP | preserve | remove stale download state | preserve |
| genuine orphan/canceled terminal family at startup | remove whole family | remove | remove |

Required test names:

```rust
test_upload_permanent_failure_fallback_removes_upload_state_but_keeps_zip
test_upload_permanent_failure_without_fallback_removes_whole_archive_family
test_upload_canceled_after_claim_removes_upload_state_without_sending
test_publish_success_removes_whole_archive_family
test_publish_permanent_failure_removes_whole_archive_family
test_cleanup_eh_cache_orphans_preserves_active_upload_state_and_removes_orphan_upload_state
```

Extend the existing `test_publish_skips_entry_canceled_after_claim` with a seeded `.zip.uploads/archive.json` and assert cancellation removes that directory while preserving its existing immediate-ZIP assertion.

- [ ] **Step 2: Run RED**

Run:

```powershell
cargo test -p pixivbot scheduler::eh_engine::tests::test_upload_permanent_failure_fallback_removes_upload_state_but_keeps_zip -- --exact
cargo test -p pixivbot db::repo::eh_download_queue::tests::test_cleanup_eh_cache_orphans_preserves_active_upload_state_and_removes_orphan_upload_state -- --exact
```

Expected: current worker cleanup only removes the ZIP file and repository family recognition does not include `.zip.uploads`.

- [ ] **Step 3: Replace ad hoc ZIP deletion with explicit artifact operations**

Use helpers that derive from `entry.zip_path` when available:

```rust
async fn remove_entry_upload_state(entry: &eh_download_queue::Model) {
    let Some(zip_path) = entry.zip_path.as_deref() else { return };
    if let Err(error) = ArchiveArtifacts::new(zip_path).remove_upload_state().await {
        warn!("Failed to delete EH upload state for gid={}: {}", entry.gid, error);
    }
}

async fn remove_entry_archive_family(entry: &eh_download_queue::Model) {
    let Some(zip_path) = entry.zip_path.as_deref() else { return };
    if let Err(error) = ArchiveArtifacts::new(zip_path).remove_all().await {
        warn!("Failed to delete EH archive family for gid={}: {}", entry.gid, error);
    }
}
```

Apply them as follows:

- before `fallback_eh_upload_to_archive`, remove upload state only;
- on permanent upload/publish failure, remove whole family;
- after upload/publish worker detects cancellation under `EH_PUBLISH_CANCEL_LOCK`, remove upload state only and perform no send/upload;
- final publish cleanup uses whole-family removal;
- existing download-worker `cleanup_archive_artifacts` already uses `remove_all()` and automatically gains upload-state removal.

Cleanup failure remains logged and does not overwrite the queue transition's primary result.

- [ ] **Step 4: Preserve active upload state in startup orphan cleanup**

Because Task 2 makes `ArchiveArtifacts::from_member()` recognize `.zip.uploads`, keep the existing active/retryable statuses and branch semantics:

```rust
if !active_final_identities.contains(&final_zip) {
    artifacts.remove_all().await
} else if final_zip.exists() {
    artifacts.remove_multipart_state().await // deliberately preserves uploads_dir
} else {
    continue
}
```

Extend fixtures with nested `archive.json`/`image-0.json`. Assert active `.zip.uploads` survives while `.zip.parts` is independently removed; orphan and canceled families lose `.zip.uploads` recursively. Do not add DB fields or migration.

- [ ] **Step 5: Run focused lifecycle and provider suites**

Run:

```powershell
cargo test -p eh_client --lib s3_multipart:: -- --nocapture
cargo test -p eh_client --lib archive_download::artifacts::tests -- --nocapture
cargo test -p eh_client --lib telegraph::tests::s3_ -- --nocapture
cargo test -p eh_client --lib telegraph::tests::ipfs3_ -- --nocapture
cargo test -p pixivbot scheduler::eh_engine::tests::test_upload_ -- --nocapture
cargo test -p pixivbot scheduler::eh_engine::tests::test_publish_ -- --nocapture
cargo test -p pixivbot db::repo::eh_download_queue::tests::test_cleanup_eh_cache_orphans -- --nocapture
```

Expected: all focused suites pass, including old PutObject, ZIP preflight, Telegraph creation, preview rewrite, Pixi, Catbox, download resume, and cleanup behavior.

- [ ] **Step 6: Audit prohibited persistence and request surfaces**

Run repository-scoped searches:

```powershell
rg -n "decompress-" eh_client/src/s3_multipart eh_client/src/telegraph.rs
rg -n "presign|X-Amz|secret_access_key|access_key_id|etag" eh_client/src/s3_multipart/manifest.rs
rg -n "OPTIONS|Method::OPTIONS" eh_client/src/s3_multipart eh_client/src/telegraph.rs
rg -n "image-.*url|successful.*url|multipart.*url" migration src/db eh_client/src/s3_multipart
```

Expected evidence:

- extension-query mutation exists only in the Create clone branch;
- manifest schema has no credential/signed-URL/object-byte/completed-ETag field (test-only secret strings are acceptable in assertions);
- no OPTIONS request exists;
- no migration or task-level successful-image URL persistence exists.

- [ ] **Step 7: Run formatter and complete repository quality gate**

Run:

```powershell
cargo fmt --all -- --check
make ci
```

Expected: formatting passes; `make ci` completes `fmt-check`, clippy with warnings denied, check, workspace tests, and release build. Do not enable extra H.264/ugoira tests beyond the repository's normal `make ci` behavior.

- [ ] **Step 8: Review the final diff without writing Git state**

Run:

```powershell
git status --short
git diff --check
git diff --stat
```

Expected: only files in the locked file map changed; no `config.toml`, migration, generated secret, or unrelated source file is present.

- [ ] **Step 9: Stop at the final review boundary; do not commit**

Suggested orchestrator commits after explicit authorization, split only if the integrated diff supports clean boundaries:

1. `feat(eh_client): resume s3 multipart uploads`
2. `feat(scheduler): preserve eh upload sessions`
3. `docs: document multipart upload compatibility`

Implementation subagents do not execute these commands; the orchestrator owns staging and commits after user authorization.

---

## Specification coverage map

| Specification requirement | Plan coverage |
|---|---|
| S3/ipfS3 threshold defaults, inclusive boundary, `0`, saturation; ZIP default true | Task 1; provider request proof in Task 8 |
| Focused `s3_multipart/{mod.rs,manifest.rs,list_parts.rs}` instead of growing transfer logic in `telegraph.rs` | Locked file map; Tasks 3-7 |
| Versioned credential-free SHA-256 manifest; atomic persistence; abort on write failure | Task 3; Create integration in Task 5 |
| `.zip.uploads` independent from `.zip.part`/`.zip.parts` | Task 2; lifecycle integration Tasks 10-11 |
| Presigned ListParts pagination, validation, server authority, URL/error sanitization | Task 4; resume use Task 6 |
| Create/List provisional support, Complete/full support, strict unsupported classification, no OPTIONS | Tasks 5 and 7; provider caches Tasks 8-9 |
| Sequential 8 MiB partitioning, 10,000 limit, sorted Complete, literal opaque/CID ETags | Task 5 |
| Resume after process restart, response loss, missing/uncertain-only upload | Task 6 |
| Stale session/invalid parts/malformed manifest/one replacement | Tasks 6-7 |
| Lost Complete HEAD: S3 length, ipfS3 image length+CID, ZIP replacement | Task 7; provider result use Task 8 |
| S3 public URL and ipfS3 image CID/gateway/warmup semantics | Task 8 |
| ipfS3 ZIP Create-only `decompress-zip`; no extension on Upload/List/Complete/Abort | Task 9 |
| Valid `DecompressZipResult`, ordered entry CIDs, archive CID never used | Task 9 |
| Standard ZIP Complete disables only ZIP extension and falls back to one single-Put ZIP | Task 9 |
| Stable EH archive/image context based on original uploadable order; no URL persistence | Task 10 and Task 11 audit |
| Success, cancellation, permanent failure, final publish, and orphan cleanup | Tasks 10-11 |
| Public config documentation and multipart permissions | Task 1 |
| Focused tests and complete `make ci` | Every task's RED/GREEN commands; Task 11 full gate |

## Plan self-review result

- **Spec coverage:** every design goal, flow, error/fallback rule, provider result, lifecycle rule, test category, and acceptance scenario maps to at least one task above.
- **Placeholder scan:** no deferred implementation decisions, incomplete sections, or unspecified test/error-handling steps remain; every behavior-changing task names concrete tests, RED evidence, implementation signatures/control flow, GREEN commands, and expected results.
- **Type consistency:** `UploadResumeContext`, input field name `resume_context`, `ProviderKind`, `CreateExtension`, `HeadRecovery`, `MultipartUploadRequest`, `CompletionEvidence`, `CompletedUpload`, `MultipartOutcome`, `MultipartCapability`, `uploads_dir`, and `remove_upload_state` are used consistently from their producing task through downstream consumers.
- **Scope check:** work remains one coordinated feature without migration, AWS SDK, parallel part transfer, successful-image URL persistence, or unrelated provider changes.
- **Ambiguity check:** zero-based uploadable image index, one replacement per call, manifest-removal timing, no-context abort behavior, MethodNotAllowed classification, Abort error precedence, HEAD decision matrix, and ZIP standard-result capability effects are explicitly fixed.
- **Receipt status:** waiting for orchestrator-owned plan-critic receipt; this planner role does not dispatch plan-critic.
