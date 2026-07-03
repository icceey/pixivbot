# EH Live Regression CLI Design

## Context

The mocked EH/Telegraph tests already cover the deterministic CI path. A real-world regression check is still useful for manually verifying a specific live E-Hentai gallery against the current downloader and Telegraph uploader. The user requested that the gallery URL be provided from the command line and that download and Telegraph upload be separate steps.

## Goals

1. Provide a manual, opt-in live regression entry point for a caller-supplied E-Hentai/ExHentai gallery URL.
2. Separate download and Telegraph upload into independent commands so the downloaded ZIP can be inspected or reused before public publishing.
3. Avoid reading local `config.toml` and avoid accepting secrets on the command line.
4. Keep the entry point out of default CI and normal `cargo test` runs.
5. Reuse `eh_client` production client code instead of duplicating HTTP or parser logic.

## Non-Goals

- Do not add a bot command or scheduler path.
- Do not store or commit any Telegraph token, EH cookie, downloaded ZIP, extracted images, or Telegraph URL artifact.
- Do not make live E-Hentai or Telegraph access part of `make ci`.
- Do not bypass E-Hentai/Telegraph access controls or content policy; the command is a local operator tool.

## Interface

Add an example binary at `eh_client/examples/live_eh.rs`.

### Download

```powershell
cargo run -p eh_client --example live_eh -- download <gallery-url> --out target/live-eh/gallery.zip
```

Behavior:

- Parse `<gallery-url>` as `https://e-hentai.org/g/{gid}/{token}/` or `https://exhentai.org/g/{gid}/{token}/`.
- Build an `EhClient` using the URL host as the base site.
- Use existing archive download flow when cookies are configured through environment; otherwise use the direct image fallback that downloads all discovered pages or fails.
- Write the ZIP to the `--out` path using existing temp-file safety in `eh_client`.
- Print the final ZIP path and byte count.

Environment:

- `EH_IPB_MEMBER_ID`, `EH_IPB_PASS_HASH`, and `EH_IGNEOUS` are optional cookies for authenticated archive downloads and ExHentai access.
- No local `config.toml` is read.

### Upload Telegraph

```powershell
$env:PIXIVBOT_LIVE_TELEGRAPH_TOKEN = "..."
cargo run -p eh_client --example live_eh -- upload-telegraph target/live-eh/gallery.zip --title "EH Test Gallery"
```

Behavior:

- Read an existing ZIP path from the command line.
- Extract image entries from the ZIP into memory one at a time or in bounded batches.
- Upload images using the configured `ImageUploader` implementation, defaulting to pixi.mg and supporting the same configured image hosting backends as production.
- Create a Telegraph page with `TelegraphClient::create_gallery_page()`.
- Print the resulting `https://telegra.ph/...` URL.

Environment:

- `PIXIVBOT_LIVE_TELEGRAPH_TOKEN` is required for upload.
- The token is intentionally not accepted as a command-line argument to avoid shell history leaks.

## Error Handling

- Missing command arguments produce usage text and non-zero exit.
- Unsupported gallery URL host or malformed gid/token returns a clear parse error.
- Download failure removes partial outputs through existing client temp-file behavior.
- Upload failure does not delete the input ZIP.
- Missing `PIXIVBOT_LIVE_TELEGRAPH_TOKEN` for `upload-telegraph` returns a clear error before reading/uploading images.

## Testing Strategy

- Add unit-testable helper functions in the example for parsing gallery URLs and CLI arguments.
- Add tests for:
  - valid E-Hentai URL parsing,
  - valid ExHentai URL parsing,
  - malformed URL rejection,
  - token environment variable requirement for upload mode,
  - argument parsing for `download` and `upload-telegraph`.
- Do not run live network operations in tests.

## Verification

Default verification:

```powershell
cargo test -p eh_client
cargo run -p eh_client --example live_eh -- help
```

Manual live regression, run only by an operator with rights/credentials:

```powershell
cargo run -p eh_client --example live_eh -- download https://e-hentai.org/g/4010440/a7d57a69b7/ --out target/live-eh/gallery.zip
$env:PIXIVBOT_LIVE_TELEGRAPH_TOKEN = "..."
cargo run -p eh_client --example live_eh -- upload-telegraph target/live-eh/gallery.zip --title "EH Test Gallery"
```

## Acceptance Criteria

- The gallery URL is supplied only via command line, not hardcoded in tests.
- Download and Telegraph upload are independent commands.
- No credentials are accepted on command line or read from `config.toml`.
- Default tests stay offline and deterministic.
- Manual download writes a validated ZIP or fails without leaving a partial final file.
- Manual upload prints a Telegraph URL when provided a valid ZIP and token.
