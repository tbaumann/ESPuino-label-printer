{
  description = "ESPuino label printer: Axum + utoipa + SQLite API with a Svelte SPA";

  inputs = {
    flake-parts.url = "github:hercules-ci/flake-parts";
    make-shell.url = "github:nicknovitski/make-shell";
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-flake.url = "github:juspay/rust-flake";
  };

  outputs = inputs @ {flake-parts, ...}:
    flake-parts.lib.mkFlake {inherit inputs;} {
      imports = [
        inputs.rust-flake.flakeModules.default
        inputs.rust-flake.flakeModules.nixpkgs
        inputs.make-shell.flakeModules.default
        ./packages.nix
      ];
      systems = ["x86_64-linux" "aarch64-linux" "aarch64-darwin"];
      perSystem = {
        lib,
        self',
        pkgs,
        ...
      }: {
        rust-project.toolchain = pkgs.rust-bin.stable.latest.default;
        rust-project.src = lib.cleanSource ./.;
        rust-project.crates.card-printer-server.crane.args = {
          buildInputs = with pkgs; [
            openssl
            pkg-config
            sqlite
          ];
          nativeBuildInputs = with pkgs; [pkg-config];
        };
        make-shells.default.inputsFrom = [
          self'.packages.card-printer-server
          self'.packages.card-printer-cli
        ];
        make-shells.default.packages = [self'.packages.card-printer];
      };
    };
}
