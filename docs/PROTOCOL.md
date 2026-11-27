# roost wire protocol

What the roost server speaks: the agent API that `roost <command>` uses (`/api/v1`), and the human API behind the PWA (`/api/human`).

## Transport conventions

- Base URL: `roost setup --url` / `ROOST_URL` / the stored env file. JSON bodies everywhere.
- Errors: `{"error": "<message>"}`, plus a machine `code` on contract errors (422).
- `GET /up`: health check, unauthenticated, `ok`.

### Request identity headers (advisory, never auth)

Sent by the CLI on every channel-key call:

| Header | Value |
|---|---|
| `Authorization` | `Bearer rst_…` (channel key) |
| `X-Roost-Fingerprint` | `fp_` + 24 hex: sha256 of host, user, `ROOST_AGENT`, the env-file path and a per-identity salt |
| `X-Roost-Label` | URI-encoded label (`ROOST_LABEL` / `ROOST_AGENT` / hostname, ≤80 chars) |
| `X-Roost-Consumer-Kind` | `listen` / `watch` (`listen --follow --timeout 0`), only on consuming commands |

The server uses these for attribution (`acked_by_label`, `sent_by_label`, `being_handled_by`) and presence (consumers in whoami).

## Auth model

- **Channel key** `rst_<random>`: a bearer token scoped to one channel. All agent endpoints use it. The server stores and looks up only its hash.
- **Owner session**: the PWA's passkey session (see "roost's human API").
- **Claim codes**: minted by the owner (15 min TTL, single use, like `AB12-CD34`). Exchanging one **rotates the channel key** and revokes the previous holder.

## Agent endpoints (`/api/v1`)

| Method & path | Used by |
|---|---|
| `POST /claim` | `setup --claim` |
| `POST /claim/ownership` | `setup` |
| `PATCH /channels/:id` | `setup` (operating contract) |
| `GET /channels`, `GET /channels/:id` | not used by the CLI |
| `GET /whoami` | `whoami`, `doctor`, `presence`, `selftest`, `ack`, setup's clobber guard |
| `POST /notify` | every send, `ask`, `approval`, `approve`, `hello` |
| `GET /notifications/:cid` (`?wait=`, `&wake_on_message=true`) | `wait`, `ask`, `approve`, `hello` |
| `DELETE /notifications/:cid` | `cancel` |
| `GET /notifications` (`?pending=`, `?limit=`) | `inbox` |
| `GET /inbox/summary` | `inbox --summary` |
| `GET /messages` (`?wait=`, `all=`, `lease=`, `since=`, `history=`, `before=`, `continuity=`) | `listen`/`online`, `catchup`, `doctor`, a wait woken by a message |
| `POST /messages/ack` | `ack`, `listen --exec`, `listen --ack-on-read` |
| `POST /selftest`, `GET /selftest/:id` | `selftest` |
| `POST /typing` | `typing`, `listen --exec` |

There is no WebSocket: blocking commands use the `?wait=` long-polls, which wake as soon as their event happens.

### `POST /claim` (no auth)
Request: `{"code": "<claim code>"}` plus identity headers. The fingerprint binds the claim; a retry from the same fingerprint within the TTL re-exchanges.
Response 2xx: `{key, channel: {id, name}, base_url}`. The CLI stores `base_url` (the server's public URL) and `key`.
404: unknown, expired or used code.
Side effects: rotate the channel key, revoke the old one, bump `claim_generation` when the owner changes.

### `POST /claim/ownership` (channel key)
Request: `{fingerprint, label}`. 200 → `{claim: {claim_generation, claimed_by_label, claimed_by_fingerprint, claimed_at}}`.

### `PATCH /channels/:id` (channel key)
`{"operating_contract": {listen_mode, keep_connection_alive, mirror_in_origin_session?, quiet_when_idle?}}`: how the agent says it operates, shown to the owner. A channel key may not write `notify_contract` (403).

### `GET /whoami` (channel key)
`channel {id, name, …}`, `devices` (live Web Push subscriptions), `device_reach`, `claim {…}`, `stale_from_prior_claim`, presence (below), `operating_contract`, `notify_contract`. 401 = dead key.

### `POST /notify` (channel key)
Request: `title` (required), `body`, `body_markdown`, `subtitle`, `template`, `template_kind` (`message|important|urgent|event`, aliases `fyi`/`report`/`alert`), `profile`, `event_at`, `lead_minutes`, `urgency`, `url`, `copy`, `actions` (built-in ids: `snooze, done, reschedule, mute, reply, yes, no, approve, reject, accept, decline, later`), `custom_actions [{id, label, style?, confirm?, biometric?, terminal?}]`, `deliver_at`, `reply_to`, `correlation_id`, `thread_id`, `after`, `collapse_key`, `sent_note`, `mirror_reply` (false for `approve`), `escalate`. Unknown fields are tolerated.
Response **201**: the notification plus `registered_devices`, `escalation {state, after_minutes, fires_at}`, `degraded`, `degrade_reason`, `requires_action`, `render_mode`, `updated`, `after`, `remind_at`, and `listening` / `nobody_listening` (whether a consumer holds the queue right now, so the agent knows an answer or reply won't be read until it listens). A blocking send without `--timeout` waits `suggested_ask_timeout`, else 3600 s when `requires_action`, else 600 s.
Semantics: `correlation_id` is idempotent (a re-POST returns the existing row, or edits it while scheduled or queued).
- **`after: <cid>`** (decision queue): if that notification on the same channel is still open, this one is stored with status `queued` and not pushed. It is released (to `pending` and pushed, or to `scheduled` if its `deliver_at` is still ahead) once the earlier one is answered or cancelled. A snooze doesn't release it. Chains work (C after B after A). Unknown cid → 422 `unknown_after`; naming itself → 422 `invalid_after`. A queued notification can be cancelled, which releases the next in line.
- **`collapse_key`** (≤256): pushes carry a Web Push `Topic` (a 32-char hash of channel and key), so the push service replaces an undelivered one, and a shared banner tag, so the phone replaces the shown one. In the PWA's Home feed and badge, a newer plain notification with the same key supersedes the older ones; an actionable one is never hidden.
- **Event reminders**: an `event` with an `event_at` that has a UTC offset gets a second push `lead_minutes` (0..10080, default 15) before it, titled "⏰ In N min: <title>". `remind_at` in the response says when; it is null when that time is under 5 s away or `event_at` has no offset. Invalid `lead_minutes` → 422 `invalid_lead_minutes`. The reminder still fires after an answer, unless the answer was mute, dismiss, no, reject or decline.
 Unknown built-in action ids are dropped. Invalid custom actions → 422. iOS web push has no buttons, so `render_mode` is `detail_only` or `plain`: actions are in the app, one tap away.

### `GET /notifications/:cid` (channel key)
`wait=N` (≤25) holds until the answer changes, the notification is cancelled, or the deadline (then the normal body). `wake_on_message=true` also returns early, with `messages_pending: true`, when the conversation queue has something. A held wait on a snoozed row waits for the next event.
Body: `responded`, `chosen_action {kind, action_id, label, text, at, snooze_until}`, `messages_pending`, `escalation`, `early_resolution`, plus the notification fields.
`chosen_action.kind`: done→`completed`, mute→`muted`, reply→`replied`, snooze/reschedule→`snoozed`, dismiss→`dismissed`, anything else→`acted`. The CLI keeps waiting only on `snoozed`, and stops with exit 2 when `status` is `cancelled`.
404 = unknown cid (the CLI keeps polling), 401/403 = dead key. A read after the answer stamps `reply_delivered_at`.

### Time-based transitions (the scheduler)
One task sleeps until the earliest `next_fire_at`. Sends, answers and cancels wake it early, and rows that came due while the server was down fire at startup, late but not dropped.
- **`deliver_at`**: `scheduled` → `pending` at fire time, then the push. A re-send with the same cid while scheduled edits in place. A terminal answer before delivery cancels it and the poll adds `early_resolution: true`.
- **Snooze**: `snoozed` → `pending` at `snooze_until`, then a re-push. `chosen_action` keeps the snooze, so a waiting CLI holds on until the real answer.
- **Escalation fuse** (urgent profile, channel `max_urgency = alarm`; otherwise `disabled_by_channel`): `pending` → `armed` on every delivery (`fires_at` = delivery + the channel's `escalate_after_secs`, default 300, or 60 s at most when the send sets `escalate: true`) → `fired` once a re-push lands, or `fired_not_pushed` while no device takes it. An answer or snooze before it fires → `defused`. A snooze re-fire re-arms it.
- **Event reminder**: `next_fire_at = remind_at` once the event is delivered (unless a snooze or the fuse holds the slot), then one reminder push.
- **Decision queue**: not time-based. An answer (not a snooze) or a cancel releases the rows queued `after` it at once.
- Each transition claims its row by matching the old `next_fire_at`, so a concurrent answer or cancel wins.

### Presence and selftest
A consumer (keyed by `X-Roost-Fingerprint`, labelled by `X-Roost-Label` / `X-Roost-Consumer-Kind`) is live while it holds a `GET /messages?wait=`, and for 60 s after any queue read. `whoami` reports `consumers[] {fingerprint, label, kind, transport, live, listening, last_seen_at}`, `listening`, `listening_state` (`listening` | `offline`) and `consumer_conflict`. The registry is in memory, so a restart reads as offline until the next read.

`POST /selftest {window_seconds?}` (5..600, default 30) puts a `kind:"system"` row on the queue (`ref: {type:"selftest", nonce, expires_at}`), wakes listeners, and answers 201 `{id, status:"pending", nonce, window_seconds, expires_at}`; the id is the message id. `GET /selftest/:id` is `passed` once the row is acked before `expires_at`, `failed` after it, else `pending`. System rows never show in the PWA conversation.

### `POST /typing` (channel key)
`{seconds?}`: show the human the typing dots for that long (default 60, clamped to 3..300); `0` clears them. Response `{typing, seconds, clamped, expires_at}`. In memory and display-only: no push, no history. Any send on the channel clears them. `listen --exec` raises them (90 s, renewed with the lease) while the handler runs and clears them after, unless `ROOST_NO_AUTO_TYPING=1`.

### `DELETE /notifications/:cid`
Cancels a still-scheduled or queued notification (it also kills a snooze re-fire). 2xx means cancelled, and the call is idempotent. 404 = unknown cid, 409 = already delivered or answered.

### `GET /notifications`
The channel's sent list. `?pending=true` returns delivered-and-unanswered notifications, `?limit=`, `?all=`. The CLI prints it as is.

### `GET /messages` (channel key)
The inbound queue. Response `{messages: [...], stale_from_prior_claim, continuity_contexts}`.
- Default: deliverable conversation rows, oldest first, ≤100. Serving a row stamps `delivered_at` and opens a lease (`lease=` 60..3600, default 600). An un-acked row re-serves after `delivery_expires_at`.
- `all=true` adds `notification_reply` rows (mirrors of answers unless `mirror_reply:false`) and `system` rows.
- `wait=N` long-poll (≤25). `since=<id>` gives a newer-only tail. `history=true` is a read-only full thread, newest first, ≤200, paged with `before=<id>`.
- Row: `{id (integer), channel_id, kind, body, thread_id, created_at, delivered_at, processed_at, delivery_expires_at, consumed_at, correlation_id?, ref?, action_id?, text?, being_handled_by?, acked_by_label?, handler_summary?, handled_state?}`.

### `POST /messages/ack` (channel key)
Body: `{up_to: <id>}` or `{ids: [...]}`, optional `state: "processed"|"delivered"` (`delivered` = lease renew), and an optional `summary` (≤1000 chars).
Response: `{acked: n, skipped?: n}`, or `{renewed: n}` for a renew, or `{acked: 0, annotated: n}` for a late summary. `up_to` skips rows under another consumer's live lease and rows never served.

### Reply webhook (`reply_to`)
`POST` to a public HTTPS URL. Loopback, private, link-local and CGNAT ranges are refused at send time with 422 `invalid_reply_to`. Body: `{type: "roost.event", event, action_id, label, text, correlation_id, notification_id, title, delivered_at, payload, occurred_at}`. Header `X-Roost-Signature: sha256=<hex HMAC-SHA256(raw_body, callback_secret)>`, where the secret is `ROOST_CALLBACK_SECRET`. About 8 attempts with backoff.

## roost's human API

`/api/human` serves the PWA and the tests. A caller is either:
- **the owner**: a session cookie (`roost_session`, HttpOnly, SameSite=Strict, Secure on https, 90 days), obtained through a passkey login; or
- **the admin token** (`Authorization: Bearer $ROOST_ADMIN_TOKEN`), which is for tests and development and **skips the Face ID gate**. Leave it unset in production.

Passkeys are WebAuthn with user verification required (webauthn-rs passkey flows). The RP ID is the base URL's host, which must be a domain name. The first passkey, and any later one, needs a one-time enrollment code: `roost passkey enroll --base-url …` prints `<base>/enroll#<code>` (15 min, single use). `roost passkey list` and `roost passkey remove <id>` (which also kills its sessions) manage them. Challenges are stored server-side, expire after 5 min and are single use.

**Face ID gate.** A custom action with `biometric: true` (`approve`'s Allow, `--gated`, `:biometric`) can only be answered by an owner session that sends a fresh assertion: `POST /notifications/:id/gate {action_id}` → `{challenge_id, options}`, then `respond` with `gate: {challenge_id, credential}`. The challenge is bound to that notification and action.

| Method & path | Does |
|---|---|
| `GET /session` | `{authenticated, enrolled, passkeys_available, vapid_public_key}`; needs no auth |
| `POST /enroll/start` `{code}` · `POST /enroll/finish` `{challenge_id, credential, label?}` | add a passkey, set the session cookie |
| `POST /login/start` · `POST /login/finish` `{challenge_id, credential}` | sign in |
| `POST /logout` | end the session |
| `POST /push/subscriptions` (a `PushSubscription.toJSON()`) · `DELETE` `{endpoint}` | register or drop this device |
| `POST /push/test` | push to every device → `{accepted, devices}` |
| `GET /events` | Server-Sent Events: `event: changed`, `data: {"channel_id": N}` whenever something the PWA shows changes on that channel (`null` = refresh everything); 15 s keep-alives |
| `GET /feed[?all=true]` | open items (or everything), newest first, with `channel_name`, `open`, `action_specs[].biometric`. Open = not answered, cancelled or queued; a plain message only until seen; not superseded by a newer one with its collapse key |
| `GET /badge` | `{open}`: the count of open feed items, for the app icon badge |
| `GET /notifications/:id` | one item |
| `POST /viewing` `{channel_id \| null, home?}` | which channel's conversation or item is on screen, or `home: true` (with `channel_id: null`) for Home showing every channel; refreshed every 5 s while visible, cleared on hide (beacon plus keepalive fetch, since iOS drops either now and then), expires after 12 s. New and re-fired notifications on that channel (on any channel, for Home) skip the push and count as delivered (escalations still push) |
| `POST /notifications/:id/seen` · `/delivered` | receipts (the PWA on open, the service worker on push); they wake waiting long-polls |
| `POST /notifications/:id/gate` `{action_id}` | open a Face ID gate |
| `POST /notifications/:id/respond` `{action_id, text?, snooze_minutes?, gate?}` | answer |
| `GET /channels` · `POST /channels` `{name}` | list (with `listening`, `last_seen_at`, `typing_until`) or create (→ `{channel, claim_code, expires_at}`) |
| `PATCH /channels/:id` `{name?, color?, icon?, max_urgency?, escalate_after_secs?, default_profile?}` | the owner's channel settings. `color` is `#rrggbb`, `icon` a glyph name, both clearable with `null`; `max_urgency` `normal\|persistent\|alarm`; `escalate_after_secs` 60..86400; `default_profile` `message\|important\|urgent` (applies to sends that name no type) → the channel. 400 on a bad value |
| `DELETE /channels/:id` | delete a channel with its notifications, messages and claim codes; its key stops working (also `roost channel delete <id>`) |
| `POST /channels/:id/claim` | mint a fresh claim code |
| `GET /channels/:id/messages` · `POST` `{body, thread_id?}` | the thread, and the composer |
| `POST /channels/:id/notifications/:cid/respond` | answer by cid (tests); also takes `snooze_seconds` |

`roost channel create <name>` and `roost channel claim-code <id>` do the same from the shell.

## Web Push

VAPID with `aes128gcm` payloads, sent with reqwest. The key is `ROOST_VAPID_KEY_FILE` (PEM) or, without it, one generated on first run and stored in `settings`. The `sub` claim is `ROOST_VAPID_SUBJECT`, defaulting to the base URL. Payload: `{id, title, body (≤240 chars), channel, profile, requires_action, sticky, url: "/n/:id", tag, badge}`. Re-fires prefix the title with ⏰, escalations with "🚨 Still waiting:", reminders with "⏰ In N min:". `tag` is `c-<topic>` for a collapse key, `r-<id>` for a reminder, else `n-<id>`. `badge` is the open count, which the service worker sets on the app icon. `sticky` (`requireInteraction`) is an urgent send under a channel ceiling above `normal`. `Urgency: high` except for `message` and anything capped to `normal` (escalations and reminders are always high), TTL 24 h, plus `Topic` for a collapse key. A 404/410 drops the subscription, so `registered_devices` counts live subscriptions. Every device is pushed in parallel, and each push and webhook attempt gives up after 10 s, so a hung push service delays a send by at most that. The service worker always shows the notification (iOS revokes subscriptions that skip it), posts the `delivered` receipt, and opens `/n/:id` on tap.
