# EH queue status command design

## Context

E-Hentai downloads use a persistent, per-chat, multi-stage queue. The primary `status` moves through `pending`, `downloading`, `downloaded`, `uploading`, `uploaded`, and `publishing` before reaching `done`, `failed`, or `canceled`. Background downloads retain primary status `pending` while `background_download_status` independently records `pending` or `running`. The bot currently has no user-facing way to inspect this state, and the existing `count_pending_eh_downloads()` method is global and does not distinguish background work.

The requested command is a current-chat view for ordinary users. It must show active items and one recent terminal record without exposing another chat's work or persisted internal errors.

## Goals

- Add `/estatus` as an ordinary EH command, visible only when EH is enabled.
- Show a current snapshot of the invoking chat's active EH queue.
- Summarize active work by friendly stage and list each visible gallery by GID, title, and stage.
- Include the most recently enqueued terminal record for the chat.
- Represent background download activity accurately.
- Keep output bounded, MarkdownV2-safe, and free of internal error details.

## Non-goals

- Do not provide a global or administrator queue view.
- Do not add cancellation, retry, queue mutation, or pagination controls.
- Do not display all historical records.
- Do not expose persisted `error`, retry scheduling details, local paths, subscription IDs, or other internal fields.
- Do not change worker scheduling or queue state transitions.

## Approaches considered

1. **Dedicated `/estatus` snapshot command (selected).** Add a repository snapshot query scoped by `chat_id`, then format the result in the EH handler. This preserves the database boundary and matches the existing EH command family.
2. **Raw `/equeue` record listing.** This is marginally smaller but couples Telegram output to database status strings and makes future queue changes user-visible.
3. **Extend `/info`.** This would naturally be an operational view, but `/info` is admin-private-only and conflicts with the selected current-chat user scope.

## Architecture

### Command and routing

Add a no-argument `Command::EStatus` variant with the command name `/estatus`. Include it in `Command::user_commands()` only when `has_ehentai` is true, so admin and owner command menus inherit the same visibility. Route it through `BotHandler::dispatch_command()` to an EH-specific handler using the invoking `chat_id`.

The handler follows the same chat-access and mention middleware as existing ordinary EH commands. It does not accept a target chat parameter.

### Repository snapshot

Add a read-only repository method scoped by an explicit `chat_id`. It returns:

- every active row for that chat, ordered by `created_at` ascending; and
- one terminal row for that chat, chosen as the most recently enqueued row by `created_at` descending.

Active primary statuses are exactly `pending`, `downloading`, `downloaded`, `uploading`, `uploaded`, and `publishing`. Terminal primary statuses are exactly `done`, `failed`, and `canceled`. The query must never use the existing global pending count for user output.

The repository owns status-set filtering and ordering. The bot handler owns user-facing labels and presentation. The two reads form a best-effort current snapshot; strict transactional consistency is unnecessary for a status display.

### Status presentation

Each active row receives one friendly stage label. Background state takes precedence while primary status is `pending`:

| Stored state | User-facing stage |
| --- | --- |
| `status=pending`, `background_download_status=running` | 后台下载中 |
| `status=pending`, `background_download_status=pending` | 后台排队 |
| `status=pending`, no background state | 排队中 |
| `status=downloading` | 下载中 |
| `status=downloaded` | 等待上传或发送 |
| `status=uploading` | 上传中 |
| `status=uploaded` | 等待发送 |
| `status=publishing` | 发送中 |

Terminal rows map to `已完成`, `失败`, or `已取消`. No error text is included.

The response contains:

1. an `EH 下载队列` heading;
2. total active count plus counts for the friendly stages that are present;
3. up to 20 active entries in queue order, each showing GID, a title truncated safely to 80 Unicode scalar values, and the friendly stage;
4. an omitted-count line when more than 20 active entries exist; and
5. a `最近记录` line for the selected terminal row, when present.

The 20-entry limit is an upper bound. The formatter budgets the complete message, including MarkdownV2 escaping and the recent terminal record, against Telegram's 4096 UTF-16-unit limit. If necessary it shows fewer active entries and increases the omitted count accordingly; the summary still covers every active row.

If there are no active entries, the response says the current chat has no active EH download tasks and still includes the recent terminal record when available. All dynamic titles and labels are escaped for MarkdownV2.

## Data flow

1. Teloxide parses `/estatus` and existing middleware validates the chat and mention rules.
2. `dispatch_command()` calls the EH queue-status handler with the invoking `chat_id`.
3. The handler requests the chat-scoped snapshot from `Repo`.
4. The handler derives friendly stages, computes counts, truncates the displayed list and titles, and formats MarkdownV2.
5. The bot sends one bounded status message to the same chat.

## Error handling

- A repository error is logged with its full error chain using `tracing`.
- The user receives only `❌ 获取 EH 下载队列状态失败，请稍后重试`.
- Unknown stored states are not expected because the repository filters the known active and terminal sets. If a row still reaches formatting with an unknown state, label it `未知状态` rather than exposing the raw value or failing the command.
- Concurrent worker transitions may make the displayed snapshot immediately stale; the command presents a snapshot and makes no stronger consistency guarantee.

## Testing

- Command parsing and visibility tests verify `/estatus` is present only when EH is enabled and inherited by admin/owner menus.
- Repository tests create active and terminal rows for two chats and verify strict chat isolation, active ordering, terminal exclusion from the active list, and selection of the most recently enqueued terminal row.
- Repository coverage includes background `pending` and `running` records without treating other chats' records as part of the result.
- Formatting tests cover every friendly stage, empty active queues, a recent terminal record, more than 20 active entries, Unicode-safe title truncation, MarkdownV2 escaping, omission of persisted error text, and the 4096 UTF-16-unit whole-message budget with non-BMP titles.
- Run focused tests for the new command, snapshot query, and formatter, then run `make ci` as required for Rust changes.

## Self-review

- Placeholder scan: no placeholders or incomplete requirements.
- Internal consistency: `/estatus` is current-chat-only; repository filtering and all displayed data use the same `chat_id`.
- Scope: the design adds one read-only command and direct tests without queue mutations or worker changes.
- Ambiguity: command name, access scope, active/terminal sets, history selection, ordering, truncation, status labels, and error disclosure are explicit.
