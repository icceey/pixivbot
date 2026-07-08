# IPFS dual-gateway Telegraph design

## Context

Telegraph pages that embed IPFS images through public gateways such as `ipfs.io` and `dweb.link` can fail to produce immediate Telegram link previews. Runtime checks on a real Telegraph page showed the first `og:image` was small enough for Telegram preview, but the public gateway response included `X-Robots-Tag: noindex, nofollow`. The custom gateway `https://ipfs-gw.moyuteam.me/ipfs/<cid>` returned the same image without that header and is therefore a better URL for Telegram's crawler.

Current code has only one IPFS gateway URL in `IpfS3UploaderConfig.gateway_url`. `IpfS3Uploader` returns final image URLs directly, `TelegraphClient::create_gallery_page()` stores those URLs in Telegraph `<img>` nodes, and the queue stores only the returned Telegraph page URL. There is no Telegraph `editPage` support, no list of all created Telegraph pages, and no delayed post-send rewrite state.

## Goal

When using the ipfS3 image uploader, create Telegraph pages with preview-friendly IPFS gateway URLs so Telegram can fetch link previews. After the Telegraph link has been sent and a configurable delay has elapsed, rewrite the Telegraph page images back to the public gateway URLs.

## Design

Add optional dual-gateway support to the ipfS3 upload path. The uploader will still upload once and extract a CID from the S3-compatible response, but it will be able to produce two URL forms for the same CID: a preview URL for Telegraph creation and a public URL for the delayed final state. The default behavior remains unchanged when no preview gateway is configured.

Telegraph page creation will return a richer result that includes every created page path and enough node content to later rewrite it. Multi-page galleries must be handled because `create_gallery_page()` can split large galleries into several Telegraph pages linked by `Next Page →`. The delayed rewrite will call Telegraph `editPage` for each created page, replacing only image `src` values that match the preview gateway with corresponding public gateway URLs. Non-IPFS links and `Next Page` links are left unchanged.

The rewrite trigger is tied to the publish stage: after the bot successfully sends the Telegraph link, the queue row records `telegraph_rewrite_after = now + 10 minutes` and retains serialized rewrite metadata. A lightweight worker periodically claims due rows and performs the `editPage` calls. This makes the rewrite survive bot restarts and avoids in-process timers that would be lost on crash. The row is marked complete when all pages are rewritten; failures are retried with the existing retry/backoff style and do not block archive delivery or normal publishing.

## Configuration

Extend `[image_upload.ipfs3]` with:

- `preview_gateway_url`: optional. If set, Telegraph pages are initially created with this gateway. It must be an HTTP(S) base URL and is normalized without a trailing slash. Example: `https://ipfs-gw.moyuteam.me/ipfs`.
- `preview_rewrite_delay_sec`: optional, default `600`. Delay after successful Telegraph link send before rewriting to `gateway_url`.

Existing `gateway_url` remains the public/final gateway. If `preview_gateway_url` is unset or equals `gateway_url`, no rewrite metadata is stored and no rewrite work is scheduled.

## Data model

Add nullable columns to `eh_download_queue`:

- `telegraph_rewrite_after TIMESTAMP NULL`
- `telegraph_rewrite_data TEXT NULL`
- `telegraph_rewritten_at TIMESTAMP NULL`

`telegraph_rewrite_data` stores JSON with page paths, titles, and page nodes containing preview image URLs plus the preview/public gateway pair. It is cleared after a successful rewrite. Existing rows default to no rewrite.

## Error handling

Preview gateway failure does not affect image upload because upload still happens against ipfS3. If the preview gateway is unreachable, the Telegram preview may fail, but the Telegraph page still exists. Delayed `editPage` failures keep rewrite metadata and retry later. If a page was manually edited or deleted, the rewrite worker logs the Telegraph API error and eventually marks the rewrite as failed only after retry exhaustion; the already-sent Telegraph link remains usable.

## Testing

Unit tests should cover CID URL pair generation, gateway URL normalization, image-node rewriting, and multi-page rewrite metadata. Repo/scheduler tests should cover scheduling rewrite only after the Telegraph link is sent, not after upload, and preserving normal publish completion. Telegraph client tests should mock `editPage` and assert the outgoing content contains public gateway URLs.
