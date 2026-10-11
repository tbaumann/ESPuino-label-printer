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
      ];
      systems = ["x86_64-linux" "aarch64-linux" "aarch64-darwin"];
      perSystem = {
        config,
        lib,
        self',
        inputs',
        pkgs,
        system,
        ...
      }: let
        # The SPA is built from source in the store. Only files the build
        # actually needs are copied in: no node_modules, no dist, no tests.
        # Nothing is read from a build output checked into the working tree.
        webSrc = lib.fileset.toSource {
          root = ./web;
          fileset = lib.fileset.unions [
            ./web/index.html
            ./web/package.json
            ./web/package-lock.json
            ./web/svelte.config.js
            ./web/tsconfig.json
            ./web/tsconfig.app.json
            ./web/tsconfig.node.json
            ./web/vite.config.ts
            ./web/public
            ./web/src
          ];
        };

        webStatic = pkgs.buildNpmPackage {
          pname = "espuino-label-printer-web";
          version = "0.1.0";
          src = webSrc;
          npmDepsHash = "sha256-RiV949pC4zPKtzPXbGm1X+oJYpcXYqby2GXU+B659w8=";
          # vite would otherwise write to ../server/static, i.e. outside the
          # source tree. Keep the output inside and install it from there.
          buildPhase = ''
            runHook preBuild
            ESPUINO_WEB_OUT=dist npm run build
            runHook postBuild
          '';
          installPhase = ''
            runHook preInstall
            mkdir -p $out
            cp -r dist/. $out/
            runHook postInstall
          '';
          meta = {
            description = "Svelte SPA for the ESPuino label printer";
            platforms = lib.platforms.unix;
          };
        };

        # The server with the store-built SPA baked in: STATIC_DIR points into
        # the store, so this package depends on nothing in the working tree.
        serverWithWeb = pkgs.symlinkJoin {
          name = "espuino-label-printer";
          paths = [self'.packages.espuino-label-printer];
          nativeBuildInputs = [pkgs.makeWrapper];
          postBuild = ''
            wrapProgram $out/bin/espuino-label-printer \
              --set-default STATIC_DIR ${webStatic}
          '';
        };
      in {
        rust-project.toolchain = pkgs.rust-bin.stable.latest.default;
        rust-project.src = lib.cleanSource ./.;
        rust-project.crates.espuino-label-printer.crane.args = {
          buildInputs = with pkgs; [
            openssl
            pkg-config
            sqlite
          ];
          nativeBuildInputs = with pkgs; [pkg-config];
        };
        make-shells.default.inputsFrom = [
          self'.packages.espuino-label-printer
          self'.packages.esplp-cli
        ];
        packages.espuino-label-printer-web = webStatic;
        packages.default = serverWithWeb;

        apps.espuino-label-printer.program = "${serverWithWeb}/bin/espuino-label-printer";
        apps.esplp.program = "${self'.packages.esplp-cli}/bin/esplp";
        apps.default = self'.apps.espuino-label-printer;
      };
      flake = {
      };
    };
}
