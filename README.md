# roost

A self-hosted way for your coding agents to reach you on your iPhone, and for you to answer them. One Rust binary is the server, the web app and the agent CLI.

Agents send notifications from the command line. They arrive as Web Push notifications on the phone, where the roost app (a PWA added to the home screen) shows them with their buttons. Your answer, or anything you write in the conversation, goes back to the agent.

```sh
roost important --title "Deploy finished" --body "All checks green"
roost ask --title "Run the migration?" --body "Touches 2M rows" --actions yes,no   # waits, prints the answer as JSON
roost approve "Delete the staging database?"   # exit 0 only if you approve with Face ID
```

## Features

- Typed sends: `message`, `important`, `urgent` (re-alerts once if unanswered) and `event` (with a "starting soon" reminder).
- Questions with buttons, free-text replies and snooze. Approvals can require Face ID, through a passkey.
- Scheduled sends (`--deliver-at`), queued questions (`--after`) so you get one at a time, and collapse keys so progress updates replace each other.
- `roost listen` / `roost online` for agents that stay reachable, with `--exec` to run a handler per message batch.
- Per-channel settings in the app: name, colour, icon, and how loud a channel may be.
- Optional signed webhooks for each answer.
- Works with [Claude Code](https://code.claude.com) and [Pi](https://pi.dev): `roost setup` installs a skill and a session hook (Claude Code) or extension (Pi) that keeps the agent reachable for the whole session, per project or for all of them (`--global`). Other agents get the same instructions through `AGENTS.md`.

## Running it

roost is packaged with Nix. The flake exports the binary and a NixOS module (`services.roost`); see [docs/DEPLOY.md](docs/DEPLOY.md) for the options and the first-run steps. The server is meant to sit behind a TLS reverse proxy, reachable from your own network (for example over WireGuard).

On each agent machine, install the binary, then claim a channel with the code the app shows:

```sh
roost setup --claim <code> --url https://roost.example.net
roost hello
```

## Development

The dev shell comes from `flake.nix` (via direnv). `cargo test` runs the agent CLI against a live server, plus Web Push and passkey tests; `nix flake check` also boots a NixOS VM with the module. The wire protocol is documented in [docs/PROTOCOL.md](docs/PROTOCOL.md), and [AGENTS.md](AGENTS.md) has the conventions for working on the code.

## Acknowledgements

roost's agent CLI and protocol started out compatible with [Pidge](https://pidge.sh) and its MIT-licensed [pidge-cli](https://github.com/pidge-sh/pidge-cli), by Thiago Correa. Some of the CLI's help text and agent instructions still derive from it; see [THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES).

## License

MIT, see [LICENSE](LICENSE).
