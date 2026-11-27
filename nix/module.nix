self:
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.roost;
  inherit (lib) mkEnableOption mkOption mkIf types optionalAttrs;
  db = "${cfg.dataDir}/roost.db";
  admin = pkgs.writeShellScriptBin "roost-admin" ''
    exec ${pkgs.util-linux}/bin/runuser -u ${cfg.user} -- env ROOST_DB=${db} ROOST_BASE_URL=${cfg.baseUrl} ${lib.getExe cfg.package} "$@"
  '';
in
{
  options.services.roost = {
    enable = mkEnableOption "roost, the self-hosted agent ↔ iPhone messenger";

    package = mkOption {
      type = types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      description = "The roost package.";
    };

    baseUrl = mkOption {
      type = types.str;
      example = "https://roost.example.net";
      description = "Public HTTPS URL of the server. Its host is the passkey RP ID, so it must be a domain name and must not change once passkeys are enrolled.";
    };

    bind = mkOption {
      type = types.str;
      default = "127.0.0.1:8787";
      description = "Address the HTTP server listens on (put a TLS reverse proxy in front).";
    };

    dataDir = mkOption {
      type = types.path;
      default = "/var/lib/roost";
      description = "Directory holding the SQLite database.";
    };

    user = mkOption {
      type = types.str;
      default = "roost";
      description = "User the service runs as (created by the module).";
    };

    group = mkOption {
      type = types.str;
      default = "roost";
      description = "Group the service runs as (created by the module).";
    };

    timezone = mkOption {
      type = types.str;
      default = if config.time.timeZone != null then config.time.timeZone else "UTC";
      defaultText = lib.literalExpression ''config.time.timeZone, else "UTC"'';
      description = "The owner's timezone, reported to agents in whoami.";
    };

    vapidKeyFile = mkOption {
      type = types.nullOr types.path;
      default = null;
      description = "File with the VAPID private key (PEM, P-256), read through a systemd credential. Without it a key is generated on first start and kept in the database. Changing the key invalidates every push subscription.";
    };

    vapidSubject = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "mailto:owner@example.net";
      description = "VAPID `sub` claim; defaults to the base URL.";
    };

    environmentFile = mkOption {
      type = types.nullOr types.path;
      default = null;
      description = "Environment file for secrets, e.g. ROOST_CALLBACK_SECRET=… (the reply_to webhook HMAC key). Never set ROOST_ADMIN_TOKEN in production: it skips the Face ID gate.";
    };

    backup = {
      enable = mkEnableOption "a daily consistent SQLite copy at <dataDir>/backup/roost.db, for file-level backups to pick up";
      startAt = mkOption {
        type = types.str;
        default = "*-*-* 03:15:00";
        description = "systemd calendar time for the copy; schedule file backups after it.";
      };
    };
  };

  config = mkIf cfg.enable {
    users.users = optionalAttrs (cfg.user == "roost") {
      roost = {
        isSystemUser = true;
        group = cfg.group;
        home = cfg.dataDir;
      };
    };
    users.groups = optionalAttrs (cfg.group == "roost") { roost = { }; };

    systemd.tmpfiles.rules = [
      "d ${cfg.dataDir} 0750 ${cfg.user} ${cfg.group} - -"
    ] ++ lib.optional cfg.backup.enable "d ${cfg.dataDir}/backup 0750 ${cfg.user} ${cfg.group} - -";

    environment.systemPackages = [ admin ];

    systemd.services.roost = {
      description = "roost agent ↔ iPhone messenger";
      wantedBy = [ "multi-user.target" ];
      wants = [ "network-online.target" ];
      after = [ "network-online.target" ];
      environment = {
        ROOST_DB = db;
        ROOST_BIND = cfg.bind;
        ROOST_BASE_URL = cfg.baseUrl;
        ROOST_TIMEZONE = cfg.timezone;
        RUST_LOG = "info,sqlx=warn";
      }
      // optionalAttrs (cfg.vapidKeyFile != null) { ROOST_VAPID_KEY_FILE = "%d/vapid"; }
      // optionalAttrs (cfg.vapidSubject != null) { ROOST_VAPID_SUBJECT = cfg.vapidSubject; };
      serviceConfig = {
        ExecStart = "${lib.getExe cfg.package} serve";
        User = cfg.user;
        Group = cfg.group;
        WorkingDirectory = cfg.dataDir;
        ReadWritePaths = [ cfg.dataDir ];
        EnvironmentFile = lib.mkIf (cfg.environmentFile != null) cfg.environmentFile;
        LoadCredential = lib.mkIf (cfg.vapidKeyFile != null) [ "vapid:${cfg.vapidKeyFile}" ];
        Restart = "on-failure";
        RestartSec = 5;
        UMask = "0077";

        NoNewPrivileges = true;
        PrivateTmp = true;
        PrivateDevices = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectKernelLogs = true;
        ProtectControlGroups = true;
        ProtectClock = true;
        ProtectHostname = true;
        ProtectProc = "invisible";
        ProcSubset = "pid";
        RestrictAddressFamilies = [ "AF_INET" "AF_INET6" "AF_UNIX" ];
        RestrictNamespaces = true;
        RestrictRealtime = true;
        RestrictSUIDSGID = true;
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        SystemCallArchitectures = "native";
        SystemCallFilter = [ "@system-service" "~@privileged" "~@resources" ];
        CapabilityBoundingSet = "";
      };
    };

    systemd.services.roost-backup = mkIf cfg.backup.enable {
      description = "Consistent copy of the roost database";
      startAt = cfg.backup.startAt;
      serviceConfig = {
        Type = "oneshot";
        User = cfg.user;
        Group = cfg.group;
        UMask = "0077";
        ExecStart = "${lib.getExe pkgs.sqlite} ${db} \".backup '${cfg.dataDir}/backup/roost.db'\"";
      };
    };
  };
}
