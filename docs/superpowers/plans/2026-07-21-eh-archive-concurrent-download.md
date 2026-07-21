# EH Archive Concurrent Download Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add crash-resumable, dynamically split, bounded concurrent HTTP Range downloads for one authenticated EH archive while preserving the existing sequential API, worker retry semantics, ZIP validation, and queue-level concurrency.

**Architecture:** Move archive byte transfer out of the already-large `client.rs` into a responsibility-focused `archive_download/` module tree: the facade owns only public options and path selection, while artifacts, manifest recovery, split policy, shared authenticated HTTP policy, sequential transfer, one-part transfer, initial Range negotiation, coordination, and assembly each have a separate production file. `client.rs` remains responsible for the archiver POST/redirect, complete ZIP validation, and final atomic rename; the scheduler only passes `EhentaiConfig.archive_download_concurrency` through `ArchiveDownloadOptions` and does not schedule parts itself. Keep each production module at or below roughly 250 pure lines of code when practical, splitting by responsibility rather than adding pass-through wrappers.

**Tech Stack:** Rust 1.94, Tokio (`fs`, `io-util`, `macros`, `rt`, `sync`, `time`), reqwest 0.12, serde/serde_json, futures-util, tempfile 3, zip 8.6, wiremock 0.6, SeaORM

---

## Scope and fixed decisions

- Default per-archive concurrency is `1`; deserializing `0` is an error.
- Existing public signatures and argument order for `EhClient::download_archive()` and `EhClient::download_archive_with_request()` stay unchanged and retain maximum concurrency `1`.
- New options-based methods append `ArchiveDownloadOptions` after the existing arguments.
- A valid existing multipart manifest takes precedence over `.zip.part` and resumes even when the current maximum is `1`.
- A new multipart transfer starts with exactly one `Range: bytes=0-` request; it never pre-splits the object.
- EWMA uses alpha `0.25`; the split floor is `max(1 MiB, ceil(rate * 15s))`.
- A fresh sample or part completion may pause only the largest sampled active interval, join it, recompute its cursor from file length, split proportionally, atomically replace the manifest, and relaunch bounded ranges.
- Multipart `200`, `416`, malformed/mismatched `206`, URL/total/validator changes, or `If-Range` fallback clear multipart state and restart the sequential downloader once from byte zero.
- Validator-free recovery is allowed when URL and total length still match and every response has an exact `Content-Range`.
- Queue-entry concurrency, Telegram UI, database schema, and unauthenticated image-to-ZIP fallback are unchanged.
- Do not log archive URLs, cookies, manifest JSON, response bodies, or token-bearing cache paths.
- No Git write is authorized by this plan. Every suggested commit boundary below requires explicit user permission before staging or committing.

## Current-state map

- `eh_client/src/client.rs:12-25,167-229,457-708,1142-1169` currently contains archive constants, progress classification, open-ended resume validation, four-attempt sequential streaming, ZIP validation, and final rename. It is 1262 lines and must shrink rather than absorb multipart coordination.
- `eh_client/src/lib.rs:1-9` exports `client`, `error`, models, parser, and selected client types; it has no archive-transfer module tree.
- `eh_client/src/error.rs:3-85` already supplies `Http`, `Json`, `Io`, `Parse`, `Other`, and `DownloadInProgress { inner, attempts, bytes_delta, elapsed }`. These existing conversions are sufficient; no new public error variant is needed.
- `eh_client/tests/integration.rs:292-718,1014-1126` already verifies full archive download, legacy `.zip.part` resume, Range rejection/416 restart, progress classification, and ZIP/entry corruption cleanup with wiremock.
- `src/config.rs:334-604` defines all EH fields/defaults; `background_download_concurrency` is independent queue-entry concurrency and remains unchanged.
- `config.toml.example:139-205` documents EH settings but has no per-archive connection setting.
- `src/scheduler/eh_engine.rs:309-384,1059-1197` contains the two authenticated archive call sites. Both currently call `download_archive_with_request`; main-worker permanent cleanup removes only `.zip` and `.zip.part`, and background permanent failure performs no artifact cleanup.
- `src/db/repo/eh_download_queue.rs:3096-3174,3886-3962` recognizes only `.zip` and `.zip.part`; top-level `.zip.parts` directories are ignored and removal is non-recursive.
- `eh_client/Cargo.toml:20-29` enables Tokio file/I/O/time features; source-level task coordination also needs `macros` and `sync`, while `tempfile` must move from dev-only to normal dependencies for cross-platform atomic manifest replacement.

## File map

| File | Change and responsibility |
|---|---|
| `eh_client/src/archive_download/mod.rs` | **Create.** Declare focused submodules; own `ArchiveDownloadOptions` and the real `download_to_partial` path-selection/orchestration facade. Re-export `ArchiveArtifacts` publicly and the HTTP sanitizer crate-privately; contain only facade/option tests. |
| `eh_client/src/archive_download/artifacts.rs` | **Create.** Own `ArchiveArtifacts`, artifact-family recognition, idempotent file/directory cleanup, and colocated artifact tests. |
| `eh_client/src/archive_download/manifest.rs` | **Create.** Own manifest/part schema, invalid-state classification, recoverable I/O boundary, atomic persistence, unreferenced cleanup, and colocated manifest tests. |
| `eh_client/src/archive_download/policy.rs` | **Create.** Own pure EWMA/adaptive-minimum/split-input calculations and colocated policy tests; no HTTP, filesystem, or task ownership. |
| `eh_client/src/archive_download/http.rs` | **Create.** Own EH-host detection, host-conditional Cookie request construction, and URL-stripping reqwest error conversion shared by sequential/part/coordinator/client paths. |
| `eh_client/src/archive_download/sequential.rs` | **Create.** Own the extracted legacy sequential downloader, Range-resume validation, retry/progress behavior, and colocated compatibility tests. |
| `eh_client/src/archive_download/part.rs` | **Create.** Own validators, exact bounded Range validation, one-part request/retry/pause loop, durable part progress, and colocated part tests. |
| `eh_client/src/archive_download/initialization.rs` | **Create.** Own the single `bytes=0-` handshake and classification into multipart seed, reused `200` response, or sequential restart. |
| `eh_client/src/archive_download/coordinator.rs` | **Create.** Own runtime part/task state, recovery scheduling, event loop, dynamic split commit, graceful join, and coordinator tests. |
| `eh_client/src/archive_download/assembly.rs` | **Create.** Own ordered part assembly into `.zip.part` and colocated assembly tests. |
| `eh_client/src/lib.rs` | Register the focused module and re-export `ArchiveArtifacts` plus `ArchiveDownloadOptions`. |
| `eh_client/src/client.rs` | Remove transfer internals, retain archiver POST/redirect and ZIP validation/final rename, preserve old methods, and add options-based wrappers. |
| `eh_client/src/error.rs` | **No source change.** Reuse existing conversions and aggregate `DownloadInProgress` fields. |
| `eh_client/Cargo.toml` | Enable Tokio `macros` and `sync`, and promote the existing dev dependency `tempfile` to a normal dependency for atomic manifest replacement; introduce no new crate. |
| `eh_client/tests/integration.rs` | Wiremock real-surface tests for first request, dynamic concurrency, work stealing, exact assembly, recovery, reset/fallback, aggregate progress, corruption, and old API compatibility. |
| `src/config.rs` | Add a default-one, non-zero-deserialized `archive_download_concurrency` field and focused tests. |
| `config.toml.example` | Document per-archive versus queue-level concurrency and the invalid-zero rule. |
| `src/scheduler/eh_engine.rs` | Pass options from both authenticated workers and clean the whole artifact family after permanent main/background failures; extend worker tests. |
| `src/db/repo/eh_download_queue.rs` | Use `ArchiveArtifacts` to group active/orphan `.zip`, `.zip.part`, and `.zip.parts` families and recursively delete orphan families; extend repository tests. |

**Module-size rule:** measure production code separately from colocated `#[cfg(test)]` modules. Target each `archive_download/*.rs` production section at `<=250` lines when practical. If a file exceeds that because another independent responsibility emerged, extract that responsibility with a data-bearing interface; do not meet the target with one-line forwarding modules or same-signature pass-through wrappers. `mod.rs` remains the only archive-level path-selection facade, `coordinator.rs` remains the only owner of live task state, and `client.rs` must shrink overall.

**Dependency direction:** `mod.rs` may call `initialization`, `coordinator`, `sequential`, and `assembly`; `coordinator` consumes `manifest`, `policy`, and `part`; `initialization` consumes `http`, `manifest`, and validator selection from `part`; `sequential` and `part` consume `http`; `manifest` and `assembly` consume `artifacts`. Child modules never call the facade, `client.rs`, scheduler, or repository.

## Core type and API contract

The implementation must use these names consistently and keep ownership at the module boundary shown in comments:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveDownloadOptions {
    pub max_concurrency: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveArtifacts {
    final_zip: std::path::PathBuf,
    assembly_scratch: std::path::PathBuf,
    parts_dir: std::path::PathBuf,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub(super) struct ArchiveManifest {
    pub(super) version: u32,
    pub(super) download_url: String,
    pub(super) total_len: u64,
    pub(super) etag: Option<String>,
    pub(super) last_modified: Option<String>,
    pub(super) next_part_id: u64,
    pub(super) parts: Vec<ManifestPart>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub(super) struct ManifestPart {
    pub(super) id: u64,
    pub(super) start: u64,
    pub(super) end: u64,
}

// manifest.rs: only semantically invalid state is purgeable.
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
```

Intervals are half-open `[start, end)`. `part-{id:016}` stores the contiguous downloaded prefix of its interval. Progress is always read from file lengths and is never serialized into `manifest.json`.

`ManifestRecovery::Invalid` is a closed list of deterministic, content/state-invalid cases and is the only recovery result that authorizes purge plus sequential restart. The outer `Result<ManifestRecovery>` is reserved for recovery-time operational failures—permission/read/stat/read-directory/remove failures—and must propagate without deleting the manifest, referenced part files, or assembly state and without starting a sequential archive GET.

---

### Task 1: Add validated configuration and public archive option

**Files:**
- Create: `eh_client/src/archive_download/mod.rs`
- Modify: `eh_client/src/lib.rs:1-9`
- Modify/Test: `src/config.rs:334-447,518-604,631-710`
- Modify: `config.toml.example:172-180`

- [ ] **Step 1: Write failing default/accept/reject configuration tests**

Add these tests to `src/config.rs`'s existing test module:

```rust
#[test]
fn test_eh_archive_download_concurrency_defaults_to_one() {
    let cfg: EhentaiConfig = serde_json::from_value(serde_json::json!({})).unwrap();
    assert_eq!(cfg.archive_download_concurrency, 1);
}

#[test]
fn test_eh_archive_download_concurrency_accepts_values_above_one() {
    let cfg: EhentaiConfig = serde_json::from_value(serde_json::json!({
        "archive_download_concurrency": 4
    }))
    .unwrap();
    assert_eq!(cfg.archive_download_concurrency, 4);
}

#[test]
fn test_eh_archive_download_concurrency_rejects_zero() {
    let err = serde_json::from_value::<EhentaiConfig>(serde_json::json!({
        "archive_download_concurrency": 0
    }))
    .unwrap_err();
    assert!(err.to_string().contains("must be at least 1"));
}
```

- [ ] **Step 2: Run the configuration tests and capture RED**

```powershell
cargo test -p pixivbot --lib archive_download_concurrency -- --nocapture
```

Expected: compilation fails because `EhentaiConfig::archive_download_concurrency` does not exist.

- [ ] **Step 3: Add the non-zero deserializer, field, default, and example**

Add beside the other EH defaults in `src/config.rs`:

```rust
fn default_eh_archive_download_concurrency() -> usize {
    1
}

fn deserialize_nonzero_usize<'de, D>(deserializer: D) -> std::result::Result<usize, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = usize::deserialize(deserializer)?;
    if value == 0 {
        return Err(serde::de::Error::custom("must be at least 1"));
    }
    Ok(value)
}
```

Add this field immediately before `background_download_enabled` so the two concurrency controls are documented together but remain independent:

```rust
    /// Maximum active HTTP Range requests used by one authenticated EH archive.
    /// This does not change queue-entry concurrency.
    #[serde(
        default = "default_eh_archive_download_concurrency",
        deserialize_with = "deserialize_nonzero_usize"
    )]
    pub archive_download_concurrency: usize,
```

Initialize it in `impl Default for EhentaiConfig`:

```rust
archive_download_concurrency: default_eh_archive_download_concurrency(),
```

Add this exact example before `background_download_enabled` in `config.toml.example`:

```toml
# # Maximum HTTP Range connections used by one authenticated archive.
# # Default: 1. Values above 1 enable dynamic multipart transfer; 0 is invalid.
# # This is independent of background_download_concurrency below.
# archive_download_concurrency = 1
```

- [ ] **Step 4: Add and export `ArchiveDownloadOptions`**

Create `eh_client/src/archive_download/mod.rs` with the option type and validation. Add focused submodule declarations only in the task that creates each backing file, so every intermediate build remains valid:

```rust
use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveDownloadOptions {
    pub max_concurrency: usize,
}

impl Default for ArchiveDownloadOptions {
    fn default() -> Self {
        Self { max_concurrency: 1 }
    }
}

impl ArchiveDownloadOptions {
    pub(crate) fn validate(self) -> Result<Self> {
        if self.max_concurrency == 0 {
            return Err(Error::Other(
                "archive download max_concurrency must be at least 1".into(),
            ));
        }
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_download_options_default_to_one_and_reject_zero() {
        assert_eq!(ArchiveDownloadOptions::default().max_concurrency, 1);
        assert!(ArchiveDownloadOptions { max_concurrency: 0 }
            .validate()
            .is_err());
        assert_eq!(
            ArchiveDownloadOptions { max_concurrency: 3 }
                .validate()
                .unwrap()
                .max_concurrency,
            3
        );
    }
}
```

Register and re-export it in `eh_client/src/lib.rs`:

```rust
pub mod archive_download;
pub use archive_download::ArchiveDownloadOptions;
```

- [ ] **Step 5: Run GREEN checks for the option and configuration**

```powershell
cargo test -p eh_client --lib archive_download::tests::archive_download_options_default_to_one_and_reject_zero -- --nocapture
cargo test -p pixivbot --lib archive_download_concurrency -- --nocapture
```

Expected: the option test and all three configuration tests pass; zero reports the configured validation message.

- [ ] **Step 6: Record the suggested commit boundary**

Suggested boundary, requiring explicit user authorization: `feat: configure per-archive EH download concurrency`. Review only the new option/config contract and example; do not stage or commit during planning or execution without permission.

---

### Task 2: Define the shared archive artifact family

**Files:**
- Create/Test: `eh_client/src/archive_download/artifacts.rs`
- Modify: `eh_client/src/archive_download/mod.rs`
- Modify: `eh_client/src/lib.rs`

- [ ] **Step 1: Write failing artifact derivation and recursive-cleanup tests**

Add these tests in `archive_download/artifacts.rs`:

```rust
#[test]
fn archive_artifacts_derive_stable_family_paths_and_members() {
    let final_zip = std::path::Path::new("cache/12_token.zip");
    let artifacts = ArchiveArtifacts::new(final_zip);
    assert_eq!(artifacts.final_zip(), final_zip);
    assert_eq!(
        artifacts.assembly_scratch(),
        std::path::Path::new("cache/12_token.zip.part")
    );
    assert_eq!(
        artifacts.parts_dir(),
        std::path::Path::new("cache/12_token.zip.parts")
    );

    for member in [
        "cache/12_token.zip",
        "cache/12_token.zip.part",
        "cache/12_token.zip.parts",
    ] {
        assert_eq!(
            ArchiveArtifacts::from_member(std::path::Path::new(member)),
            Some(artifacts.clone())
        );
    }
    assert_eq!(
        ArchiveArtifacts::from_member(std::path::Path::new("cache/note.txt")),
        None
    );
}

#[tokio::test]
async fn archive_artifacts_remove_all_recursively_and_idempotently() {
    let temp = tempfile::tempdir().unwrap();
    let artifacts = ArchiveArtifacts::new(temp.path().join("12_token.zip"));
    tokio::fs::write(artifacts.final_zip(), b"zip").await.unwrap();
    tokio::fs::write(artifacts.assembly_scratch(), b"partial")
        .await
        .unwrap();
    tokio::fs::create_dir_all(artifacts.parts_dir()).await.unwrap();
    tokio::fs::write(artifacts.parts_dir().join("part-0000000000000000"), b"part")
        .await
        .unwrap();

    artifacts.remove_all().await.unwrap();
    artifacts.remove_all().await.unwrap();
    assert!(!artifacts.final_zip().exists());
    assert!(!artifacts.assembly_scratch().exists());
    assert!(!artifacts.parts_dir().exists());
}
```

- [ ] **Step 2: Run artifact tests and capture RED**

```powershell
cargo test -p eh_client --lib archive_download::artifacts::tests::archive_artifacts -- --nocapture
```

Expected: compilation fails because `ArchiveArtifacts` is undefined.

- [ ] **Step 3: Implement `ArchiveArtifacts` and idempotent removals**

Add these imports and implementation to `archive_download/artifacts.rs`:

```rust
use crate::error::Result;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveArtifacts {
    final_zip: PathBuf,
    assembly_scratch: PathBuf,
    parts_dir: PathBuf,
}

impl ArchiveArtifacts {
    pub fn new(final_zip: impl Into<PathBuf>) -> Self {
        let final_zip = final_zip.into();
        Self {
            assembly_scratch: final_zip.with_extension("zip.part"),
            parts_dir: final_zip.with_extension("zip.parts"),
            final_zip,
        }
    }

    pub fn from_member(path: &Path) -> Option<Self> {
        let name = path.file_name()?.to_str()?;
        let final_name = if let Some(name) = name.strip_suffix(".zip.parts") {
            format!("{name}.zip")
        } else if let Some(name) = name.strip_suffix(".zip.part") {
            format!("{name}.zip")
        } else if name.ends_with(".zip") {
            name.to_owned()
        } else {
            return None;
        };
        Some(Self::new(path.with_file_name(final_name)))
    }

    pub fn final_zip(&self) -> &Path {
        &self.final_zip
    }

    pub fn assembly_scratch(&self) -> &Path {
        &self.assembly_scratch
    }

    pub fn parts_dir(&self) -> &Path {
        &self.parts_dir
    }

    pub async fn remove_assembly_scratch(&self) -> Result<()> {
        remove_file_if_present(&self.assembly_scratch).await
    }

    pub async fn remove_parts_dir(&self) -> Result<()> {
        remove_dir_if_present(&self.parts_dir).await
    }

    pub async fn remove_multipart_state(&self) -> Result<()> {
        let assembly_result = self.remove_assembly_scratch().await;
        let parts_result = self.remove_parts_dir().await;
        assembly_result?;
        parts_result
    }

    pub async fn remove_all(&self) -> Result<()> {
        let final_result = remove_file_if_present(&self.final_zip).await;
        let assembly_result = self.remove_assembly_scratch().await;
        let parts_result = self.remove_parts_dir().await;
        final_result?;
        assembly_result?;
        parts_result
    }
}

async fn remove_file_if_present(path: &Path) -> Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

async fn remove_dir_if_present(path: &Path) -> Result<()> {
    match tokio::fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}
```

Register and re-export the helper from `archive_download/mod.rs`:

```rust
mod artifacts;
pub use artifacts::ArchiveArtifacts;
```

Keep the crate-root re-export in `lib.rs`:

```rust
pub use archive_download::{ArchiveArtifacts, ArchiveDownloadOptions};
```

- [ ] **Step 4: Run artifact tests and capture GREEN**

```powershell
cargo test -p eh_client --lib archive_download::artifacts::tests::archive_artifacts -- --nocapture
```

Expected: both artifact tests pass, including recursive directory removal and the second idempotent call. All cleanup members are attempted before the first encountered error is returned, so one locked file does not prevent cleanup of the remaining family.

- [ ] **Step 5: Record the suggested commit boundary**

Suggested boundary, requiring explicit user authorization: `feat(eh_client): model EH archive artifact families`. Review only path derivation, family recognition, and idempotent cleanup.

---

### Task 3: Implement manifest invariants and dynamic split policy

**Files:**
- Create/Test: `eh_client/src/archive_download/policy.rs`
- Create/Test: `eh_client/src/archive_download/manifest.rs`
- Modify: `eh_client/src/archive_download/mod.rs`
- Modify: `eh_client/Cargo.toml:9-29`

Add `mod manifest;` and `mod policy;` to `archive_download/mod.rs`. Expose only the `pub(super)` items consumed by the facade/coordinator; neither submodule becomes a crate-root public API.

- [ ] **Step 1: Add failing EWMA, adaptive-minimum, and split-selection tests**

Add tests that call the exact private functions below:

```rust
#[test]
fn ewma_and_adaptive_minimum_follow_fixed_policy() {
    assert_eq!(update_ewma(None, 100.0), 100.0);
    assert_eq!(update_ewma(Some(100.0), 200.0), 125.0);
    assert_eq!(adaptive_min_bytes(1.0), 1024 * 1024);
    assert_eq!(adaptive_min_bytes(100_000.0), 1_500_000);
}

#[test]
fn split_chooses_largest_sampled_active_interval_and_proportions_rates() {
    let parts = vec![
        split_input(0, 0, 10 * 1024 * 1024, 2 * 1024 * 1024, Some(2.0), true),
        split_input(1, 10 * 1024 * 1024, 16 * 1024 * 1024, 0, Some(1.0), true),
    ];
    let split = choose_split(&parts, 2, 3).unwrap();
    assert_eq!(split.part_id, 0);
    assert_eq!(split.new_rate, 1.0);
    assert_eq!(split.split_at, 7_689_557);
}

#[test]
fn split_clamps_children_and_requires_sample_slot_and_enough_tail() {
    let no_sample = vec![split_input(0, 0, 4 * 1024 * 1024, 0, None, true)];
    assert!(choose_split(&no_sample, 1, 2).is_none());

    let sampled = vec![split_input(
        0,
        0,
        2 * 1024 * 1024 - 1,
        0,
        Some(1.0),
        true,
    )];
    assert!(choose_split(&sampled, 1, 2).is_none());
    assert!(choose_split(&sampled, 2, 2).is_none());

    let clamp = vec![
        split_input(
            0,
            0,
            5 * 1024 * 1024,
            0,
            Some(100_000.0),
            true,
        ),
        split_input(
            1,
            5 * 1024 * 1024,
            6 * 1024 * 1024,
            0,
            Some(1.0),
            true,
        ),
    ];
    let split = choose_split(&clamp, 1, 2).unwrap();
    assert_eq!(split.split_at, 4 * 1024 * 1024);
}
```

Use this test constructor so every policy input is explicit:

```rust
fn split_input(
    id: u64,
    start: u64,
    end: u64,
    downloaded: u64,
    ewma: Option<f64>,
    active: bool,
) -> SplitInput {
    SplitInput {
        part_id: id,
        cursor: start + downloaded,
        end,
        ewma,
        active,
        has_stable_sample: ewma.is_some(),
    }
}
```

- [ ] **Step 2: Run policy tests and capture RED**

```powershell
cargo test -p eh_client --lib archive_download::policy::tests::ewma_and_adaptive_minimum -- --nocapture
cargo test -p eh_client --lib archive_download::policy::tests::split_ -- --nocapture
```

Expected: compilation fails because the policy types and functions are undefined.

- [ ] **Step 3: Implement the pure split-policy interface**

Add these constants/types/functions:

```rust
const EWMA_ALPHA: f64 = 0.25;
const MIN_SPLIT_BYTES: u64 = 1024 * 1024;
const TARGET_PART_SECONDS: f64 = 15.0;

#[derive(Debug, Clone, Copy)]
pub(super) struct SplitInput {
    pub(super) part_id: u64,
    pub(super) cursor: u64,
    pub(super) end: u64,
    pub(super) ewma: Option<f64>,
    pub(super) active: bool,
    pub(super) has_stable_sample: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct SplitPlan {
    pub(super) part_id: u64,
    pub(super) split_at: u64,
    pub(super) new_rate: f64,
}

pub(super) fn update_ewma(previous: Option<f64>, current: f64) -> f64 {
    previous.map_or(current, |old| {
        EWMA_ALPHA * current + (1.0 - EWMA_ALPHA) * old
    })
}

fn adaptive_min_bytes(rate: f64) -> u64 {
    ((rate * TARGET_PART_SECONDS).ceil() as u64).max(MIN_SPLIT_BYTES)
}

fn median_rate(mut rates: Vec<f64>) -> Option<f64> {
    if rates.is_empty() {
        return None;
    }
    rates.sort_by(f64::total_cmp);
    let middle = rates.len() / 2;
    Some(if rates.len() % 2 == 0 {
        (rates[middle - 1] + rates[middle]) / 2.0
    } else {
        rates[middle]
    })
}

pub(super) fn choose_split(
    parts: &[SplitInput],
    active_count: usize,
    max: usize,
) -> Option<SplitPlan> {
    if active_count >= max {
        return None;
    }
    let selected = parts
        .iter()
        .filter(|part| part.active && part.has_stable_sample && part.ewma.is_some())
        .filter(|part| part.cursor < part.end)
        .max_by_key(|part| part.end.saturating_sub(part.cursor))?;
    let selected_rate = selected.ewma?;
    let new_rate = median_rate(
        parts
            .iter()
            .filter(|part| {
                part.active && part.has_stable_sample && part.part_id != selected.part_id
            })
            .filter_map(|part| part.ewma)
            .collect(),
    )
    .unwrap_or(selected_rate);
    let cursor = selected.cursor;
    let remaining = selected.end - cursor;
    let selected_min = adaptive_min_bytes(selected_rate);
    let new_min = adaptive_min_bytes(new_rate);
    if remaining < selected_min.saturating_add(new_min) {
        return None;
    }
    let ideal_selected = ((remaining as f64) * selected_rate / (selected_rate + new_rate))
        .round() as u64;
    let selected_bytes = ideal_selected.clamp(selected_min, remaining - new_min);
    Some(SplitPlan {
        part_id: selected.part_id,
        split_at: cursor + selected_bytes,
        new_rate,
    })
}
```

For the proportional test inputs, `cursor = 2,097,152`, `remaining = 8,388,608`, selected/new rates are `2:1`, and the rounded selected share is `5,592,405`, producing split byte `7,689,557`. In the clamp test, the selected interval's ideal share exceeds `remaining - new_min`, so the split is clamped to exactly `4 MiB`.

- [ ] **Step 4: Add failing manifest round-trip, classification, and I/O-preservation tests**

Add one async round-trip/atomic replacement test and one table-driven recovery-classification test. Assert a missing manifest inside an existing parts directory, malformed JSON, unsupported version, zero total, URL mismatch, empty parts, gap, overlap, zero-length interval, end beyond total, duplicate ID, `next_part_id` not above every ID, simultaneous ETag and Last-Modified, weak/empty ETag, empty Last-Modified, missing referenced file, and oversized referenced file each return the corresponding `ManifestRecovery::Invalid(...)` variant rather than an outer error. Include a named `incomplete_final_coverage` row whose last interval ends below `total_len` and assert exactly `ManifestInvalid::InvalidIntervalCoverage`; keep the independent stale `next_part_id` row asserting exactly `ManifestInvalid::InvalidNextPartId`. Also create `part-0000000000000099` and `manifest.json.tmp` in a valid directory and assert successful recovery removes both without removing referenced part files.

Add `manifest_recovery_io_error_propagates_and_preserves_state`: create `manifest.json` as a directory (portable fault injection yielding a read/permission-like I/O error), create a sentinel part file, call `recover_manifest`, assert the outer result is `Err(Error::Io(_))`, and assert the manifest directory plus sentinel part still exist. This function-level test locks the non-purgeable classification before facade behavior is added in Task 6.

Use this exact valid fixture:

```rust
fn valid_manifest(url: &str) -> ArchiveManifest {
    ArchiveManifest {
        version: 1,
        download_url: url.to_owned(),
        total_len: 8,
        etag: Some("\"strong-v1\"".to_owned()),
        last_modified: None,
        next_part_id: 2,
        parts: vec![
            ManifestPart { id: 0, start: 0, end: 4 },
            ManifestPart { id: 1, start: 4, end: 8 },
        ],
    }
}

#[test]
fn manifest_interval_and_next_id_failures_are_classified_independently() {
    let url = "https://example.invalid/archive.zip";
    let mut incomplete_final_coverage = valid_manifest(url);
    incomplete_final_coverage.parts[1].end = 7;
    let mut stale_next_part_id = valid_manifest(url);
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
        assert_eq!(manifest.validate_shape(url), Err(expected), "{name}");
    }
}
```

- [ ] **Step 5: Run manifest tests and capture RED**

```powershell
cargo test -p eh_client --lib archive_download::manifest::tests::manifest_ -- --nocapture
```

Expected: compilation fails because `ArchiveManifest`, `ManifestPart`, `ManifestRecovery`, and recovery I/O functions are undefined.

- [ ] **Step 6: Implement versioned manifest validation and atomic replacement**

Move `tempfile = "3"` from `[dev-dependencies]` to `[dependencies]`; keep `wiremock` and the test Tokio entry under `[dev-dependencies]`. This promotes an already-resolved crate rather than adding a package. Then add the manifest types from the core contract and these methods/signatures:

```rust
use super::artifacts::ArchiveArtifacts;
use crate::error::{Error, Result};
use std::path::PathBuf;

const MANIFEST_VERSION: u32 = 1;
const MANIFEST_FILE: &str = "manifest.json";
const MANIFEST_TEMP_PREFIX: &str = "manifest.json.tmp-";

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
        let mut ids = std::collections::HashSet::new();
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
            Error::Other(format!("refusing to persist invalid archive manifest: {reason:?}"))
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
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ManifestRecovery::Invalid(ManifestInvalid::MissingManifest));
        }
        Err(error) => return Err(error.into()),
    };
    let manifest: ArchiveManifest = match serde_json::from_slice(&bytes) {
        Ok(manifest) => manifest,
        Err(_parse_error) => {
            return Ok(ManifestRecovery::Invalid(ManifestInvalid::MalformedJson));
        }
    };
    if let Err(reason) = manifest.validate_shape(current_url) {
        return Ok(ManifestRecovery::Invalid(reason));
    }
    for part in &manifest.parts {
        let metadata = match tokio::fs::metadata(ArchiveManifest::part_path(artifacts, part.id)).await {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => {
                return Ok(ManifestRecovery::Invalid(
                    ManifestInvalid::MissingReferencedPart { part_id: part.id },
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
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
```

Implement `cleanup_unreferenced_parts(artifacts, manifest) -> Result<()>` by reading only the top level of `parts_dir`, retaining `manifest.json` and exact referenced `part-{id:016}` names, and removing every other file or directory, including abandoned `manifest.json.tmp-*` files. File deletion uses `tokio::fs::remove_file`; unexpected directories use `tokio::fs::remove_dir_all`. Every read-directory/entry/remove error propagates through the outer `Result`; this function never calls artifact-family purge. Error text must identify only the operation and part ID, never the URL, manifest body, or full path.

`write_atomic` itself sorts and validates before every publish. During initialization create the empty part file before publishing the manifest. During split create the new empty part file before writing the updated manifest; if manifest replacement fails, delete that new unreferenced file and return the I/O error.

- [ ] **Step 7: Run all policy/manifest tests and capture GREEN**

```powershell
cargo test -p eh_client --lib archive_download::policy::tests -- --nocapture
cargo test -p eh_client --lib archive_download::manifest::tests -- --nocapture
```

Expected: EWMA, floors, proportional/clamped split, no-split guards, manifest round trip, atomic replacement, every deterministic invalid classification, file-length checks, unreferenced cleanup, and operational-I/O state preservation pass.

- [ ] **Step 8: Record the suggested commit boundary**

Suggested boundary, requiring explicit user authorization: `feat(eh_client): add multipart manifest and split policy`. Review pure policy and crash-consistent structural state without HTTP behavior.

---

### Task 4: Extract and preserve the sequential transfer path

**Files:**
- Create/Test: `eh_client/src/archive_download/sequential.rs`
- Create/Test: `eh_client/src/archive_download/http.rs`
- Modify: `eh_client/src/archive_download/mod.rs`
- Modify/Test: `eh_client/src/client.rs:1-25,167-229,457-708,1171-1262`

Add `mod http;`, `mod sequential;`, `pub(crate) use http::archive_http_error;`, and `pub(crate) use sequential::download_sequential;` to `archive_download/mod.rs`; these crate-private re-exports expose the real implementations to `client.rs` without defining same-signature forwarding functions. `http.rs` owns `is_ehentai_host`, `archive_get`, and `archive_http_error`; these helpers enforce authentication/error-sanitization policy rather than merely forwarding arguments.

- [ ] **Step 1: Run the existing sequential characterization set before refactoring**

```powershell
cargo test -p eh_client --test integration test_download_archive_resumes_existing_partial_file -- --nocapture
cargo test -p eh_client --test integration test_download_archive_restarts_complete_partial_on_416 -- --nocapture
cargo test -p eh_client --test integration test_download_archive_rejects_mismatched_content_range -- --nocapture
cargo test -p eh_client --test integration test_download_archive_returns_download_in_progress_when_fast_partial -- --nocapture
```

Expected baseline: each command reports `1 passed; 0 failed`. Preserve these outputs as the pre-refactor behavior receipt.

- [ ] **Step 2: Move shared HTTP policy and sequential internals into their owned modules**

Move retry/progress/Range helpers from `client.rs` into `archive_download/sequential.rs` without changing values or behavior:

```rust
const ARCHIVE_DOWNLOAD_MAX_ATTEMPTS: usize = 4;
const ARCHIVE_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(1);
const PROGRESS_THRESHOLD_BYTES_PER_SEC: f64 = 10_240.0;

pub(super) fn made_progress(new_bytes: u64, elapsed_secs: f64) -> bool;
fn response_expected_total(
    headers: &reqwest::header::HeaderMap,
    existing_len: u64,
    append: bool,
) -> Option<u64>;
fn parse_content_range_header(value: &str) -> Option<(u64, u64, Option<u64>)>;
fn validate_sequential_content_range(
    headers: &reqwest::header::HeaderMap,
    existing_len: u64,
) -> Result<u64>;
```

Move EH host/auth/error policy into `archive_download/http.rs` with concrete ownership:

```rust
use crate::error::Error;

pub(super) fn is_ehentai_host(url: &str) -> bool {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .is_some_and(|host| matches!(host.as_str(), "e-hentai.org" | "exhentai.org"))
}

pub(super) fn archive_get(
    http: &reqwest::Client,
    cookies: &crate::models::EhCookies,
    download_url: &str,
) -> reqwest::RequestBuilder {
    let request = http.get(download_url);
    if is_ehentai_host(download_url) {
        request.header(reqwest::header::COOKIE, cookies.to_header())
    } else {
        request
    }
}

pub(crate) fn archive_http_error(error: reqwest::Error) -> Error {
    Error::Http(error.without_url())
}
```

In `sequential.rs`, import `super::http::{archive_get, archive_http_error}`, `crate::error::{Error, Result}`, and `std::path::Path`. Implement the extracted entry points with an optional already-received `200` response so multipart initialization can later hand an ignored-Range response to the sequential writer without issuing a second full request:

```rust
pub(crate) async fn download_sequential(
    http: &reqwest::Client,
    cookies: &crate::models::EhCookies,
    download_url: &str,
    temp_path: &Path,
    mut first_response: Option<reqwest::Response>,
) -> Result<()>;

async fn download_sequential_once(
    http: &reqwest::Client,
    cookies: &crate::models::EhCookies,
    download_url: &str,
    temp_path: &Path,
    response: Option<reqwest::Response>,
) -> Result<()>;
```

The retry loop must retain the current `initial_len`, `attempts`, `had_progress`, per-attempt file-length delta, one-second delay, and final `Error::DownloadInProgress { inner, attempts, bytes_delta, elapsed }`. Consume `first_response.take()` only on attempt one. `download_sequential_once` must retain the current `416` partial deletion, exact append validation, `200` truncate/restart, 2 MiB `BufWriter`, best-effort flush after stream failure, and expected-total check. Build every GET with `http::archive_get`; route reqwest send/body errors through `archive_http_error` (or `error.without_url()` before wrapping as I/O) so existing warning/error chains cannot print token-bearing redirect URLs.

- [ ] **Step 3: Delegate the old client methods to the extracted sequential function**

Keep both old public method signatures byte-for-byte unchanged. Import `crate::archive_download::{download_sequential, ArchiveArtifacts}` in `client.rs`; retain the POST/redirect logic in `download_archive_with_request` and replace only the transfer call:

```rust
let artifacts = ArchiveArtifacts::new(dest);
download_sequential(
    &self.http,
    &self.cookies,
    &download_url,
    artifacts.assembly_scratch(),
    None,
)
.await?;
```

Keep `validate_complete_zip`, temporary-file deletion after validation error, total metadata read, and atomic rename in `client.rs`. Move the `made_progress` tests into `sequential.rs` and EH-host tests into `http.rs`; leave unrelated URL/client tests in place.

- [ ] **Step 4: Run the characterization set after refactoring**

Run all four Step 1 commands again.

```powershell
cargo test -p eh_client --lib archive_download::sequential::tests -- --nocapture
cargo test -p eh_client --lib archive_download::http::tests -- --nocapture
```

Expected GREEN: each characterization command still reports `1 passed; 0 failed`; sequential and HTTP-policy unit tests pass; Range headers, conditional Cookie behavior, four-attempt behavior, partial preservation/deletion, URL sanitization, and `DownloadInProgress` fields remain unchanged.

- [ ] **Step 5: Run the unauthenticated fallback regression**

```powershell
cargo test -p eh_client --test integration test_download_gallery_images -- --nocapture
```

Expected: all four existing `test_download_gallery_images_*` tests pass, proving this refactor did not enter the no-login path.

- [ ] **Step 6: Record the suggested commit boundary**

Suggested boundary, requiring explicit user authorization: `refactor(eh_client): isolate sequential archive transfer`. Review movement and compatibility only; no multipart behavior belongs in this boundary.

---

### Task 5: Define exact bounded Range state, validation, and aggregate progress

**Files:**
- Modify: `eh_client/Cargo.toml:20-21`
- Create/Test: `eh_client/src/archive_download/part.rs`
- Modify: `eh_client/src/archive_download/mod.rs`

Add `mod part;` to `archive_download/mod.rs`. `part.rs` consumes manifest schema and the shared URL-sanitized error conversion through `pub(super)` interfaces; it does not own archive-level scheduling.

- [ ] **Step 1: Enable source-level Tokio coordination features**

Change the normal Tokio dependency to:

```toml
tokio = { version = "1", features = ["fs", "io-util", "macros", "rt", "sync", "time"] }
```

Do not add another dependency and do not change the existing test Tokio entry; Task 3 already promoted the existing `tempfile` dependency.

- [ ] **Step 2: Add failing validator and exact-range unit tests**

Add tests for these exact cases:

1. `206 bytes 10-19/100` accepts request `[10, 20)` and total `100`.
2. Start `11`, end `18`, missing total, total `101`, `200`, and `416` each return `PartFailureKind::RestartSequential`.
3. A strong ETag stores/sends `"v1"`; a weak ETag is ignored in favor of Last-Modified.
4. Existing strong ETag mismatch, missing ETag, Last-Modified mismatch, and missing Last-Modified each request restart.
5. No validator adds no `If-Range` and accepts an otherwise exact response.

The tests target these signatures:

```rust
pub(super) fn select_validator(headers: &reqwest::header::HeaderMap) -> Validator;
fn validate_part_response(
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
    requested_start: u64,
    requested_end: u64,
    total_len: u64,
    validator: &Validator,
) -> std::result::Result<(), PartFailure>;
```

- [ ] **Step 3: Run response-validation tests and capture RED**

```powershell
cargo test -p eh_client --lib archive_download::part::tests::part_response -- --nocapture
cargo test -p eh_client --lib archive_download::part::tests::validator -- --nocapture
```

Expected: compilation fails because validator and part failure types do not exist.

- [ ] **Step 4: Implement validators and reset classification**

In `part.rs`, import `super::artifacts::ArchiveArtifacts`, `super::http::{archive_get, archive_http_error}`, `super::manifest::{ArchiveManifest, ManifestPart}`, `crate::error::{Error, Result}`, and `std::path::PathBuf`. Use these archive-internal types:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Validator {
    StrongEtag(String),
    LastModified(String),
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PartFailureKind {
    Retryable,
    RestartSequential,
}

#[derive(Debug)]
pub(super) struct PartFailure {
    pub(super) kind: PartFailureKind,
    pub(super) error: Error,
    pub(super) attempts: usize,
}
```

`select_validator` accepts ETag only when its trimmed value is non-empty and does not start with `W/`; otherwise it selects a non-empty trimmed Last-Modified value, then `None`. Add exact manifest conversion so initialization persists only the selected validator and recovery reconstructs the same policy:

```rust
impl Validator {
    pub(super) fn store_in_manifest(&self, manifest: &mut ArchiveManifest) {
        manifest.etag = None;
        manifest.last_modified = None;
        match self {
            Validator::StrongEtag(value) => manifest.etag = Some(value.clone()),
            Validator::LastModified(value) => manifest.last_modified = Some(value.clone()),
            Validator::None => {}
        }
    }

    pub(super) fn from_manifest(manifest: &ArchiveManifest) -> Self {
        if let Some(value) = &manifest.etag {
            Validator::StrongEtag(value.clone())
        } else if let Some(value) = &manifest.last_modified {
            Validator::LastModified(value.clone())
        } else {
            Validator::None
        }
    }
}
```

`validate_part_response` requires status `206`, exact inclusive end `requested_end - 1`, exact total, and the selected validator's exact response header. Every protocol/status/validator failure is `RestartSequential`; reqwest send/body errors remain `Retryable` but must be converted with `archive_http_error`/`without_url()` before storage in `PartFailure`.

- [ ] **Step 5: Add failing pure part-state and aggregate tests**

In colocated `part.rs` tests, assert `requested_range([0, 100), 12)` returns `[12, 100)` (rendered later as `bytes=12-99`), exact completion returns `None`, and a file length above the interval returns `RestartSequential`. Add `part_sample_rate_eligibility_requires_one_second_and_nonzero_delta`: a 500 ms reconciliation with a non-zero window delta is ineligible, a one-second non-zero window is eligible, and a one-second zero-delta flush is ineligible. Create two part files and assert `aggregate_downloaded_bytes` sums their metadata lengths exactly once and rejects an oversized file. These are filesystem/state tests, not HTTP integration tests.

```rust
#[test]
fn part_sample_rate_eligibility_requires_one_second_and_nonzero_delta() {
    let sample = |window_delta, elapsed| PartSample {
        part_id: 1,
        generation: 2,
        durable_len: 64,
        window_delta,
        elapsed,
    };

    assert!(!sample(64, std::time::Duration::from_millis(500)).is_rate_eligible());
    assert!(sample(64, std::time::Duration::from_secs(1)).is_rate_eligible());
    assert!(!sample(0, std::time::Duration::from_secs(1)).is_rate_eligible());
}
```

- [ ] **Step 6: Run part-state tests and capture RED**

```powershell
cargo test -p eh_client --lib archive_download::part::tests::requested_range -- --nocapture
cargo test -p eh_client --lib archive_download::part::tests::part_sample_rate_eligibility -- --nocapture
cargo test -p eh_client --lib archive_download::part::tests::aggregate_downloaded_bytes -- --nocapture
```

Expected: compilation fails because part event/range/aggregate functions are undefined.

- [ ] **Step 7: Implement part event contracts, exact request bounds, and durable aggregate accounting**

Use these contracts; the actual HTTP loop is implemented after failing real-surface tests in Task 6:

```rust
#[derive(Debug)]
pub(super) struct PartSample {
    pub(super) part_id: u64,
    pub(super) generation: u64,
    /// Absolute current length of this part file, read from metadata after flush.
    pub(super) durable_len: u64,
    /// Non-overlapping bytes written since the previous successfully emitted flush event.
    pub(super) window_delta: u64,
    /// Transfer-window duration only; retry sleep is never included.
    pub(super) elapsed: std::time::Duration,
}

impl PartSample {
    pub(super) fn is_rate_eligible(&self) -> bool {
        self.window_delta > 0 && self.elapsed >= std::time::Duration::from_secs(1)
    }
}

#[derive(Debug)]
pub(super) struct PartSampleEvent {
    pub(super) sample: PartSample,
    /// The worker waits for this acknowledgement before retry backoff or more I/O.
    pub(super) applied: tokio::sync::oneshot::Sender<()>,
}

#[derive(Debug)]
pub(super) enum PartExit {
    Complete { attempts_used: usize },
    Paused { attempts_used: usize },
    Failed(PartFailure),
}

pub(super) struct SeedResponse {
    pub(super) response: reqwest::Response,
    pub(super) request_started_at: std::time::Instant,
}

fn requested_range(
    part: &ManifestPart,
    downloaded: u64,
) -> std::result::Result<Option<(u64, u64)>, PartFailureKind>;

pub(super) async fn aggregate_downloaded_bytes(
    artifacts: &ArchiveArtifacts,
    manifest: &ArchiveManifest,
) -> Result<u64>;
```

`PartSample.durable_len` is part-relative absolute file length, not an archive-global offset. `window_delta` covers exactly the bytes since the preceding successfully emitted flush event; the first event begins at the metadata length observed when the worker attempt starts. Every successful worker flush emits this pair, including a transient-error flush shorter than one second. Such a short event reconciles the cursor but is not a rate sample. The enclosing `PartSampleEvent` is an apply barrier: the worker may not enter retry backoff, send another request, or continue reading until the coordinator has applied/rejected the event and sent `applied`. `requested_range` returns `Ok(None)` only at exact completion, returns `Err(RestartSequential)` when `downloaded > part.len()`, and otherwise returns `(part.start + downloaded, part.end)`. `aggregate_downloaded_bytes` sums metadata lengths for each manifest part exactly once and rejects a length greater than the interval. It never sums response counters.

- [ ] **Step 8: Run worker/validation tests and capture GREEN**

```powershell
cargo test -p eh_client --lib archive_download::part::tests::part_response -- --nocapture
cargo test -p eh_client --lib archive_download::part::tests::validator -- --nocapture
cargo test -p eh_client --lib archive_download::part::tests::requested_range -- --nocapture
cargo test -p eh_client --lib archive_download::part::tests::part_sample_rate_eligibility -- --nocapture
cargo test -p eh_client --lib archive_download::part::tests::aggregate_downloaded_bytes -- --nocapture
```

Expected: exact response validation, validator mapping, durable-offset Range bounds, completion/oversize classification, short-versus-stable sample eligibility, and aggregate progress pass. The HTTP worker reconciliation test is added only after `run_part` exists in Task 6.

- [ ] **Step 9: Record the suggested commit boundary**

Suggested boundary, requiring explicit user authorization: `feat(eh_client): define bounded archive part state`. Review exact response/range policy and durable accounting only; HTTP request/retry/pause execution belongs to Task 6's real-surface boundary.

---

### Task 6: Add dynamic coordinator, options-based API, assembly, and success cleanup

**Files:**
- Create: `eh_client/src/archive_download/initialization.rs`
- Create/Test: `eh_client/src/archive_download/coordinator.rs`
- Create/Test: `eh_client/src/archive_download/assembly.rs`
- Modify/Test: `eh_client/src/archive_download/mod.rs`
- Modify/Test: `eh_client/src/archive_download/part.rs`
- Modify: `eh_client/src/client.rs:457-531`
- Modify: `eh_client/src/lib.rs`
- Modify/Test: `eh_client/tests/integration.rs`

Add `mod assembly;`, `mod coordinator;`, and `mod initialization;` to `archive_download/mod.rs`. Once `client.rs` delegates to the options facade in Step 7, replace Task 4's temporary `pub(crate) use sequential::download_sequential` with a private `use self::sequential::download_sequential`; retain only the crate-private `archive_http_error` re-export needed by the POST path. The facade calls concrete functions owned by these modules; do not duplicate their bodies in forwarding wrappers.

Use direct sibling interfaces: `initialization.rs` imports `artifacts::ArchiveArtifacts`, `http::archive_get`, `manifest::{ArchiveManifest, ManifestPart}`, `part::{select_validator, SeedResponse, Validator}`, and `crate::error::{Error, Result}`; `coordinator.rs` imports `artifacts::ArchiveArtifacts`, `manifest::{ArchiveManifest, ManifestPart}`, `part::{aggregate_downloaded_bytes, run_part, PartExit, PartSample, PartSampleEvent, SeedResponse, Validator}`, `policy::{choose_split, update_ewma, SplitInput, SplitPlan}`, `sequential::made_progress`, and `crate::error::{Error, Result}`; `assembly.rs` imports only `artifacts::ArchiveArtifacts`, `manifest::ArchiveManifest`, and `crate::error::Result`.

- [ ] **Step 1: Add a stateful wiremock Range responder**

In `eh_client/tests/integration.rs`, extend imports with `ArchiveDownloadOptions`, atomics, `Arc`, `Mutex`, and `Duration`. Implement a `ScriptedRangeResponder` that:

- parses `Range: bytes=start-end` and `bytes=start-`;
- records every Range string and `If-Range` value;
- returns exact `Content-Range` and optional ETag/Last-Modified;
- can delay and truncate the first open-ended response to 64 KiB after at least 1100 ms;
- can delay and truncate the first low-start bounded response;
- counts delayed bounded responses with `AtomicUsize`, updates `max_active` using `fetch_max`, and uses a test thread to decrement only after its matching `ResponseTemplate::set_delay` expires;
- slices all full responses byte-for-byte from one `Arc<Vec<u8>>`.

The responder's production-shaped entry point must be:

```rust
impl wiremock::Respond for ScriptedRangeResponder {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let range = request
            .headers
            .get("range")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
            .expect("multipart request must include Range");
        let (start, end) = parse_test_range(&range, self.bytes.len() as u64);
        let if_range = request
            .headers
            .get("if-range")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        self.record_request(&range, if_range);
        self.response_for(start, end, range.ends_with('-'))
    }
}
```

Keep test-only parsing and counters in `integration.rs`; do not expose production internals to integration tests.

Also add a persisted-state helper used by recovery and reset tests:

```rust
fn seed_multipart_manifest(
    dest: &std::path::Path,
    download_url: &str,
    total_len: u64,
    etag: Option<&str>,
    last_modified: Option<&str>,
    parts: &[(u64, u64, u64, &[u8])],
) -> std::path::PathBuf;
```

Each tuple is `(id, start, end, downloaded_prefix)`. The helper creates `dest.with_extension("zip.parts")`, writes every stable `part-{id:016}` file, writes `version: 1`, sets `next_part_id` to one more than the maximum ID, and returns the parts directory.

- [ ] **Step 2: Write failing dynamic-transfer wiremock tests**

Add these tests, all calling the new request-options method with `ArchiveDownloadOptions { max_concurrency: N }`:

1. `test_multipart_starts_with_one_open_ended_range_and_assembles_exact_zip` — first GET is exactly `bytes=0-`, no bounded request exists before its sample, final bytes equal source, `.zip.part` and `.zip.parts` are absent after success.
2. `test_multipart_dynamic_split_uses_two_actual_connections_without_overlap` — a delayed/truncated first response yields bounded child ranges, `max_active == 2`, request history proves the selected task was joined and relaunched from its durable file length, and final effective intervals are gap-free/non-overlapping over `[0, total_len)`.
3. `test_multipart_completion_reuses_slot_by_splitting_largest_remaining_interval` — with maximum `3`, a fast high interval completes while a sampled low interval remains; request history proves another split of that low interval and `max_active <= 3`.
4. `test_multipart_small_archive_does_not_force_configured_slot_count` — remaining bytes below two adaptive minima produce no child request even with maximum `4`.
5. `test_multipart_restart_resumes_offsets_with_strong_etag_even_when_limit_is_one` — seed a valid manifest, call with maximum one, assert stale assembly is deleted before requests, exact Range starts after existing bytes, `If-Range` is the strong ETag, no second active part exists, and final ZIP matches.
6. `test_multipart_restart_without_validator_uses_url_total_and_content_range` — seed without validators, assert no `If-Range`, exact resumed Range/total validation, and successful byte assembly.
7. `test_multipart_recovery_removes_unreferenced_files_after_validation` — referenced files survive while an unreferenced stable part and `manifest.json.tmp` disappear.
8. `test_malformed_manifest_clears_all_multipart_state_and_starts_sequentially` — corrupt JSON, a gap, a missing referenced file, and an oversized part file each remove the invalid directory plus stale assembly before one no-Range `200` request from byte zero; final ZIP matches and no multipart artifacts remain.
9. `test_archive_options_zero_is_rejected_before_archiver_post` — call the request-options method with zero, assert the validation message, and assert wiremock received no request.
10. `test_manifest_recovery_io_error_preserves_state_and_skips_archive_get` — after the archiver POST is mounted, create `manifest.json` as a directory plus a sentinel part and stale assembly, call the options method, assert an I/O error, assert no archive GET (Range or sequential) was received, and assert every existing artifact remains. This is the real-facade counterpart to Task 3's fault-injected recovery unit test.
11. `test_multipart_part_retries_from_durable_offset_after_truncated_body` — seed one incomplete interval, truncate the first bounded body after durable bytes, then assert the next request starts at `part.start + file_len` and final ZIP is exact.
12. `test_multipart_part_exhausts_four_attempts_and_preserves_state` — seed one interval, make four bounded attempts fail transiently, record `Instant` at the responder and assert exactly four requests with each adjacent timestamp at least 900 ms apart, assert no fifth request, and assert manifest/part files remain.

Create stored ZIP fixtures with payload sizes of 4 MiB for two-way split and 8 MiB for completion-triggered work stealing so the one-MiB floors are deterministic.

- [ ] **Step 3: Run dynamic tests and capture RED**

```powershell
cargo test -p eh_client --test integration multipart_starts_with_one_open_ended_range -- --nocapture
cargo test -p eh_client --test integration multipart_dynamic_split -- --nocapture
cargo test -p eh_client --test integration multipart_completion_reuses_slot -- --nocapture
cargo test -p eh_client --test integration multipart_restart -- --nocapture
cargo test -p eh_client --test integration multipart_recovery -- --nocapture
cargo test -p eh_client --test integration malformed_manifest -- --nocapture
cargo test -p eh_client --test integration archive_options_zero -- --nocapture
cargo test -p eh_client --test integration manifest_recovery_io_error -- --nocapture
cargo test -p eh_client --test integration multipart_part_ -- --nocapture
```

Expected: compilation fails because the options-based method and multipart coordinator/recovery path are absent.

- [ ] **Step 4: Lock retry-boundary reconciliation and coordinator ownership with unit tests, then implement initialization/task state**

In `part.rs`, add the one focused private-channel HTTP test `part_worker_emits_durable_reconciliation_before_retry_after_transient_error`. Its short `206` body is a transient incomplete-body failure: the test must receive the post-flush absolute length and window delta while only one request has occurred, then pause the worker during its backoff so no retry can hide an ordering bug.

```rust
#[tokio::test]
async fn part_worker_emits_durable_reconciliation_before_retry_after_transient_error() {
    use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("Content-Range", "bytes 0-7/8")
                .set_body_bytes(b"abc"),
        )
        .mount(&server)
        .await;
    let temp = tempfile::tempdir().unwrap();
    let part_path = temp.path().join("part-0000000000000004");
    tokio::fs::write(&part_path, b"").await.unwrap();
    let (pause_tx, pause_rx) = tokio::sync::watch::channel(false);
    let (samples_tx, mut samples_rx) = tokio::sync::mpsc::unbounded_channel();
    let worker = tokio::spawn(run_part(
        reqwest::Client::new(),
        crate::models::EhCookies::default(),
        format!("{}/archive", server.uri()),
        8,
        Validator::None,
        ManifestPart { id: 4, start: 0, end: 8 },
        part_path,
        0,
        7,
        None,
        pause_rx,
        samples_tx,
    ));

    let event = tokio::time::timeout(
        std::time::Duration::from_millis(900),
        samples_rx.recv(),
    )
    .await
    .expect("flush reconciliation must precede the one-second retry delay")
    .expect("worker must emit reconciliation");
    let PartSampleEvent { sample, applied } = event;
    assert_eq!(sample.part_id, 4);
    assert_eq!(sample.generation, 7);
    assert_eq!(sample.durable_len, 3);
    assert_eq!(sample.window_delta, 3);
    assert!(sample.elapsed < std::time::Duration::from_secs(1));
    assert!(!sample.is_rate_eligible());
    assert_eq!(server.received_requests().await.unwrap().len(), 1);

    applied.send(()).unwrap();
    pause_tx.send(true).unwrap();
    let (_, exit) = worker.await.unwrap();
    assert!(matches!(exit, PartExit::Paused { .. }));
}
```

In `coordinator.rs`, add `coordinator_recovery_queues_incomplete_parts_in_start_order_with_limit`, `coordinator_retry_boundary_flush_updates_cursor_once_without_polluting_ewma`, `coordinator_stale_generation_sample_updates_neither_downloaded_nor_ewma`, and `coordinator_invalid_absolute_samples_are_non_mutating`. The recovery test constructs completed/incomplete runtime parts and asserts only the earliest `max_concurrency` incomplete IDs become active while the rest remain in `pending_recovered`. Use this exact state fixture and assertions for the sample tests:

```rust
fn sample_runtime() -> RuntimePart {
    RuntimePart {
        part: ManifestPart { id: 4, start: 100, end: 500 },
        downloaded: 25,
        ewma: Some(100.0),
        attempts_used: 0,
        active: true,
        has_stable_sample: false,
        generation: 7,
    }
}

#[test]
fn coordinator_retry_boundary_flush_updates_cursor_once_without_polluting_ewma() {
    let mut runtime = sample_runtime();
    let short_retry_flush = PartSample {
        part_id: 4,
        generation: 7,
        durable_len: 89,
        window_delta: 64,
        elapsed: std::time::Duration::from_millis(500),
    };

    assert_eq!(
        apply_part_sample(&mut runtime, &short_retry_flush).unwrap(),
        SampleDisposition::Reconciled
    );
    assert_eq!(runtime.downloaded, 89);
    assert_eq!(runtime.split_input().cursor, 189);
    assert_eq!(runtime.ewma, Some(100.0));
    assert!(!runtime.has_stable_sample);

    let snapshot = runtime.clone();
    assert!(apply_part_sample(&mut runtime, &short_retry_flush).is_err());
    assert_eq!(runtime, snapshot);

    let stable_later_window = PartSample {
        part_id: 4,
        generation: 7,
        durable_len: 153,
        window_delta: 64,
        elapsed: std::time::Duration::from_secs(1),
    };

    assert_eq!(
        apply_part_sample(&mut runtime, &stable_later_window).unwrap(),
        SampleDisposition::Stable
    );
    assert_eq!(runtime.downloaded, 153);
    assert_eq!(runtime.split_input().cursor, 253);
    assert_eq!(runtime.ewma, Some(91.0));
    assert!(runtime.has_stable_sample);
}

#[test]
fn coordinator_stale_generation_sample_updates_neither_downloaded_nor_ewma() {
    let mut runtime = sample_runtime();
    let snapshot = runtime.clone();
    let stale = PartSample {
        part_id: 4,
        generation: 6,
        durable_len: 89,
        window_delta: 64,
        elapsed: std::time::Duration::from_secs(1),
    };

    assert_eq!(
        apply_part_sample(&mut runtime, &stale).unwrap(),
        SampleDisposition::Ignored
    );
    assert_eq!(runtime, snapshot);
}

#[test]
fn coordinator_invalid_absolute_samples_are_non_mutating() {
    let cases = [
        (
            "beyond_part_length",
            PartSample {
                part_id: 4,
                generation: 7,
                durable_len: 401,
                window_delta: 376,
                elapsed: std::time::Duration::from_secs(1),
            },
        ),
        (
            "checked_add_overflow",
            PartSample {
                part_id: 4,
                generation: 7,
                durable_len: 25,
                window_delta: u64::MAX,
                elapsed: std::time::Duration::from_secs(1),
            },
        ),
        (
            "absolute_regression",
            PartSample {
                part_id: 4,
                generation: 7,
                durable_len: 24,
                window_delta: 0,
                elapsed: std::time::Duration::from_millis(100),
            },
        ),
        (
            "mismatched_window_delta",
            PartSample {
                part_id: 4,
                generation: 7,
                durable_len: 31,
                window_delta: 5,
                elapsed: std::time::Duration::from_millis(100),
            },
        ),
    ];

    for (name, sample) in cases {
        let mut runtime = sample_runtime();
        let snapshot = runtime.clone();
        assert!(apply_part_sample(&mut runtime, &sample).is_err(), "{name}");
        assert_eq!(runtime, snapshot, "{name}");
    }
}
```

```powershell
cargo test -p eh_client --lib archive_download::part::tests::part_worker_emits_durable_reconciliation -- --nocapture
cargo test -p eh_client --lib archive_download::coordinator::tests::coordinator_ -- --nocapture
```

Expected RED: the part-worker test fails because the flush event/barrier and HTTP loop are absent, and coordinator tests fail because `SampleDisposition`, absolute reconciliation, and coordinator-owned runtime/task types are absent.

Add these private types/signatures:

```rust
// coordinator.rs
#[derive(Debug, Clone, PartialEq)]
struct RuntimePart {
    part: ManifestPart,
    downloaded: u64,
    ewma: Option<f64>,
    attempts_used: usize,
    active: bool,
    has_stable_sample: bool,
    generation: u64,
}

impl RuntimePart {
    fn split_input(&self) -> SplitInput {
        SplitInput {
            part_id: self.part.id,
            cursor: self.part.start + self.downloaded,
            end: self.part.end,
            ewma: self.ewma,
            active: self.active,
            has_stable_sample: self.has_stable_sample,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SampleDisposition {
    Ignored,
    Reconciled,
    Stable,
}

fn apply_part_sample(
    runtime: &mut RuntimePart,
    sample: &PartSample,
) -> Result<SampleDisposition> {
    if sample.part_id != runtime.part.id
        || !runtime.active
        || sample.generation != runtime.generation
    {
        return Ok(SampleDisposition::Ignored);
    }
    if sample.durable_len > runtime.part.len() {
        return Err(Error::Other(format!(
            "EH archive part {} durable length exceeds its interval",
            runtime.part.id
        )));
    }
    if sample.durable_len < runtime.downloaded {
        return Err(Error::Other(format!(
            "EH archive part {} durable length regressed",
            runtime.part.id
        )));
    }
    let expected_durable_len = runtime
        .downloaded
        .checked_add(sample.window_delta)
        .ok_or_else(|| {
            Error::Other("EH archive part sample byte counter overflow".to_owned())
        })?;
    if expected_durable_len != sample.durable_len {
        return Err(Error::Other(format!(
            "EH archive part {} durable length disagrees with its window delta",
            runtime.part.id
        )));
    }

    let updated_ewma = sample.is_rate_eligible().then(|| {
        let current = sample.window_delta as f64 / sample.elapsed.as_secs_f64();
        update_ewma(runtime.ewma, current)
    });
    runtime.downloaded = sample.durable_len;
    if let Some(ewma) = updated_ewma {
        runtime.ewma = Some(ewma);
        runtime.has_stable_sample = true;
        Ok(SampleDisposition::Stable)
    } else {
        Ok(SampleDisposition::Reconciled)
    }
}

struct ActivePart {
    pause: tokio::sync::watch::Sender<bool>,
}

pub(super) struct MultipartCoordinator {
    http: reqwest::Client,
    cookies: crate::models::EhCookies,
    download_url: String,
    artifacts: ArchiveArtifacts,
    manifest: ArchiveManifest,
    runtime_parts: Vec<RuntimePart>,
    pending_recovered: std::collections::VecDeque<u64>,
    active: std::collections::HashMap<u64, ActivePart>,
    tasks: tokio::task::JoinSet<(u64, PartExit)>,
    samples_rx: tokio::sync::mpsc::UnboundedReceiver<PartSampleEvent>,
    samples_tx: tokio::sync::mpsc::UnboundedSender<PartSampleEvent>,
    max_concurrency: usize,
    initial_downloaded: u64,
    started_at: std::time::Instant,
}

// initialization.rs
pub(super) enum MultipartInitialization {
    Ready {
        manifest: ArchiveManifest,
        seed: SeedResponse,
    },
    SequentialResponse(reqwest::Response),
    SequentialRestart(Error),
}

pub(super) async fn initialize_multipart(
    http: &reqwest::Client,
    cookies: &crate::models::EhCookies,
    download_url: &str,
    artifacts: &ArchiveArtifacts,
) -> std::result::Result<MultipartInitialization, Error>;

// coordinator.rs
pub(super) enum MultipartOutcome {
    Complete(ArchiveManifest),
    RestartSequential(Error),
}

// mod.rs
pub(crate) async fn download_to_partial(
    http: &reqwest::Client,
    cookies: &crate::models::EhCookies,
    download_url: &str,
    artifacts: &ArchiveArtifacts,
    options: ArchiveDownloadOptions,
) -> Result<()>;

impl MultipartCoordinator {
    pub(super) async fn new(
        http: reqwest::Client,
        cookies: crate::models::EhCookies,
        download_url: String,
        artifacts: ArchiveArtifacts,
        manifest: ArchiveManifest,
        max_concurrency: usize,
        seed: Option<SeedResponse>,
    ) -> Result<Self>;

    pub(super) async fn run(self) -> Result<MultipartOutcome>;
}
```

In `part.rs`, now implement the HTTP loop against the failing `integration.rs` tests from Step 2:

```rust
pub(super) async fn run_part(
    http: reqwest::Client,
    cookies: crate::models::EhCookies,
    download_url: String,
    total_len: u64,
    validator: Validator,
    part: ManifestPart,
    part_path: PathBuf,
    attempts_used: usize,
    generation: u64,
    seed_response: Option<SeedResponse>,
    mut pause: tokio::sync::watch::Receiver<bool>,
    samples: tokio::sync::mpsc::UnboundedSender<PartSampleEvent>,
) -> (u64, PartExit);

async fn flush_and_emit_sample(
    writer: &mut tokio::io::BufWriter<tokio::fs::File>,
    part_path: &std::path::Path,
    part_id: u64,
    generation: u64,
    window_delta: u64,
    elapsed: std::time::Duration,
    samples: &tokio::sync::mpsc::UnboundedSender<PartSampleEvent>,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    writer.flush().await?;
    let durable_len = tokio::fs::metadata(part_path).await?.len();
    let (applied, acknowledged) = tokio::sync::oneshot::channel();
    samples
        .send(PartSampleEvent {
            sample: PartSample {
                part_id,
                generation,
                durable_len,
                window_delta,
                elapsed,
            },
            applied,
        })
        .map_err(|_| Error::Other("EH archive part coordinator stopped".to_owned()))?;
    acknowledged
        .await
        .map_err(|_| Error::Other("EH archive part coordinator stopped".to_owned()))
}
```

For every attempt, read `downloaded = metadata(part_path).len()` and treat that value as authoritative for `requested_range` and the new attempt's zeroed `window_delta`. Exact completion returns `Complete`; oversize returns `RestartSequential`. Treat a supplied initialization `SeedResponse` as the first of four cumulative attempts; later requests run only while `attempts_used < 4` and wait one pause-aware second between failures. Build each request with `archive_get`, exact `Range: bytes={requested_start}-{requested_end - 1}`, and selected `If-Range`.

Wrap both `send()` and each stream read in `tokio::select!` against `pause.changed()`. Start a fresh transfer-window clock immediately before each send (or use `SeedResponse.request_started_at`); never start it before retry backoff. Increment `window_delta` only for bytes written in that window. A periodic sample calls `flush_and_emit_sample` after at least one second and non-zero progress, then resets `window_delta` to zero and starts a new same-attempt clock only after the send succeeds.

On every transient stream error or clean-but-incomplete EOF, call `flush_and_emit_sample` regardless of whether one second elapsed. If this best-effort flush succeeds, metadata supplies the absolute `durable_len`; `flush_and_emit_sample` sends the event and waits for the coordinator's apply acknowledgement before the worker may enter the pause-aware one-second backoff or construct the next request. Only after acknowledgement may the caller reset the window. Any flush/metadata/send/acknowledgement failure exits this `run_part` invocation as `PartExit::Failed` without another internal attempt, so the coordinator cannot make a later split decision while durable state is unreported. Therefore a short error-boundary event reaches the coordinator cursor exactly once but remains rate-ineligible, while the next attempt starts a fresh clock after backoff and cannot charge sleep time to EWMA. On exact completion, flush, emit, and await acknowledgement for the final window before returning `Complete`. On pause, drop unflushed buffered bytes and return `Paused`; coordinator pause handling reads metadata. Reject a chunk crossing the interval before writing it. Attempt warnings contain only part ID, start/end, attempt, and failure kind—never URL, headers, cookies, body, manifest, or path.

`initialize_multipart` builds the GET through `http::archive_get`, adds only `Range: bytes=0-`, records `request_started_at` immediately before `send()`, and sends the request. On valid `206`, require start `0`, inclusive end `total_len - 1`, and concrete non-zero total; choose the validator, create `parts_dir`, create empty part ID `0`, persist one interval `[0, total_len)`, and return `SeedResponse { response, request_started_at }` for part zero. A `200` returns `MultipartInitialization::SequentialResponse(response)`. `416` or malformed initial `206` returns `SequentialRestart(error)` and creates no manifest.

Implement `download_to_partial` in the approved order from its first version: validate options; prefer an existing `parts_dir`; classify recovery through `recover_manifest(...).await?`; remove stale assembly and resume only `ManifestRecovery::Valid`; purge and start sequentially from zero only for `ManifestRecovery::Invalid`; propagate the outer I/O `Err` unchanged without deleting state or issuing an archive GET; otherwise use legacy sequential when `.zip.part` exists or maximum is one; otherwise initialize dynamic multipart. Recovery launches incomplete intervals in ascending `start` order and fills an available slot with the next existing interval before considering a structural split. Completed intervals issue no request. This order makes valid recovery, malformed-state fallback, and operational-I/O preservation part of the same RED/GREEN cycle as new-download coordination.

Use this concrete orchestration shape in `archive_download/mod.rs`; `MultipartCoordinator::new` in `coordinator.rs` reads part lengths, queues recovered intervals in ascending order, and attaches the optional seed only to part ID zero. The facade constructs and runs the coordinator directly, with no pass-through coordinator helper:

```rust
use self::assembly::assemble_parts;
use self::coordinator::{MultipartCoordinator, MultipartOutcome};
use self::initialization::{initialize_multipart, MultipartInitialization};
use self::manifest::{recover_manifest, ManifestRecovery};
use self::sequential::download_sequential;

pub(crate) async fn download_to_partial(
    http: &reqwest::Client,
    cookies: &crate::models::EhCookies,
    download_url: &str,
    artifacts: &ArchiveArtifacts,
    options: ArchiveDownloadOptions,
) -> Result<()> {
    let options = options.validate()?;
    let outcome = if tokio::fs::try_exists(artifacts.parts_dir()).await? {
        match recover_manifest(artifacts, download_url).await? {
            ManifestRecovery::Valid(manifest) => {
                artifacts.remove_assembly_scratch().await?;
                MultipartCoordinator::new(
                    http.clone(),
                    cookies.clone(),
                    download_url.to_owned(),
                    artifacts.clone(),
                    manifest,
                    options.max_concurrency,
                    None,
                )
                .await?
                .run()
                .await?
            }
            ManifestRecovery::Invalid(reason) => {
                tracing::warn!(
                    reason = ?reason,
                    "Invalid EH multipart manifest; restarting sequentially"
                );
                artifacts.remove_multipart_state().await?;
                return download_sequential(
                    http,
                    cookies,
                    download_url,
                    artifacts.assembly_scratch(),
                    None,
                )
                .await;
            }
        }
    } else if tokio::fs::try_exists(artifacts.assembly_scratch()).await?
        || options.max_concurrency == 1
    {
        return download_sequential(
            http,
            cookies,
            download_url,
            artifacts.assembly_scratch(),
            None,
        )
        .await;
    } else {
        match initialize_multipart(http, cookies, download_url, artifacts).await? {
            MultipartInitialization::Ready { manifest, seed } => {
                MultipartCoordinator::new(
                    http.clone(),
                    cookies.clone(),
                    download_url.to_owned(),
                    artifacts.clone(),
                    manifest,
                    options.max_concurrency,
                    Some(seed),
                )
                .await?
                .run()
                .await?
            }
            MultipartInitialization::SequentialResponse(response) => {
                return download_sequential(
                    http,
                    cookies,
                    download_url,
                    artifacts.assembly_scratch(),
                    Some(response),
                )
                .await;
            }
            MultipartInitialization::SequentialRestart(error) => return Err(error),
        }
    };

    match outcome {
        MultipartOutcome::Complete(manifest) => assemble_parts(artifacts, &manifest).await,
        MultipartOutcome::RestartSequential(error) => Err(error),
    }
}
```

The two `Err(error)` protocol-restart arms are deliberate Task 6 intermediate behavior and are the RED seam replaced by one-shot cleanup/fallback in Task 7; ordinary retryable errors and every outer `recover_manifest` I/O error remain `Err` and preserve state. Within manifest recovery classification, only the explicit `ManifestRecovery::Invalid(reason)` branch purges persisted multipart artifacts.

Spawn each part through one helper that sets `runtime_part.active = true`, creates a `watch::channel(false)`, passes the runtime part's `generation` to `run_part`, and inserts the sender in `active`. Every task returns its part ID with `PartExit`; do not detach tasks.

In `MultipartCoordinator::run`, use `tokio::select! { biased; ... }` with `samples_rx.recv()` before `tasks.join_next()`. Consume an event with this exact order so the worker cannot cross its apply barrier prematurely:

```rust
let PartSampleEvent { sample, applied } = event;
let result = if let Some(runtime) = runtime_parts
    .iter_mut()
    .find(|runtime| runtime.part.id == sample.part_id)
{
    apply_part_sample(runtime, &sample)
} else {
    Ok(SampleDisposition::Ignored)
};
let _ = applied.send(());
let disposition = result?;
```

`apply_part_sample` validates the whole event before mutation. Inactive, wrong-ID, or stale-generation input is `Ignored` and changes neither downloaded bytes nor EWMA. A current event must have `durable_len <= part.len()`, `durable_len >= runtime.downloaded`, and `runtime.downloaded.checked_add(window_delta) == durable_len`; overflow, regression, excess, or delta mismatch returns an error with all runtime fields unchanged. After validation, assign `runtime.downloaded = durable_len`. A rate-ineligible event returns `Reconciled` without touching EWMA/eligibility and must not itself trigger `choose_split`; a stable event then updates EWMA from only `window_delta / elapsed`, sets `has_stable_sample`, and returns `Stable`.

Only `Stable` and completion events can trigger splitting. Before either trigger builds `Vec<SplitInput>`, drain and apply/acknowledge every already-queued `PartSampleEvent`; use the same validation path, and perform no `await` between the final empty `try_recv()` and `choose_split`. Combined with the worker acknowledgement barrier and biased sample branch, every successful error-boundary flush that precedes a later event-loop split decision is reflected in the cursor exactly once. Call `policy::choose_split` at most once for each triggering stable/completion event. On `PartExit::Complete`, replace `runtime.downloaded` from actual part-file metadata before marking it inactive and building split inputs; the selected `PartExit::Paused` path likewise replaces `downloaded` from metadata in `pause_and_split`. At attempt start, the worker's request offset is independently authoritative from metadata.

- [ ] **Step 5: Implement pause/join/split with crash-safe ordering**

Use this signature and ordering:

```rust
async fn pause_and_split(
    coordinator: &mut MultipartCoordinator,
    split: SplitPlan,
) -> Result<()>;
```

1. Send `true` only to the selected part.
2. Repeatedly select between `samples_rx.recv()` and `join_next()` until the selected ID exits; apply and acknowledge samples and process unrelated completions, but do not trigger another split while waiting. This prevents a paused worker that is waiting on its final flush acknowledgement from deadlocking with the join.
3. If selected completed before pause, return without structural change; if it returns `Failed`, abort through the same retry-exhausted or `RestartSequential` coordinator path rather than mutating intervals; only `Paused` proceeds.
4. Read the selected file's metadata and set `downloaded`; reject `split_at <= cursor` or `split_at >= old_end`.
5. Allocate `new_id = manifest.next_part_id`, increment `next_part_id`, create an empty stable part file, shorten selected `end`, and append `[split_at, old_end)`.
6. Call `manifest.write_atomic(artifacts)`; that owned manifest operation sorts, validates, and atomically replaces state before coordinator runtime state is updated.
7. Increment the selected runtime part's `generation` and relaunch it with cumulative `attempts_used`; launch the new part with generation zero, zero attempts, and `ewma = Some(split.new_rate)` only as an estimate for allocation. Require a real sample before that new part can itself be selected for another split by tracking `has_stable_sample: bool` separately from estimated rate.

Use the existing `RuntimePart.has_stable_sample`: `choose_split` requires it for candidates and includes only active parts with `has_stable_sample` when computing the median. A new part may carry its estimated `ewma` for bookkeeping, but it cannot influence another split until a real sample sets `has_stable_sample = true`. This prevents an estimated/stale rate from cascading immediately.

On any coordinator failure, send pause to every active task, then select between pending sample events and `JoinSet::join_next()` until all events are acknowledged and every task is drained before returning. This teardown must acknowledge even the sample whose validation failed so no worker remains blocked at the apply barrier. A retry-exhausted part calls `aggregate_downloaded_bytes`; when `made_progress(final - initial, elapsed)` is true, return:

```rust
Error::DownloadInProgress {
    inner: Box::new(failure.error),
    attempts: failure.attempts,
    bytes_delta: final_downloaded.saturating_sub(coordinator.initial_downloaded),
    elapsed: coordinator.started_at.elapsed(),
}
```

Otherwise return the underlying error. A `RestartSequential` failure is propagated through an internal coordinator outcome so the caller can perform one reset/fallback instead of wrapping it as progress.

- [ ] **Step 6: Write the ordered-assembly unit test, then implement assembly**

In `assembly.rs`, first add `assembly_orders_intervals_and_copies_exact_bytes`: create two part files, intentionally provide manifest entries out of vector order, assemble, and assert the scratch bytes follow ascending `start`; add a mismatched-length subcase that returns an error without deleting manifest/parts.

```powershell
cargo test -p eh_client --lib archive_download::assembly::tests::assembly_ -- --nocapture
```

Expected RED: compilation fails because `assemble_parts` is absent.

Use this signature:

```rust
pub(super) async fn assemble_parts(
    artifacts: &ArchiveArtifacts,
    manifest: &ArchiveManifest,
) -> Result<()>;
```

Remove stale assembly scratch, open a fresh truncate/create file, iterate manifest parts sorted by `start`, require each file length equals `part.len()`, copy each with `tokio::io::copy`, verify copied length, flush the assembly, and leave manifest/part files intact. ZIP validation and final rename remain in `client.rs`.

- [ ] **Step 7: Add options-based public methods and final lifecycle**

Keep the old two methods in their current textual order and add these methods after them:

```rust
pub async fn download_archive_with_options(
    &self,
    gid: u64,
    token: &str,
    archiver_key: &str,
    resolution: &str,
    dest: &Path,
    options: ArchiveDownloadOptions,
) -> Result<u64>;

pub async fn download_archive_with_request_and_options(
    &self,
    request: &ArchiveDownloadRequest,
    dest: &Path,
    options: ArchiveDownloadOptions,
) -> Result<u64>;
```

Keep the old signatures and their textual order, replacing only their bodies with these delegations:

```rust
// Existing download_archive signature is unchanged.
self.download_archive_with_options(
    gid,
    token,
    archiver_key,
    resolution,
    dest,
    ArchiveDownloadOptions::default(),
)
.await

// Existing download_archive_with_request signature is unchanged.
self.download_archive_with_request_and_options(
    request,
    dest,
    ArchiveDownloadOptions::default(),
)
.await
```

The new archiver-key method builds the existing `ArchiveDownloadRequest` and delegates to the new request-options method. The request-options method validates options before POSTing, maps POST send/text transport errors through `archive_download::archive_http_error`, performs the otherwise unchanged POST/redirect parse, then calls:

```rust
archive_download::download_to_partial(
    &self.http,
    &self.cookies,
    &download_url,
    &artifacts,
    options,
)
.await?;
```

After transfer, `client.rs` validates `artifacts.assembly_scratch()`. Validation failure removes assembly plus multipart directory and returns the validation error. Rename failure removes only assembly and preserves multipart manifest/parts as authoritative recovery state. After successful rename, call `remove_parts_dir`; log a concise warning on cleanup failure without URL/path/token and still return the successful byte count so a post-rename cleanup problem cannot spend GP again. Startup orphan cleanup provides the second cleanup opportunity.

- [ ] **Step 8: Run dynamic tests and capture GREEN**

Run every Step 3 command plus:

```powershell
cargo test -p eh_client --lib archive_download::part::tests::part_worker_emits_durable_reconciliation -- --nocapture
cargo test -p eh_client --lib archive_download::coordinator::tests -- --nocapture
cargo test -p eh_client --lib archive_download::assembly::tests -- --nocapture
cargo test -p eh_client --test integration multipart_small_archive -- --nocapture
```

Expected: all dynamic, recovery, part-worker, and coordinator tests pass. A short transient-error flush emits and is acknowledged before retry, advances `RuntimePart.downloaded` exactly once from its absolute length/delta pair, and leaves EWMA untouched; a later stable event advances the cursor first and computes EWMA from only its own transfer window with retry sleep excluded. Stale-generation, overflow, regression, beyond-length, mismatched-delta, and duplicate events leave runtime state unchanged. The first request is one open-ended Range, dynamic requests use no more than the configured connections, completion can release/reuse a slot, selected tasks pause/join and resume from durable length, truncated parts retry exact bounds no more than four times with one-second spacing, successful ranges cover the ZIP exactly, valid persisted offsets resume with or without a validator, invalid state purges, operational recovery I/O errors preserve every artifact and issue no archive GET, unreferenced files are removed, and success leaves only the final ZIP.

- [ ] **Step 9: Verify old method compatibility**

```powershell
cargo test -p eh_client --test integration test_download_archive_full_flow -- --nocapture
cargo test -p eh_client --test integration test_download_archive_resumes_existing_partial_file -- --nocapture
```

Expected: both existing tests pass and old methods issue the sequential request pattern because their implicit maximum remains one.

- [ ] **Step 10: Record the suggested commit boundary**

Suggested boundary, requiring explicit user authorization: `feat(eh_client): coordinate dynamic archive parts`. Review new-download splitting, bounded tasks, assembly, API compatibility, ZIP validation placement, and success cleanup.

---

### Task 7: Add protocol fallback and corruption cleanup

**Files:**
- Modify/Test: `eh_client/src/archive_download/mod.rs`
- Modify/Test: `eh_client/src/archive_download/coordinator.rs`
- Modify: `eh_client/src/client.rs:457-531`
- Modify/Test: `eh_client/tests/integration.rs`

- [ ] **Step 1: Add failing protocol-reset tests**

For each row below, seed valid multipart bytes, mount the listed invalid multipart response, mount one no-Range `200` full ZIP response, and assert cancellation/join, directory/scratch deletion before fallback, exactly one sequential restart, final bytes equal source, and no remaining multipart artifacts:

| Test | Trigger |
|---|---|
| `test_multipart_200_if_range_fallback_resets_to_sequential` | bounded request returns `200` |
| `test_multipart_416_resets_to_sequential` | bounded request returns `416` |
| `test_multipart_invalid_content_range_resets_to_sequential` | wrong start or inclusive end |
| `test_multipart_url_change_resets_before_range_request` | new archiver redirect URL differs from manifest URL |
| `test_multipart_total_change_resets_to_sequential` | exact bounds but changed total |
| `test_multipart_validator_change_or_missing_validator_resets` | changed/missing required strong ETag and changed/missing Last-Modified subcases |

Also add two fresh-download rows with no seeded manifest: `test_multipart_initial_416_resets_to_sequential` mounts `416` for `bytes=0-`, and `test_multipart_initial_invalid_content_range_resets_to_sequential` mounts a `206` whose start/end/total is wrong. Each then mounts exactly one no-Range `200` full ZIP, asserts the one-shot restart, and leaves no multipart state.

- [ ] **Step 2: Run protocol-reset tests and capture RED**

```powershell
cargo test -p eh_client --test integration multipart_200_if_range -- --nocapture
cargo test -p eh_client --test integration multipart_416 -- --nocapture
cargo test -p eh_client --test integration multipart_invalid_content_range -- --nocapture
cargo test -p eh_client --test integration multipart_url_change -- --nocapture
cargo test -p eh_client --test integration multipart_total_change -- --nocapture
cargo test -p eh_client --test integration multipart_validator_change -- --nocapture
cargo test -p eh_client --test integration multipart_initial_416 -- --nocapture
cargo test -p eh_client --test integration multipart_initial_invalid_content_range -- --nocapture
```

Expected: the URL-change case may already pass through Task 6's invalid-manifest branch; the bounded-response and fresh-initialization reset commands fail because `RestartSequential` is returned instead of completing the mounted no-Range fallback.

- [ ] **Step 3: Implement one-shot graceful reset**

Replace both deliberate Task 6 restart-error arms. For `MultipartInitialization::SequentialRestart(_)`, remove multipart state and directly call `download_sequential(http, cookies, download_url, artifacts.assembly_scratch(), None)`. In `MultipartCoordinator::run`, a restart-classified part must send pause to every active task and drain `JoinSet` before returning `MultipartOutcome::RestartSequential(_)`; the facade then performs that same cleanup and direct sequential call exactly once. Do not route fallback through `download_to_partial`, which would reconsider multipart. The `ManifestRecovery::Invalid` branch remains a direct state removal plus sequential restart, while outer recovery I/O errors must continue propagating before either protocol-reset arm is considered. Ordinary reqwest transport/body failures preserve manifest and files; only exhausted attempts return the ordinary/`DownloadInProgress` error.

- [ ] **Step 4: Add failing aggregate-progress and final-corruption tests**

Add:

1. `test_multipart_truncated_parts_preserve_aggregate_progress` — two persisted intervals each append durable bytes, one exhausts four attempts, and the returned `DownloadInProgress.bytes_delta` equals the sum of both final file-length deltas rather than response counters.
2. `test_multipart_final_zip_corruption_removes_assembly_and_parts` — exact ranges assemble a PK-prefixed corrupt ZIP; final destination, assembly, and parts directory are absent.
3. `test_multipart_corrupt_entry_removes_assembly_and_parts` — corrupt stored entry CRC/data produces the existing ZIP-entry validation error and removes multipart state.

- [ ] **Step 5: Run aggregate/corruption tests and capture RED**

```powershell
cargo test -p eh_client --test integration multipart_truncated_parts -- --nocapture
cargo test -p eh_client --test integration multipart_final_zip_corruption -- --nocapture
cargo test -p eh_client --test integration multipart_corrupt_entry -- --nocapture
```

Expected: failures identify any missing aggregate accounting or client-side multipart cleanup branch.

- [ ] **Step 6: Complete aggregate and validation-failure cleanup paths**

Before starting recovered/new multipart coordination, record `initial_downloaded = aggregate_downloaded_bytes(...)`. Before wrapping retry exhaustion, pause/join all tasks and recompute final aggregate from metadata. In `client.rs`, on any `validate_complete_zip(assembly_scratch)` error, call `artifacts.remove_multipart_state()` after best-effort assembly removal and return the original validation error unless cleanup is the only failure. Keep the final destination absent.

- [ ] **Step 7: Run the reset/corruption suite and capture GREEN**

Run every command from Steps 2 and 5.

Expected: all reset triggers, aggregate progress, and both corruption cleanup cases pass. The Task 6 recovery suite remains green and is rerun with every `multipart_` test in Task 9.

- [ ] **Step 8: Run existing sequential rejection and fallback regressions**

```powershell
cargo test -p eh_client --test integration test_download_archive_rejects_mismatched_content_range -- --nocapture
cargo test -p eh_client --test integration test_download_archive_restarts_after_invalid_partial_on_416 -- --nocapture
cargo test -p eh_client --test integration test_download_archive_rejects_zip_with_corrupt_entry_data -- --nocapture
cargo test -p eh_client --test integration test_download_gallery_images -- --nocapture
```

Expected: legacy sequential and unauthenticated behavior remains green.

- [ ] **Step 9: Record the suggested commit boundary**

Suggested boundary, requiring explicit user authorization: `feat(eh_client): recover and reset multipart archives`. Review persisted resume, compatibility without validators, one-shot protocol fallback, aggregate progress, and corruption cleanup.

---

### Task 8: Thread options through both workers and clean every artifact family

**Files:**
- Modify/Test: `src/scheduler/eh_engine.rs:11-15,261-384,992-1197,1945-2016,2018-6147`
- Modify/Test: `src/db/repo/eh_download_queue.rs:1-15,3096-3174,3886-3962`

Task 8 consumes only the crate-root re-exports `eh_client::{ArchiveArtifacts, ArchiveDownloadOptions}`. Scheduler/repository code must not import private archive submodule paths, so the module tree remains an `eh_client` implementation detail.

- [ ] **Step 1: Write failing main/background threading tests**

Add two scheduler integration tests:

- `test_download_worker_threads_archive_download_concurrency` sets `archive_download_concurrency = 2`, mounts an archive response only for `Range: bytes=0-` with exact `206`, and asserts main entry reaches `STATUS_DOWNLOADED`.
- `test_background_download_worker_threads_archive_download_concurrency` schedules one background entry, sets the same value, mounts the same Range-only response, calls `tick()`, and asserts the entry reaches `STATUS_DOWNLOADED` with background state cleared.

Both tests use a small valid ZIP that completes in the initial response. If either worker calls the old default-one method, its no-Range GET will not match wiremock and the test must fail.

- [ ] **Step 2: Run worker threading tests and capture RED**

```powershell
cargo test -p pixivbot --lib worker_threads_archive_download_concurrency -- --nocapture
```

Expected: both tests fail because workers still call `download_archive_with_request`.

- [ ] **Step 3: Pass the option in both authenticated worker paths**

Import the option:

```rust
use eh_client::{
    parser::DownloadCost, rewrite_ipfs_gateway_nodes, ArchiveDownloadOptions, EhClient, EhGallery,
    ImageUploadInput, ImageUploader, IpfS3PreviewRewriteConfig, TelegraphClient,
    TelegraphImageUrlPair, TelegraphRewriteData, ZipArchiveUploadInput,
};
```

Replace both authenticated archive calls with:

```rust
.download_archive_with_request_and_options(
    &archive_request,
    &zip_path,
    ArchiveDownloadOptions {
        max_concurrency: self.config.archive_download_concurrency,
    },
)
```

Do not change `background_download_concurrency.max(1)`, `JoinSet` queue scheduling, GP/size gates, source resolution, or the unauthenticated branch.

- [ ] **Step 4: Run worker threading tests and capture GREEN**

Run the Step 2 command again.

Expected: both tests pass and each recorded archive transfer begins with `bytes=0-`.

- [ ] **Step 5: Extend permanent-failure cleanup tests before changing cleanup code**

In `test_download_worker_permanent_failure_cleans_partial_archive`, also create `123456_abcdef0123.zip.parts/manifest.json` and a nested part, then assert the directory is removed.

Add `test_background_worker_permanent_failure_cleans_archive_artifact_family`: schedule a background entry, set `background_download_max_attempts = 1`, pre-create final ZIP, assembly scratch, and nested parts directory, make the prepared archive POST fail, run `tick()`, assert `STATUS_FAILED`, and assert all three family members are absent.

- [ ] **Step 6: Run permanent cleanup tests and capture RED**

```powershell
cargo test -p pixivbot --lib permanent_failure_cleans -- --nocapture
```

Expected: the main test leaves `.zip.parts`; the new background test leaves the full family.

- [ ] **Step 7: Use `ArchiveArtifacts` for both permanent-failure paths**

Add one scheduler-local constructor and cleanup function:

```rust
fn archive_artifacts_for_entry(
    cache_dir: &std::path::Path,
    entry: &eh_download_queue::Model,
) -> eh_client::ArchiveArtifacts {
    eh_client::ArchiveArtifacts::new(
        cache_dir
            .join("eh_cache")
            .join(format!("{}_{}.zip", entry.gid, entry.token)),
    )
}

async fn cleanup_archive_artifacts(
    cache_dir: &std::path::Path,
    entry: &eh_download_queue::Model,
) {
    if let Err(error) = archive_artifacts_for_entry(cache_dir, entry).remove_all().await {
        warn!(
            gid = entry.gid,
            error = %error,
            "Failed to delete EH archive artifacts after permanent failure"
        );
    }
}
```

Make `EhDownloadWorker::cleanup_zip` delegate to this function. In `EhBackgroundDownloadWorker::process_claimed`, call it only when `schedule_eh_background_download_retry` returns `permanent = true`. Retryable failures continue preserving all resume state.

- [ ] **Step 8: Run permanent cleanup tests and capture GREEN**

Run the Step 6 command again.

Expected: both main and background permanent failures remove final ZIP, assembly scratch, and the recursive parts directory.

- [ ] **Step 9: Extend orphan cleanup tests with active and orphan multipart directories**

In repository tests, create:

- `orphan.zip.parts/manifest.json` plus a nested part;
- `active.zip`, `active.zip.part`, and `active.zip.parts/manifest.json` plus a nested part for a downloaded active row;
- `88_tok.zip.parts/manifest.json` plus a nested part for the pending retry row;
- an unrelated directory `notes/keep.txt`.

Assert first cleanup removes the entire orphan family; keeps `active.zip` but removes its now-stale `.zip.part` and `.zip.parts`; preserves the pending `88_tok.zip.part` and `.zip.parts` resumable state because no final ZIP exists; and leaves `notes` untouched. After changing row `88` to canceled, assert cleanup recursively removes its `.zip.part` and `.zip.parts` directory.

- [ ] **Step 10: Run orphan cleanup tests and capture RED**

```powershell
cargo test -p pixivbot --lib cleanup_eh_cache_orphans -- --nocapture
```

Expected: existing orphan tests expose that `.zip.parts` directories are ignored.

- [ ] **Step 11: Group top-level cache entries by `ArchiveArtifacts` and delete orphan families**

Import `eh_client::ArchiveArtifacts`. Change `expected_eh_cache_zip_paths` to return `Vec<PathBuf>` instead of lossy strings. Build `HashSet<PathBuf>` of active final ZIP identities. Scan only top-level cache entries and collect one artifact helper per unique final ZIP:

```rust
let mut families = std::collections::HashMap::<std::path::PathBuf, ArchiveArtifacts>::new();
for entry in std::fs::read_dir(cache_dir).context("Failed to read eh_cache dir")? {
    let path = entry?.path();
    if let Some(artifacts) = ArchiveArtifacts::from_member(&path) {
        families
            .entry(artifacts.final_zip().to_path_buf())
            .or_insert(artifacts);
    }
}

for (final_zip, artifacts) in families {
    if !active_paths.contains(&final_zip) {
        if let Err(error) = artifacts.remove_all().await {
            warn!(
                error = %error,
                "Failed to remove orphan EH archive artifact family"
            );
        }
    } else if final_zip.exists() {
        if let Err(error) = artifacts.remove_multipart_state().await {
            warn!(
                error = %error,
                "Failed to remove stale EH archive resume artifacts"
            );
        }
    }
}
```

Remove the old `is_eh_cache_archive_artifact` and `final_eh_cache_zip_path` helpers. Active pending/downloading rows continue adding their expected `{gid}_{token}.zip`; rows with persisted `zip_path` add that final ZIP. Membership preserves the whole resumable family only while the final ZIP is absent; once an active final ZIP exists, it is authoritative and startup cleanup removes stale assembly/parts left by a prior post-rename cleanup failure.

- [ ] **Step 12: Run orphan cleanup tests and capture GREEN**

Run the Step 10 command again.

Expected: orphan files/directories are removed recursively, active final ZIPs lose stale resume artifacts, active retryable families without a final ZIP remain intact, canceled families disappear, and unrelated paths are untouched.

- [ ] **Step 13: Run application regressions around queue concurrency and progress**

```powershell
cargo test -p pixivbot --lib test_drain_background_download_tasks_waits_for_siblings_after_error -- --nocapture
cargo test -p pixivbot --lib test_download_worker_progress_failure_defers_without_retry -- --nocapture
cargo test -p pixivbot --lib test_background_gp_rate_limit_allows_only_one_post -- --nocapture
cargo test -p pixivbot --lib test_main_and_background_gp_rate_limit_allows_only_one_post -- --nocapture
```

Expected: all existing tests pass. Queue-level worker count, sibling draining, progress deferral, and GP locking are unchanged.

- [ ] **Step 14: Record the suggested commit boundary**

Suggested boundary, requiring explicit user authorization: `feat: thread EH archive concurrency and clean multipart artifacts`. Review only worker option propagation and artifact lifecycle; no queue-level concurrency or UI change is allowed.

---

### Task 9: Final verification and acceptance mapping

**Files:**
- Verify every file in the file map and the approved design document.

- [ ] **Step 1: Format implementation files**

```powershell
cargo fmt --all
```

Expected: exit code `0`; only intended Rust formatting changes occur.

- [ ] **Step 2: Run focused `eh_client` policy/state tests**

```powershell
cargo test -p eh_client --lib archive_download:: -- --nocapture
```

Expected evidence: facade options, artifact paths, pure policy, manifest valid/invalid/I/O classification, sequential compatibility, shared HTTP policy, validators, pure part range/aggregate state, coordinator ownership/generation handling, and assembly tests all pass under their colocated module paths.

- [ ] **Step 3: Run the real HTTP multipart suite**

```powershell
cargo test -p eh_client --test integration multipart_ -- --nocapture
cargo test -p eh_client --test integration manifest_recovery_io_error -- --nocapture
cargo test -p eh_client --test integration archive_options_zero -- --nocapture
```

Expected evidence: all multipart wiremock tests pass; actual delayed requests never exceed the configured maximum, persisted offsets resume, every reset trigger falls back once, output equals source bytes, terminal corruption cleans state, zero options fail before POST, and recovery-time I/O failure preserves state without any archive GET.

- [ ] **Step 4: Run sequential and no-login regressions**

```powershell
cargo test -p eh_client --test integration test_download_archive_ -- --nocapture
cargo test -p eh_client --test integration test_download_gallery_images -- --nocapture
```

Expected: all existing archive/sequential and unauthenticated image fallback tests pass.

- [ ] **Step 5: Run config, worker, and repository focused tests**

```powershell
cargo test -p pixivbot --lib archive_download_concurrency -- --nocapture
cargo test -p pixivbot --lib worker_threads_archive_download_concurrency -- --nocapture
cargo test -p pixivbot --lib permanent_failure_cleans -- --nocapture
cargo test -p pixivbot --lib cleanup_eh_cache_orphans -- --nocapture
cargo test -p pixivbot --lib test_download_worker_progress_failure_defers_without_retry -- --nocapture
```

Expected: zero config is rejected, both workers pass the option, main/background permanent failures clean all artifacts, orphan cleanup groups families, and retryable progress preserves resumable state.

- [ ] **Step 6: Run crate and workspace test gates**

```powershell
cargo test -p eh_client
cargo test -p pixivbot --lib eh_engine
make ci
```

Expected: both focused suites pass; `make ci` completes `fmt-check`, warnings-denied clippy, check, test, and release build successfully. Do not enable `ffmpeg-codec` for this feature.

- [ ] **Step 7: Inspect diagnostics and patch hygiene**

First audit the focused module production sections (lines before the first colocated test module):

```powershell
$files = @(fd --type f --extension rs . eh_client/src/archive_download)
foreach ($file in $files) {
    $marker = rg -n -m 1 '^#\[cfg\(test\)\]' -- $file
    $pureLines = if ($LASTEXITCODE -eq 0) {
        [int](($marker -split ':', 2)[0]) - 1
    } else {
        [int](rg -c '^' -- $file)
    }
    "$file`t$pureLines"
}
```

Expected: each responsibility-focused production section is at or below roughly 250 lines. If one practical exception remains, the diff must show a cohesive single responsibility rather than a hidden mixed module or forwarding split.

Open each changed Rust file with `lsp_diagnostics` and require no error or warning diagnostics. Then run read-only Git checks:

```powershell
git diff --check
git status --short
git diff -- eh_client/src/archive_download eh_client/src/client.rs eh_client/src/lib.rs eh_client/Cargo.toml eh_client/tests/integration.rs src/config.rs config.toml.example src/scheduler/eh_engine.rs src/db/repo/eh_download_queue.rs
```

Expected: `git diff --check` has no output; status lists only the approved spec, this plan, and intended implementation files; the diff contains no credentials, archive URL fixtures with real tokens, database migration, queue UI, or unrelated refactor.

- [ ] **Step 8: Verify acceptance coverage**

| Accepted requirement | Implementation task | Executable evidence |
|---|---|---|
| Default one; zero rejected before POST | Tasks 1 and 6 | config tests plus `archive_options_zero_is_rejected_before_archiver_post` |
| Old APIs stay sequential; options API added | Tasks 4 and 6 | existing full/resume tests plus old-method compatibility tests |
| Focused module tree, no mixed mega-module or further `client.rs` growth | Tasks 1-7 | file map/diff show owned facade, artifacts, manifest, policy, HTTP, sequential, part, initialization, coordinator, and assembly files; production sections target `<=250` LOC |
| First request only `bytes=0-` | Task 6 | `multipart_starts_with_one_open_ended_range` |
| EWMA `0.25`, one-MiB/15-second floor | Task 3 | policy unit tests |
| Largest sampled tail, proportional clamp, one split per event | Tasks 3 and 6 | split unit tests and completion/work-stealing wiremock test |
| Pause selected task, join, trust file length | Task 6 | dynamic split/relaunch and coordinator-generation tests |
| Every successful worker flush reports absolute durable length plus one non-overlapping window delta before further I/O | Tasks 5 and 6 | `part_sample_rate_eligibility_requires_one_second_and_nonzero_delta` and `part_worker_emits_durable_reconciliation_before_retry_after_transient_error` |
| Short retry-boundary reconciliation advances the cursor once without EWMA; stable later window updates EWMA; invalid/stale events are non-mutating | Task 6 | `coordinator_retry_boundary_flush_updates_cursor_once_without_polluting_ewma`, `coordinator_stale_generation_sample_updates_neither_downloaded_nor_ewma`, and `coordinator_invalid_absolute_samples_are_non_mutating` |
| Exact Range/Content-Range/total/validator | Tasks 5-7 | part-policy tests, real HTTP Range tests, and reset matrix |
| Four attempts, one-second delay excluded from sample elapsed, durable aggregate progress | Tasks 6 and 7 | part-worker retry-boundary test, real-surface part exhaustion, and aggregate `DownloadInProgress` tests |
| Manifest crash recovery and unreferenced cleanup | Tasks 3 and 6 | manifest and process-style restart tests |
| Invalid manifests purge, operational recovery I/O preserves state and starts no GET | Tasks 3 and 6 | table row `incomplete_final_coverage` maps only to `InvalidIntervalCoverage`, stale `next_part_id` maps only to `InvalidNextPartId`, plus `manifest_recovery_io_error_preserves_state_and_skips_archive_get` |
| Resume with strong validator and without validator | Task 6 | two restart wiremock tests |
| Wrong protocol state clears and sequentially restarts once | Task 7 | eight named recovered/initial reset tests |
| Ordered merge, ZIP/entry validation, atomic final rename | Tasks 6 and 7 | exact-byte assembly and corruption tests |
| Both workers use configured maximum | Task 8 | two worker threading tests |
| Success, permanent failure, and orphan cleanup cover family | Tasks 2, 6, and 8 | artifact helper, worker cleanup, and repo cleanup tests |
| URLs/cookies/manifests/token paths are absent from diagnostics | Tasks 4-8 | URL-sanitized reqwest conversion plus final logging diff inspection |
| Queue-level concurrency/no-login/UI/database unchanged | Tasks 8-9 | existing GP/background/fallback tests and final diff inspection |

- [ ] **Step 9: Record final suggested commit boundaries**

If the user later authorizes Git writes, preserve the task boundaries as reviewable semantic commits rather than one mixed change. Suggested order: configuration/options; artifact helper; manifest/policy; sequential extraction; Range worker; coordinator/API; recovery/reset; application threading/cleanup. Without explicit authorization, stop with a verified working tree and report the commands/results only.

## Self-review

- **Spec coverage:** Every design section maps to Tasks 1-9 and to an executable unit, wiremock, scheduler, repository, or full-CI check in the acceptance table.
- **Placeholder scan:** Passed. Every behavior-changing step names concrete files, types, signatures, logic, commands, and expected outcomes; no deferred implementation instruction remains.
- **Type and error consistency:** `ArchiveDownloadOptions`, `ArchiveArtifacts`, `ArchiveManifest`, `ManifestPart`, `ManifestRecovery`, `ManifestInvalid`, `SplitInput`, `SplitPlan`, coordinator-owned `RuntimePart`, `Validator`, `PartFailure`, `PartSample`, `PartSampleEvent`, `SampleDisposition`, `SeedResponse`, `PartExit`, `MultipartInitialization`, `MultipartOutcome`, `MultipartCoordinator`, and both options-based client methods use one spelling and one owner. `validate_shape` classifies incomplete final interval coverage as `InvalidIntervalCoverage` independently from the later `next_part_id <= max_id` check, which alone returns `InvalidNextPartId`. Within manifest recovery, only `ManifestRecovery::Invalid` authorizes family purge; the outer recovery `Result` propagates without deleting the manifest/referenced parts/assembly and without starting an archive GET. `PartSample.durable_len` is the post-flush absolute part-file length and `window_delta` is one non-overlapping window: the coordinator validates bounds, monotonicity, checked addition, and equality before any mutation; short events return `Reconciled`, stable events update downloaded before EWMA, and stale/invalid events update neither. `PartSampleEvent.applied` prevents retry/backoff/further I/O until coordinator application; completion and pause still overwrite downloaded from metadata, and attempt clocks start after retry sleep. Separately approved protocol-reset, final-corruption, permanent-failure, and orphan-cleanup branches retain their explicit cleanup semantics.
- **Path and responsibility consistency:** Production ownership is split across `archive_download/mod.rs`, `artifacts.rs`, `manifest.rs`, `policy.rs`, `http.rs`, `sequential.rs`, `part.rs`, `initialization.rs`, `coordinator.rs`, and `assembly.rs`. Unit-test filters name those modules, public HTTP real-surface tests remain in `eh_client/tests/integration.rs` with one documented private-channel worker exception in `part.rs`, the former monolithic-file target is absent, and no pass-through wrapper is planned.
- **Size and test placement:** Each production module has one named responsibility and a practical `<=250` pure-LOC target verified by Task 9's executable audit. Pure unit/state tests are colocated with their owner; the single colocated `part_worker_emits_durable_reconciliation_before_retry_after_transient_error` test observes the private apply-barrier channel that public integration tests cannot access, while all public request concurrency, full retry timing, pause/relaunch, recovery, and fallback HTTP tests remain in `eh_client/tests/integration.rs`.
- **Scope:** Limited to per-archive authenticated transfer, direct configuration threading, artifact cleanup, and direct tests. Queue-entry concurrency, UI, database schema, publishing, GP/size gates, and unauthenticated fallback are excluded.
- **Git policy:** All version-control boundaries are recommendations explicitly conditioned on later user authorization; the plan performs no staging, commit, push, tag, or other Git write.
