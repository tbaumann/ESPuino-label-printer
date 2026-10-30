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

      # The NixOS deployment module (see "Deploy (NixOS)" in the README). It is
      # handed the flake's own outputs so its `package` option can default to
      # this flake's SPA-baked server for whatever system the consuming host runs.
      flake.nixosModules.card-printer-server =
        import ./nix/modules/card-printer-server.nix {self = inputs.self;};

      perSystem = {
        config,
        lib,
        self',
        pkgs,
        ...
      }: {
        rust-project.toolchain = pkgs.rust-bin.stable.latest.default;
        # Only the Rust workspace is a build input: the manifests and the two
        # crate dirs. server/ carries the migrations, which sqlx::migrate!
        # embeds at compile time. Editing the SPA, a Nix file or a doc no
        # longer moves the crates' derivations.
        rust-project.src = lib.fileset.toSource {
          root = ./.;
          fileset = lib.fileset.unions [
            ./Cargo.toml
            ./Cargo.lock
            ./cli
            ./server
          ];
        };
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
        make-shells.default.packages = [
          self'.packages.card-printer
          # The markdown backend the server package wraps. In the shell it is
          # on PATH, which is where MARKIE_BIN's default looks for it.
          self'.packages.markie
        ];
      };
    };
}
