# EH direct archive form regression fix

## Context

An EH Archiver page exposes two direct archive forms (`dltype=org` and
`dltype=res`) plus a separate `H@H Downloader` form (`hathdl_xres`). Commit
`35ea87a` made known resample resolutions prefer the H@H form. The scheduler
then POSTs an H@H request and passes its HTML response to the direct archive
redirect parser, which fails with `archive redirect URL not found`.

## Decision

Direct archive downloads must select only the form whose `dltype` matches the
requested archive class:

- `original` or an empty resolution selects `dltype=org`;
- every non-original resolution selects `dltype=res`.

The selected direct form also remains the source of download cost and estimated
size. The H@H form and its resolution table do not describe the direct archive
request and must not influence its request body or guard metadata. Supporting
the H@H Downloader workflow is out of scope.

## Data flow and errors

`prepare_archive_download()` fetches the Archiver page, parses cost and size for
the matching direct form, and builds an `ArchiveDownloadRequest` from that form.
The existing GP/size guards then run before the existing POST, redirect parsing,
ZIP transfer, validation, and atomic rename. Existing fail-closed behavior for
missing or unparseable direct-form metadata remains unchanged.

## Testing

Add a regression fixture modeled on the observed page but with synthetic IDs and
URLs. When both direct forms and `hathdl_form` are present, a `1280x` request must
POST `dltype=res` and `dlcheck=Download Resample Archive`, must not POST
`hathdl_xres`, and must complete the mocked ZIP download. Unit tests must also
assert that direct-form cost and size win over differing H@H table values.

## Self-review

- No placeholders or incomplete requirements.
- Request selection and GP/size metadata use the same direct form.
- Scope is limited to the confirmed form-selection regression and tests.
- H@H Downloader support is explicitly excluded.
