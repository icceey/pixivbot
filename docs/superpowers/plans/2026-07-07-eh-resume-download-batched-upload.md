# EH resumable download and batched upload plan

1. Inspect current download/upload code and tests.
2. Add resumable archive ZIP fetch helper in `eh_client/src/client.rs`:
   - preserve `.zip.part` on transient stream errors;
   - send `Range` from existing partial length;
   - append on `206`, restart on `200`, validate ZIP, rename, return final size.
3. Add/adjust `eh_client/tests/integration.rs` tests for Range resume and existing download flows.
4. Replace `EhUploadWorker` all-images-in-memory extraction with incremental ZIP producer / async upload consumer.
5. Update upload tests to assert per-image upload requests include large images.
6. Run focused tests, formatting, diagnostics, clippy/check, and final review.
