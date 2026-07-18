# E-Hentai archiver.php Page Reference

Source: real captures provided by user (2026-07-18). Used by `eh_client/parser.rs`
and `src/scheduler/eh_engine.rs` to gate GP-spending downloads.

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
| `N/A` | Resolution not available (donor-only or too-large) | `2560x` when donor-only |
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
- H@H table present, all cells `Free` (one cell `N/A` for donor-only 2560x)
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
- POSTing either form WILL charge GP (auto-converts credits if GP insufficient)
- This is the case that needs the GP guard

## Resolution -> Cost Mapping

The two `<form>` blocks have `dltype=org` (left) and `dltype=res` (right). The
H@H table has per-resolution cells.

| Config `resolution` | Which form POSTs | H@H cell used |
|---|---|---|
| `original` / `""` | left (`dltype=org`) | Original column |
| `780x` | right (`dltype=res`) | 800x column (closest match) |
| `980x` | right (`dltype=res`) | 800x or 1280x |
| `1280x` | right (`dltype=res`) | 1280x column |
| `1600x` | right (`dltype=res`) | 1920x column (donor-only) |
| `2400x` | right (`dltype=res`) | 2560x column (donor-only) |

Current code uses the two-form approach (`from_archiver_form` with `dltype`).
GP cost is read from the matching form's Download Cost div.

## Parser Strategy

`parse_archive_download_cost(html, resolution) -> DownloadCost`:

1. Scan for `You unlocked a resample download of this archive on <strong>{date}</strong>`.
   If found AND resolution is a resample (`780x`/`980x`/`1280x`/...), return
   `DownloadCost::Unlocked`. (Original downloads are not free just because
   resample was unlocked.)
2. Find both `<form>` blocks with `dltype=org` and `dltype=res`. Extract the
   Download Cost `<strong>` text from each.
3. Match the cost text:
   - `Free!` -> `DownloadCost::Free`
   - `{n} GP` (strip commas) -> `DownloadCost::Gp(n)`
   - `Insufficient Funds` -> `DownloadCost::Insufficient`
   - `N/A` -> `DownloadCost::Unavailable`
   - anything else -> `DownloadCost::Unknown`
4. Select the form matching the configured resolution (original -> `dltype=org`,
   any resample -> `dltype=res`).
5. If `Unknown`, callers should conservatively reject (do not POST).

## When POST Charges GP

Only `download_archive_with_request()` (the POST to `archiver.php?...` with
`dltype`/`dlcheck`/`hathdl_xres`) charges GP. The prior GETs (gallery page +
archiver page) do not charge GP.

This means:
- `prepare_archive_download()` (GETs + parse) is always safe.
- `download_archive_with_request()` (POST) is the GP-spending step.
- The guard must run between them.

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
