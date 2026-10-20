{
  description = "ESPuino label printer: Axum + utoipa + SQLite API with a Svelte SPA";

  inputs = {
    flake-parts.url = "github:hercules-ci/flake-parts";
    make-shell.url = "github:nicknovitski/make-shell";
    nix-oci = {
      url = "github:Dauliac/nix-oci";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.flake-parts.follows = "flake-parts";
    };
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-flake.url = "github:juspay/rust-flake";
  };

  outputs = inputs @ {flake-parts, ...}:
    flake-parts.lib.mkFlake {inherit inputs;} {
      imports = [
        inputs.rust-flake.flakeModules.default
        inputs.rust-flake.flakeModules.nixpkgs
        inputs.make-shell.flakeModules.default
        inputs.nix-oci.modules.flake.nix-oci
        ./packages.nix
        ./oci.nix
        ./checks.nix
      ];
      systems = ["x86_64-linux" "aarch64-linux" "aarch64-darwin"];
      perSystem = {
        config,
        lib,
        self',
        pkgs,
        ...
      }: {
        rust-project.toolchain = pkgs.rust-bin.stable.latest.default;
        rust-project.src = lib.cleanSource ./.;
        # The crate binary is never exposed on its own: packages.nix owns
        # `packages.card-printer-server` and bundles the SPA into it. Keep the
        # docs package and the clippy check autowired.
        rust-project.crates.card-printer-server.autoWire = ["doc" "clippy"];
        rust-project.crates.card-printer-server.crane.args = {
          buildInputs = with pkgs; [
            openssl
            pkg-config
            sqlite
          ];
          nativeBuildInputs = with pkgs; [pkg-config];
        };
        make-shells.default.inputsFrom = [
          config.rust-project.crates.card-printer-server.crane.outputs.drv.crate
          self'.packages.card-printer-cli
        ];
        make-shells.default.packages = [self'.packages.card-printer];
      };
    };
}
