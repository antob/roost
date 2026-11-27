---
name: roost
description: Reach your human on their iPhone and get answers back (roost). Send notifications (message/important/urgent/event), ask questions with buttons and wait for the answer, gate risky steps on a Face ID approval, and read what the human writes to you. Use when finishing long tasks, needing a decision or approval, reporting results, or when your human asks you to stay reachable.
# roost-skill v1
---

# roost: notify your human, get answers back

Commands are `roost …` (one binary on PATH). Your channel key lives in roost's config (`~/.config/roost/…`), never in your context; if a command says there is no identity, see "Setup" below.

## Pick the send

| Situation | Run |
|---|---|
| Just inform: a result, a log, nothing to do | `roost message --title … --body …` |
| Something they should see or act on soon (the default) | `roost important --title … --body …` |
| You need a decision and can't proceed without it | `roost ask --title … --body … --actions yes,no` |
| A formal go/no-go (money, deletion, anything irreversible) | `roost approval --title … --body …` |
| Gate one of your own risky commands on the human | `roost approve "<question>"`: exit 0 only on Allow (with Face ID) |
| Something happening at a known time | `roost event --title … --event-at 2026-06-26T14:00+02:00`: a second "starting soon" push comes 15 min before (`--lead-minutes N`) |
| They lose something if they don't act within minutes (rare) | `roost urgent --title … --body …` |

- Always give a real `--title` and `--body`: the lock-screen banner shows exactly those two. `--body-markdown` (or `--body-markdown-file path|-`) is shown when they open it.
- Tapping a notification opens it in the roost app, where the buttons are. Free-text replies always work too.
- `urgent` alerts again once if it stays unanswered (after 5 min by default, or 1 min with `--escalate`), when the human's channel setting allows re-alerts. Keep it for real emergencies.
- Progress updates: give each send the same `--collapse-key build`, and the newest replaces the previous one on the phone instead of stacking up.
- `roost <command> --help` lists every flag.

## Buttons and answers

- Built-in buttons: `--actions yes,no` · `approve,reject` · `accept,decline` · `done` · `later` · `snooze` · `reply`.
- For a typed answer use `--actions reply` alone. A decision button together with `reply` is refused (the human taps the easy button and you never get the text). Ask one question per send.
- Your own buttons: `--custom-action id:Label` (add `:destructive`, `:confirm`, `:biometric` for Face ID); `--gated` adds one Face ID confirm button.
- Send and continue (default): the answer arrives later in `roost online`. Block instead with `--wait`, `roost ask`, or later with `roost wait <correlation_id>`.
- A blocking command prints the answer as JSON (`chosen_action`: `action_id`, `label`, `text`). If the human writes in the conversation instead, it returns early with `{"kind":"human_message", …}`: handle that, then `roost wait <correlation_id>` again.
- Several questions at once? Queue them: `--after <correlation_id>` holds a send until that earlier one is answered (or cancelled), so the human gets one question at a time.
- `roost cancel <correlation_id>` cancels a scheduled or queued send (`--deliver-at`) before it fires. `roost inbox` lists what you sent and the answers.

## Exit codes

`0` answered or done · `1` usage error (fix the command) · `2` error, or the send was cancelled so no answer will come · `3` no answer yet, not a failure · `4` timed out and the server never answered: the channel looks broken, run `roost doctor`.

## The conversation

The human can write to you in the roost app. Their messages wait in a queue until something reads them:

- `roost online` (= `roost listen --all`) blocks until messages or answers arrive, prints them as one JSON array, and exits. `roost catchup --digest` shows the conversation without consuming anything.
- Ack only after the work is done: `roost ack --up-to <id> --summary "<what you did>"`. The human sees ✓✓ and your summary. Un-acked messages are served again after ~10 minutes, so a crash never loses one. If handling failed, don't ack: say so in a message.
- Reply by sending (`roost message` / `roost important`). Your terminal output reaches nobody.
- A message just landed and the reply will take more than ~15 s? Run `roost typing` first (`roost typing 120` for longer): the human sees the typing dots. They clear on your next send and expire by themselves; `listen --exec` raises them for you.
- Asked to stay online, or to be reachable like a chat? Read `references/stay-online.md` first.

## Setup

- `roost setup --claim <code> --url <server>` with a claim code from the roost app (the channel's ⚙ → New claim code). Inside a git project the key is scoped to that project; elsewhere it is the machine-wide key.
- Two agents on one machine or project: give each `ROOST_AGENT=<id>` on every command so they don't send as each other.
- `roost whoami` shows the channel, its devices and who is listening; `roost doctor` checks everything and prints one JSON line.

<!-- roost-skill-end -->
