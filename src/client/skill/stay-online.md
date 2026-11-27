# Staying online

"Stay online" means your human can reach you, the session they are talking to, from their phone like a chat. A listener started inside your turn dies when the turn ends, so being online has to be a watch your harness owns. When the session ends, offline is the honest state: their messages wait in the queue and reach you when you reconnect.

## Claude Code: the watch

```text
Monitor({ command: 'roost online --follow --ndjson --timeout 0', description: 'roost: messages from my human', persistent: true })
```

Each output line is one JSON object:

- `type` is `message` (the human wrote), `notification_reply` (an answer to one of your notifications) or `system` (a selftest: ack it like any other row).
- `continuity_context` lines are read-only context.
- `{"type":"batch_end","count":N,"max_ackable_id":ID}` closes each batch.

For each batch: handle it, reply with `roost message` / `roost important`, then `roost ack --up-to <max_ackable_id> --summary "<what you did>"`, but only once the work is really done.

Prove it instead of claiming it: `roost selftest` fails (exit 2) unless a live listener acks its nonce, and `roost whoami` → `listening_state` is what your human sees. One channel has one listener; a second `listen` is refused (exit 2), so read with `roost catchup` instead.

## Pi: the roost_watch tool

roost's Pi extension (`.pi/extensions/roost.ts`, written by `roost setup` inside Pi or by `roost hook install --target pi`) adds the `roost_watch` tool. Call it with `action: "start"`: each batch then arrives in your session as a roost message holding the same JSON lines as above, and wakes you if you're idle. Handle it, reply, and `roost ack --up-to <max_ackable_id> --summary "<what you did>"`. `stop` goes offline, `status` checks it, and the human can type `/roost start|stop|status`. The watch ends with the session. If the tool is missing, the project isn't trusted in Pi yet, or the extension isn't installed (per project, or for all projects with `roost hook install --target pi --global`).

## Without a session-length watch (Codex, Gemini CLI, a plain shell)

Run one round in the foreground of your turn: `roost online` blocks until something arrives (exit 0) or `--timeout` passes (exit 3, default 600 s). Handle, ack, relaunch. Never leave it in a background terminal that doesn't wake you: the server would show you listening while nobody reads. `--follow --timeout 0` refuses outside Claude Code for that reason (`ROOST_EVENT_STREAM=1` overrides it for a harness that does stream output to you; Pi's extension sets it). Between turns you are offline; say so.

## Let a handler decide the ack

```bash
roost listen --all --exec 'printf "Read the batch at \$ROOST_BATCH_FILE (messages from your human), handle them, reply by running roost message, then print a last line: roost-summary: <what you did>" | claude -p --allowedTools Bash,Read,Write'
```

- With Pi instead of Claude Code: `… | pi -p --tools read,bash,write`.
- The batch (`{"messages":[…]}`) arrives on the handler's stdin and in `$ROOST_BATCH_FILE`. An LLM CLI reading its prompt from stdin should take the batch from the file.
- Exit 0 acks the batch's exact ids, with the last `roost-summary:` line as the note.
- Any other exit acks nothing and prints `{"type":"handler_failed",…}`; the batch is served again after the lease, so make the handler idempotent.
- `--handler-timeout` (default 1800 s) kills a stuck handler. A cron job can run one `roost listen --all --exec '<handler>' --timeout 50` per tick.

<!-- roost-skill-end -->
