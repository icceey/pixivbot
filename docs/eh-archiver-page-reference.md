# E-Hentai archiver.php Page Reference

Source: real captures provided by user (2026-07-18). Used by `eh_client/parser.rs`
and `src/scheduler/eh_engine.rs` to gate archive size and GP-spending downloads.

## Page Structure (consistent across all 4 samples)

```html
<div id="db">
  <h1>{gallery title}</h1>

  <!-- Optional: only on e-hentai.org when GP balance is shown -->
  <p>Current Funds:</p>
  <p>{gp} GP [...] &nbsp; {credits} Credits [...]</p>

  <div style="position:relative; width:370px; ...">
    <!-- LEFT form = Original Archive -->
    <div style="width:180px; float:left">
      <div>Download Cost: &nbsp; <strong>{cost}</strong></div>
      <form action=".../archiver.php?gid={gid}&amp;token={token}" method="post">
        <input type="hidden" name="dltype" value="org" />
        <input type="submit" name="dlcheck" value="Download Original Archive" />
      </form>
      <p>Estimated Size: &nbsp; <strong>{size} MiB</strong></p>
    </div>

    <!-- RIGHT form = Resample Archive -->
    <div style="width:180px; float:right">
      <div>Download Cost: &nbsp; <strong>{cost}</strong></div>
      <form action=".../archiver.php?gid={gid}&amp;token={token}" method="post">
        <input type="hidden" name="dltype" value="res" />
        <input type="submit" name="dlcheck" value="Download Resample Archive" />
      </form>
      <p>Estimated Size: &nbsp; <strong>{size} MiB</strong></p>
    </div>

    <div style="clear:both"></div>
  </div>

  <!-- Optional: only when a resample download has been unlocked (within 7 days) -->
  <p>You unlocked a <strong>resample</strong> download of this archive on <strong>{date}</strong>
     &nbsp;[<a href="#" onclick="return cancel_sessions()">cancel</a>]</p>

  <!-- Optional: H@H Downloader section -->
  <div>
    <p>H@H Downloader</p>
    <form id="hathdl_form" action=".../archiver.php?gid={gid}&amp;token={token}" method="post">
      <input type="hidden" id="hathdl_xres" name="hathdl_xres" value="" />
    </form>
    <table>
      <tr>
        <td><p>Original</p><p>{size} MiB</p><p>{cost}</p></td>
        <td><p>800x</p><p>{size} MiB</p><p>{cost}</p></td>
        <td><p>1280x</p><p>{size} MiB</p><p>{cost}</p></td>
        <td><p>1920x</p><p>{size} MiB</p><p>{cost}</p></td>
        <td><p>2560x</p><p>{size} MiB</p><p>{cost}</p></td>
      </tr>
    </table>
  </div>
</div>
```

## `{cost}` Token Values

Observed across the 4 captured samples:

| Token | Meaning | Examples |
|---|---|---|
| `Free!` | No GP cost for this resolution | All samples at some resolution |
| `{n} GP` | Costs N GP (thousand separators with commas) | `8,800 GP`, `218 GP`, `747,708 GP` |
| `N/A` | Resolution not available | Observed in the ignored H@H table |
| `Insufficient Funds` | Account lacks GP (variant from external sources) | Not in captured samples but seen in EhViewer test fixtures |

Note: EH uses `Free!` with the `!` in the original/resample cost forms. The H@H
table cells use `Free` without `!`.

## Known Cases

### Case 1: Resample unlocked (exhentai, gp-free gallery)

- Both Download Cost `<strong>` show `Free!`
- Has the `You unlocked a resample download ... on {date}` paragraph
- H@H table present, all cells `Free`
- Safe to POST the resample form; no GP will be charged
- Original form POST is also free in this case

### Case 2: Default exhentai (gp-free gallery)

- Both Download Cost `<strong>` show `Free!`
- No unlocked paragraph
- H@H table present, all cells `Free` except one unavailable cell; the direct
  workflow ignores this table
- Safe to POST either form; no GP will be charged

### Case 3: e-hentai.org (gp-free gallery, shows funds)

- Has `Current Funds:` paragraph (GP + Credits balance)
- Both Download Cost `<strong>` show `Free!`
- H@H table present, all cells `Free`
- Safe to POST either form; no GP will be charged

### Case 4: exhentai GP-required gallery

- No `Current Funds:` paragraph
- Original Download Cost: `8,800 GP`
- Resample Download Cost: `218 GP`
- H@H table cells per resolution: `8800 GP`, `114 GP`, `218 GP`, `376 GP`, `546 GP`
  (observed only; ignored by the direct workflow)
- POSTing either generic form WILL charge GP (auto-converts credits if GP insufficient)
- This is the case that needs the GP guard

## Resolution -> Form, Cost, and Estimated-Size Mapping

### Form-based prepared path

When `prepare_archive_download()` constructs a request from an HTML form, it
uses the generic forms: `dltype=org` (left) and `dltype=res` (right).

| Config `resolution` | Form posted on the generic-form path |
|---|---|
| `original` | left (`dltype=org`) |
| `780x` | right (`dltype=res`) |
| `980x` | right (`dltype=res`) |
| `1280x` | right (`dltype=res`) |

`1600x`, `2400x`, empty, and unknown values are rejected before the direct
workflow makes a GET or POST. Donor resolutions require the separate H@H
Downloader and are not direct archive resolutions.

### Archiver-key compatibility path

When a fetched archiver page exposes an archiver key,
`prepare_archive_download()` constructs a legacy compatibility request with
`dlcheck` plus `hathdl_xres` from that key instead of selecting an HTML form.
`download_archive_with_options()` always constructs that same compatibility
request from its supplied key. Both paths accept only the four validated
resolutions. The legacy field name does not submit the separate live-page
`form#hathdl_form`; that form and its per-resolution H@H table remain ignored,
and the H@H Downloader workflow is out of scope.

`prepare_archive_download()` always fetches and parses the selected generic
form's Download Cost and Estimated Size for guards, even when the resulting
request uses an archiver key. In contrast, `download_archive_with_options()`
with a supplied key does not fetch or parse the archiver page itself. The
displayed decimal MiB value is converted to bytes by rounding up, so the size
guard cannot underestimate an archive.

## Parser Strategy

`parse_archive_download_cost(html, resolution) -> DownloadCost`:

1. Scan for `You unlocked a resample download of this archive on <strong>{date}</strong>`.
   If found AND resolution is a supported resample (`780x`/`980x`/`1280x`), return
   `DownloadCost::Unlocked`. (Original downloads are not free just because
   resample was unlocked.)
2. For `original`, read the `dltype=org` form's Download Cost. For `780x`,
   `980x`, or `1280x`, read the generic `dltype=res` form's Download Cost. The
   H@H form and table are ignored.
3. Match the selected generic form cost text:
   - `Free!` -> `DownloadCost::Free`
   - `{n} GP` (strip commas) -> `DownloadCost::Gp(n)`
   - `Insufficient Funds` -> `DownloadCost::Insufficient`
   - `N/A` -> `DownloadCost::Unavailable`
   - anything else -> `DownloadCost::Unknown`
4. `Insufficient`, `Unavailable`, and `Unknown` are temporary defer states:
   callers do not POST and do not treat them as permanent archive-policy failures.

`parse_archive_download_estimated_size(html, resolution) -> Option<u64>` uses
the same generic-form selection. It returns `None` for missing or malformed
estimates; the H@H form and table cannot bind a size to a direct request. `None`
(and a parsed zero) does not block a download.

## When POST Charges GP

After a resolution passes client validation, the archive POST in
`download_archive_with_request()` or the direct archive-key download APIs can
charge GP. The prior GETs (gallery page + archiver page) do not charge GP.

This means:
- `prepare_archive_download()` rejects unsupported resolutions before HTTP; its
  GETs + parse are otherwise safe.
- Logged-in workers first call `prepare_archive_download()` and then check the
  selected archive estimate against `max_archive_size_mb`. An estimate strictly
  greater than the limit rejects; an equal, missing, or zero estimate passes.
- The GP cost guard and any GP ledger reservation run after the selected-size
  check and before `download_archive_with_request()`.
- `download_archive_with_request()` (POST) is the prepared-request GP-spending
  step. The unauthenticated direct-image path does not use the archive-size gate.

## Account GP Balance (when shown)

`Current Funds:` paragraph only appears on e-hentai.org and only when the
account has a GP/Credits balance worth showing. Format:

```
<p>Current Funds:</p>
<p>{gp_with_commas} GP [...] &nbsp; {credits_with_commas} Credits [...]</p>
```

Can be parsed to know whether the account has enough GP for a paid download
without auto-converting credits. Current implementation does not use this; it
relies on EH's server-side check and the GP guard threshold from config.
