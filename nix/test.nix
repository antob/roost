{ self, pkgs }:

let
  vapid = pkgs.runCommand "vapid.pem" { } ''
    ${pkgs.openssl}/bin/openssl ecparam -genkey -name prime256v1 -noout -out $out
  '';
  secrets = pkgs.writeText "roost.env" "ROOST_CALLBACK_SECRET=test-secret\n";
in
pkgs.testers.runNixOSTest {
  name = "roost";
  nodes.machine = {
    imports = [ self.nixosModules.default ];
    services.roost = {
      enable = true;
      baseUrl = "https://roost.test";
      dataDir = "/srv/roost";
      vapidKeyFile = vapid;
      environmentFile = secrets;
      backup.enable = true;
    };
    environment.systemPackages = [ pkgs.curl pkgs.jq ];
  };
  testScript = ''
    import json, re

    machine.wait_for_unit("roost.service")
    machine.wait_for_open_port(8787)
    machine.succeed("curl -sf localhost:8787/up")
    machine.succeed("curl -sf localhost:8787/ | grep -q '<title>'")
    session = json.loads(machine.succeed("curl -sf localhost:8787/api/human/session"))
    assert session["vapid_public_key"], session

    out = machine.succeed("roost-admin channel create Laptop")
    match = re.search(r"claim code (\S+)", out)
    assert match, out
    code = match.group(1)
    claim = json.loads(machine.succeed(
        f"curl -sf -X POST localhost:8787/api/v1/claim -H 'Content-Type: application/json' -d '{{\"code\": \"{code}\"}}'"
    ))
    assert claim["base_url"] == "https://roost.test", claim
    who = json.loads(machine.succeed(
        f"curl -sf localhost:8787/api/v1/whoami -H 'Authorization: Bearer {claim['key']}'"
    ))
    assert who["channel"]["name"] == "Laptop", who

    machine.succeed("roost-admin passkey enroll | grep -q '^https://roost.test/enroll#'")
    machine.succeed("stat -c %U:%a /srv/roost | grep -qx roost:750")

    machine.succeed("systemctl start roost-backup.service")
    machine.succeed("${pkgs.sqlite}/bin/sqlite3 /srv/roost/backup/roost.db 'SELECT name FROM channels' | grep -qx Laptop")

    machine.succeed("systemctl restart roost.service")
    machine.wait_for_open_port(8787)
    after = json.loads(machine.succeed("curl -sf localhost:8787/api/human/session"))
    assert after["vapid_public_key"] == session["vapid_public_key"], "the VAPID key must survive a restart"
  '';
}
