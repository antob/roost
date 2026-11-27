# Deploying roost

The flake exports `packages.<system>.default` (the `roost` binary with the PWA embedded) and `nixosModules.default` (`services.roost`). `nix flake check` builds the package, which runs the whole test suite in the sandbox, and boots a NixOS VM that exercises the module (`nix/test.nix`).

## The module

```nix
{
  inputs.roost = {
    url = "github:antob/roost";
    inputs.nixpkgs.follows = "nixpkgs";
  };

  # in a NixOS configuration:
  imports = [ inputs.roost.nixosModules.default ];
  services.roost = {
    enable = true;
    baseUrl = "https://roost.example.net";
    dataDir = "/var/lib/roost";
    vapidKeyFile = config.sops.secrets.roost_vapid_key.path;
    environmentFile = config.sops.secrets.roost_env.path;
  };
}
```

| Option | Default | Notes |
|---|---|---|
| `baseUrl` | (required) | The public HTTPS URL. Its host is the passkey RP ID: passkeys enrolled under one host don't work under another, so pick it once. |
| `bind` | `127.0.0.1:8787` | Put a TLS reverse proxy in front. |
| `dataDir` | `/var/lib/roost` | Holds `roost.db` (SQLite, WAL). Created `0750` for the service user. |
| `user`, `group` | `roost` | Created by the module when left at `roost`. |
| `timezone` | `time.timeZone`, else UTC | Reported to agents in `whoami`. |
| `vapidKeyFile` | null | A P-256 private key in PEM (`openssl ecparam -genkey -name prime256v1 -noout`), passed as a systemd credential, so a root-owned secret file is fine. Without it a key is generated on first start and kept in the database. A different key invalidates every push subscription. |
| `vapidSubject` | the base URL | The VAPID `sub` claim (`mailto:` or `https:`). |
| `environmentFile` | null | Secrets as env lines: `ROOST_CALLBACK_SECRET=…` (the `reply_to` webhook HMAC key). Never set `ROOST_ADMIN_TOKEN` in production: it skips the Face ID gate. |
| `backup.enable`, `backup.startAt` | off, 03:15 | A daily `sqlite3 .backup` copy to `<dataDir>/backup/roost.db`, for file-level backups. Not needed when backups snapshot the filesystem (ZFS), which is already consistent for SQLite. |

The service is hardened (`ProtectSystem=strict`, no capabilities, `@system-service` syscalls) and needs only outbound HTTPS, for Web Push and webhooks, plus the system CA bundle.

The module also installs `roost-admin`, which runs `roost` as the service user against the service's database. Run it as root:

- `roost-admin passkey enroll`: a one-time link (15 min) that enrolls a passkey. Open it on the iPhone.
- `roost-admin passkey list` · `roost-admin passkey remove <id>`
- `roost-admin channel create <name>` · `roost-admin channel claim-code <id>` · `roost-admin channel delete <id>`

## First run

1. Deploy, and check `curl https://<host>/up` answers `ok`.
2. On the iPhone, run `roost-admin passkey enroll` on the server and open the link in Safari. Create the passkey, then Share → Add to Home Screen, and open roost from the home screen.
3. In the app: Settings → Turn on notifications → Send test push.
4. Channels → + gives a claim code. On each agent machine: `roost setup --claim <code> --url https://<host>`, then `roost hello`.

## Agent machines

Agents need only the binary: add `inputs.roost.packages.${system}.default` to the user's packages. The CLI's config lives in `~/.config/roost`.

`roost setup` installs the agent integration into the project it runs in. To have it in every project of a user instead:

- Claude Code: `roost skill install --global` and `roost hook install --global` (into `$CLAUDE_CONFIG_DIR`, default `~/.claude`).
- Pi: `roost skill install --target pi --global` and `roost hook install --target pi --global` (into `$PI_CODING_AGENT_DIR`, default `~/.pi/agent`). A global Pi extension needs no project trust.

Global hooks run `roost presence --if-configured`, which stays silent in projects without a roost key. They leave `ROOST_AGENT` out, so each project keeps its own identity. Install either globally or per project, not both: roost warns when it finds the other.
