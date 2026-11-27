# roost

A self-hosted way for agents to reach their human on the iPhone: one Rust binary that is the server, the PWA and the agent CLI. The wire protocol is in `docs/PROTOCOL.md` (read it before changing server behaviour), and packaging and deployment are in `docs/DEPLOY.md`.

## Layout

- The repo root is the Rust crate (`roost` binary), with `migrations/` and `tests/`.
- `src/server/`: the server. axum + tokio, `sqlx` with SQLite, `web-push`, and `rust-embed` serving `web/dist`.
- `src/client/`: the agent CLI (`roost setup`, `roost ask`, `roost online`, …). The skill it writes for agents is in `src/client/skill/`, and the Pi extension it installs (`roost_watch` tool, `/roost` command) is `src/client/pi/roost.ts`; changing a command means checking both.
- `src/util.rs`: helpers both halves share. `src/main.rs` dispatches agent commands to the client, the rest to the server.
- `web/`: the PWA in TypeScript, built with Bun (`bun run build` → `web/dist`).

## Rules

- `docs/PROTOCOL.md` is the contract between the server and the CLI. Change both sides and the doc together.
- Names are roost's everywhere: `ROOST_*` env vars, `~/.config/roost`, `X-Roost-*` headers, `rst_` keys, `roost.event` webhooks.
- Never commit tokens, claim codes, push endpoints or real notification content.

## Commands

The dev shell comes from `flake.nix` through direnv. Outside it, use `direnv exec . <cmd>`. `.envrc` is gitignored and personal: it may move cargo's and Bun's caches into the repo (`.cargo-home/`, `.bun/`, both gitignored), so don't assume they live in `~`.

- Server: `cargo build`, `cargo test`, `cargo clippy`, `cargo fmt`
- Queries use sqlx's compile-time macros against `$DATABASE_URL` (`target/sqlx-dev.db`, set by the dev shell). After adding a migration: `sqlx migrate run`. After changing any query: `cargo sqlx prepare`, and commit `.sqlx/`.
- `tests/cli.rs` runs the built `roost` CLI against a live server. Any change to agent-facing behaviour needs a case there.
- `tests/app.rs` covers the owner side: Web Push against a fake push service, passkeys with a software authenticator, and the Face ID gate.
- Nix: `nix build` (the package, tests included) and `nix flake check` (also a NixOS VM test of `services.roost`, `nix/test.nix`). The package and module are in `nix/`, documented in `docs/DEPLOY.md`.
- PWA: `cd web && bun install && bun run build`, and `bun run typecheck`.
- Pi extension: Pi loads `src/client/pi/roost.ts` as TypeScript (no build step). Typecheck it with `tsc` against the `node_modules` of an installed Pi, and keep its `// roost-config` line as is: `roost hook install --target pi` replaces it. The binary embeds `web/dist` (read from disk in debug builds, compiled in for release), so build the PWA before a release build. Use Bun, not Node or npm.
