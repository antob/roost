{
  lib,
  rustPlatform,
  pkg-config,
  openssl,
  aws-lc,
  sqlite,
  bun,
  cacert,
}:

rustPlatform.buildRustPackage {
  pname = "roost";
  version = (lib.importTOML ../Cargo.toml).package.version;

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../LICENSE
      ../THIRD_PARTY_NOTICES
      ../.sqlx
      ../src
      ../migrations
      ../tests
      ../web/src
      ../web/public
      ../web/package.json
      ../web/tsconfig.json
    ];
  };

  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [
    pkg-config
    bun
  ];
  nativeCheckInputs = [ cacert ];
  buildInputs = [
    openssl
    aws-lc
    sqlite
  ];

  env.SQLX_OFFLINE = "true";

  # The release binary embeds web/dist, so the PWA is built first.
  preBuild = ''
    (cd web && HOME=$TMPDIR bun run build)
  '';

  postInstall = ''
    install -Dm644 -t $out/share/doc/roost LICENSE THIRD_PARTY_NOTICES
  '';

  cargoBuildFlags = [ "--package" "roost" ];
  cargoTestFlags = [ "--package" "roost" ];

  meta = {
    description = "Self-hosted two-way agent ↔ iPhone messenger: server, PWA and agent CLI in one binary";
    homepage = "https://github.com/antob/roost";
    license = lib.licenses.mit;
    mainProgram = "roost";
    platforms = lib.platforms.linux;
  };
}
