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
          npmDepsHash = "sha256-m7rIhYggK2Lp9kTGAxY+1to04nbgckNf69CvUumF0kI=";
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

        # The Evolis SDK ships as a fully self-contained wheel (no Python deps):
        # the per-platform native lib is inside the wheel and loaded via ctypes.
        # Format "wheel" avoids a pointless source build.
        #
        # The ctypes .so has its own dynamic deps (libudev.so.1, libstdc++.so.6)
        # that nothing on the plain Nix store RPath provides. Simply propagating
        # systemd/gcc as propagatedBuildInputs does NOT help: buildPythonPackage
        # only folds *Python modules* into PYTHONPATH, and a ctypes-loaded .so
        # gets no RPath and no wrapper LD_LIBRARY_PATH from a transitive native
        # dep. The fix is an RPath (patchelf) on the .so itself, so it needs no
        # LD_LIBRARY_PATH anywhere — runnable from any environment.
        sdkLibs = lib.makeLibraryPath [
          pkgs.systemd           # libudev.so.1
          pkgs.gcc-unwrapped.lib # libstdc++.so.6
        ];
        evolisSdk = pkgs.python3.pkgs.buildPythonPackage {
          pname = "evolis-sdk";
          version = "9.4.1";
          src = pkgs.fetchurl {
            url = "https://files.pythonhosted.org/packages/19/02/b4eca9226cdd1ca1b89f0d9c674e0856ab5d411f5ecf251f4c9c63d79bb0/evolis_sdk-9.4.1-py3-none-any.whl";
            sha256 = "sha256-kii6yXTdGWCb2i6SPgqbEv+P3+0G6kXzTA7ujKCZnw4=";
          };
          format = "wheel";
          # Keep the native libs as buildInputs so they are in the build env's
          # store at fixup time AND as a convenience for tooling that inspects
          # the derivation. The RPath below is what actually makes them load.
          nativeBuildInputs = [pkgs.autoPatchelfHook];
          buildInputs = [
            pkgs.systemd
            pkgs.gcc-unwrapped.lib
          ];
          # Patch every bundled libevolis.so (one per platform is shipped;
          # patching only the one for this system is enough, autopatchelf
          # handles whichever it finds) with the RPath to its runtime deps.
          # autoPatchelfHook also catches the wheel if its .so needs anything
          # beyond the two known libs.
          # The wheel carries no tests; skip the import smoke test too, because
          # the ctypes lib probes hardware state that a sandboxed build cannot.
          doCheck = false;
          dontUsePythonImportsCheck = true;
          preFixup = ''
            find "$out" -name '*.so' -print -exec \
              ${pkgs.patchelf}/bin/patchelf --set-rpath '${sdkLibs}' {} \;
          '';
          meta = {
            description = "Evolis card printer SDK (self-contained wheel)";
            platforms = lib.platforms.linux;
          };
        };

        # The local print spooler: our Python package, with the Evolis SDK as a
        # required dependency (it is not optional — talking to the printer is
        # the spooler's whole job). A buildPythonApplication productionises the
        # console entrypoint into a wrapper that puts the SDK's propagated
        # native libs (libudev, libstdc++) on LD_LIBRARY_PATH automatically, so
        # the runnable `esplp-print` needs nothing extra.
        esplpPrint = pkgs.python3.pkgs.buildPythonApplication {
          pname = "esplp-print";
          version = "0.1.0";
          src = lib.fileset.toSource {
            root = ./printer;
            fileset = ./printer;
          };
          format = "pyproject";
          propagatedBuildInputs = [evolisSdk];
          nativeBuildInputs = [pkgs.python3.pkgs.setuptools pkgs.python3.pkgs.wheel];
          build-system = [pkgs.python3.pkgs.setuptools];
          doCheck = false;
          meta = {
            description = "ESPuino Label Printer spooler (local Evolis card printer)";
            platforms = lib.platforms.linux;
          };
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
        make-shells.default.packages = [esplpPrint];
        packages.espuino-label-printer-web = webStatic;
        packages.esplp-print = esplpPrint;
        packages.default = serverWithWeb;

        apps.espuino-label-printer.program = "${serverWithWeb}/bin/espuino-label-printer";
        apps.esplp.program = "${self'.packages.esplp-cli}/bin/esplp";
        apps.esplp-print.program = "${esplpPrint}/bin/esplp-print";
        apps.default = self'.apps.espuino-label-printer;
      };
      flake = {
      };
    };
}
