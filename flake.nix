{
  description = "roost — self-hosted two-way agent ↔ iPhone messenger: the server, the PWA and the agent CLI in one binary";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs, ... }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      pkgsFor = system: nixpkgs.legacyPackages.${system};
    in
    {
      packages = forAllSystems (system: {
        default = (pkgsFor system).callPackage ./nix/package.nix { };
      });

      nixosModules.default = import ./nix/module.nix self;

      checks = forAllSystems (system: {
        package = self.packages.${system}.default;
        module = import ./nix/test.nix {
          inherit self;
          pkgs = pkgsFor system;
        };
      });

      devShells = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              rustc
              rust-analyzer
              clippy
              rustfmt
              sqlx-cli
              sqlite
              bun
              github-cli
              pkg-config
              openssl
            ];
            shellHook = ''
              export PRJ_ROOT="$PWD"
              export DATABASE_URL="sqlite://$PRJ_ROOT/target/sqlx-dev.db"
            '';
          };
        }
      );
    };
}
