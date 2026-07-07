# EH resumable archive downloads and batched Telegraph uploads design

## Context

Large EH archive downloads can fail with `operation timed out` while streaming the final archive response. The current downloader removes the `.zip.part` file on any stream error, so every retry starts from byte zero. Telegraph uploads also read every image from the ZIP into memory before uploading, so one large archive can create a high memory peak.

The user delegated design, planning, implementation, and verification without intermediate approval.

## Design

Archive download will keep the existing `gallery -> archiver form/key -> redirect -> ZIP` flow, but the final ZIP fetch will become resumable. The downloader writes to `dest.with_extension("zip.part")`; if that file already has bytes, subsequent attempts send `Range: bytes=<len>-`. A `206 Partial Content` response appends to the partial file only after validating `Content-Range`. A `200 OK` response to a ranged request means the server ignored Range, so the partial file is replaced from byte zero. A `416 Range Not Satisfiable` response accepts the partial only when ZIP central-directory validation proves it is already complete; otherwise the partial is removed and the next attempt restarts. Network/body decode errors leave the `.part` file in place for the next in-function attempt or later queue retry. Invalid ZIP responses still remove the partial file because they are not resumable archive data. A successful download validates ZIP magic and atomically renames the part file to the final destination, returning the final file size.

Telegraph upload will stop materializing all ZIP images at once. A blocking ZIP reader task will iterate image entries and send each image over a small channel to the async worker. The async side uploads each image immediately and drops its buffer before receiving the next one. This bounds memory to roughly one image plus small channel/request overhead while preserving URL order for `create_gallery_page()`.

## Error handling and retry semantics

Existing queue retry behavior remains unchanged: transient download/upload failures go back to the appropriate persisted status with backoff and retry count. The new downloader makes each retry more useful by preserving partial archive bytes. Permanent failures still clean up ZIP artifacts where existing worker logic already does so.

## Testing

Add integration coverage for resuming an existing `.zip.part` with `Range`. Update upload worker tests so a ZIP with three images produces three one-image upload requests instead of one large multipart request. Keep existing full download, form download, upload, and publish regression tests green.
