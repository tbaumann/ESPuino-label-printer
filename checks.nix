{...}: {
  # nix-oci already gates every image build on its scanners (CVE, SBOM,
  # credentials leak, container-structure-test); expose that gate as a check so
  # `nix flake check` runs it too.
  oci.flake.outputs.checks = true;

  perSystem = {
    config,
    lib,
    pkgs,
    self',
    ...
  }: let
    # The workspace's crane args (openssl, sqlite, pkg-config) live with the
    # crate in flake.nix; reuse them rather than restating the inputs here.
    rustArgs = config.rust-project.crates.card-printer-server.crane.args;
    craneLib = config.rust-project.crane-lib;

    workspaceArgs =
      rustArgs
      // {
        pname = "card-printer-workspace";
        src = config.rust-project.src;
        strictDeps = true;
        cargoExtraArgs = "--workspace";
      };

    web = pkgs.callPackage ./nix/packages/card-printer-server-web.nix {};

    # node_modules (jsdom) for the smoke suite, from the same npm deps the SPA
    # package installs.
    webNodeModules = web.overrideAttrs (_: {
      pname = "card-printer-server-web-node-modules";
      buildPhase = ":";
      dontCheck = true;
      installPhase = "cp -r node_modules $out";
    });
  in {
    checks = {
      # `cargo test --workspace`, offline: crane vendors the lock file and the
      # deps-only build is shared with the package builds.
      cargo-test = craneLib.cargoTest (
        workspaceArgs
        // {
          cargoArtifacts = craneLib.buildDepsOnly workspaceArgs;
        }
      );

      # svelte-check + tsc. Same source and npm deps as the SPA package, with
      # only the check phase left in place.
      svelte-check = web.overrideAttrs (_: {
        pname = "card-printer-server-web-svelte-check";
        buildPhase = ":";
        doCheck = true;
        checkPhase = "npm run check";
        installPhase = "mkdir -p $out";
      });

      # The spooler's API layer against its own in-process mock server. No
      # printer and no built spooler needed, just the module tree.
      spooler-api-test = pkgs.runCommand "spooler-api-test" {
        nativeBuildInputs = [pkgs.python3];
      } ''
        export PYTHONPATH=${./printer}
        python3 ${./scripts/spooler_api_test.py}
        touch $out
      '';

      # The two end-to-end suites want a running server and the stand-ins, which
      # is a VM with systemd units rather than a build sandbox — so they are
      # NixOS tests, driven by the same scripts the README documents.
      web-smoke = pkgs.callPackage ./nix/tests/web-smoke.nix {
        inherit web webNodeModules;
        server = self'.packages.card-printer-server;
      };

      stripe-e2e = pkgs.callPackage ./nix/tests/stripe-e2e.nix {
        server = self'.packages.card-printer-server;
      };
    };
  };
}
