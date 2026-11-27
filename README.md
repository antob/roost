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

The dev shell comes from `flake.nix` (via direnv). The wire protocol is documented in [docs/PROTOCOL.md](docs/PROTOCOL.md), and [AGENTS.md](AGENTS.md) has the conventions for working on the code.

### Tests

Most changes can be checked without a phone:

- `cargo test` runs the agent CLI against a live server (`tests/cli.rs`), plus Web Push against a fake push service and passkeys with a software authenticator (`tests/app.rs`).
- `cargo clippy` and `cd web && bun run typecheck`.
- `nix flake check` builds the package and boots a NixOS VM with the module.

### A dev server for the phone

To try a change on the iPhone, run a second server from the checkout next to your real one. It keeps its own database, so it has its own passkey, channels and push subscription, and nothing you do there touches the real install.

```sh
cd web && bun install && bun run build && cd ..
cargo build
target/debug/roost --db roost-dev.db serve --bind 0.0.0.0:8787 --base-url https://roost-dev.example.net --instance dev
```

`--instance dev` names the app "roost dev", gives it an orange icon and puts a DEV tag beside the page titles, so it can't be mistaken for your real roost on the home screen or in a notification.

The phone needs HTTPS for passkeys and Web Push, so put the dev server behind a TLS reverse proxy (or tunnel) at the base URL. Then, once:

1. `target/debug/roost --db roost-dev.db passkey enroll --base-url https://roost-dev.example.net`, open the link in Safari on the iPhone, create the passkey and add the page to the home screen. It's a separate app from your real roost, since it's a different site. iOS keeps the name and icon from when the app was added, so add it after starting the server with `--instance`.
2. In the dev app: Settings → Turn on notifications.
3. `target/debug/roost --db roost-dev.db channel create Dev` prints a claim code (or use Channels → + in the app).

After that:

- Rust changes: `cargo build` and restart the server.
- PWA changes: `cd web && bun run build`, then reload the app. Debug builds read `web/dist` from disk, so the server needn't restart.

### Pointing an agent at the dev build

The CLI keeps its keys under `$XDG_CONFIG_HOME/roost` (default `~/.config/roost`), and the installed Claude Code hook and Pi extension call `roost` from `PATH`. So a shell with the dev build first on `PATH` and its own config directory talks to the dev server, with the dev binary, and leaves your real keys alone:

```sh
mkdir -p ~/roost-try && cd ~/roost-try
export PATH=~/path/to/roost/target/debug:$PATH XDG_CONFIG_HOME=$PWD/.roost
roost setup --claim <code> --url https://roost-dev.example.net
roost hello
pi   # or claude: a global hook or extension now runs the dev build too
```

Keep the `PATH` prefix when you install: `roost hook install` writes `roost` by name when one is on `PATH`, so without it a project install would run your installed release instead of the dev build.

If you change the Pi extension itself (`src/client/pi/roost.ts`), a global install still loads the old copy. Start Pi with an empty agent directory and install the extension into the project:

```sh
export PI_CODING_AGENT_DIR=$PWD/.pi-agent
roost hook install --target pi
pi
```

### Trying an unpushed build on a NixOS server

The flake input can be overridden with the checkout for a single rebuild; the next normal rebuild goes back to the locked revision:

```sh
nixos-rebuild switch --flake .#<host> --override-input roost path:/path/to/roost
```

## Acknowledgements

roost's agent CLI and protocol started out compatible with [Pidge](https://pidge.sh) and its MIT-licensed [pidge-cli](https://github.com/pidge-sh/pidge-cli), by Thiago Correa. Some of the CLI's help text and agent instructions still derive from it; see [THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES).

## License

MIT, see [LICENSE](LICENSE).
