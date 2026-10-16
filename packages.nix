{...}: {
  perSystem = {
    config,
    pkgs,
    lib,
    self',
    ...
  }: let
    web = pkgs.callPackage ./nix/packages/card-printer-server-web.nix {};
    evolisSdk = pkgs.callPackage ./nix/packages/evolis-sdk.nix {};
    cardPrinter = pkgs.callPackage ./nix/packages/card-printer.nix {inherit evolisSdk;};
    # The server always ships with the SPA baked in: the crate binary wrapped so
    # STATIC_DIR points at the store-built SPA. flake.nix drops "crate" from this
    # crate's autoWire, so this derivation can own the `card-printer-server` name.
    server = pkgs.callPackage ./nix/packages/card-printer-server.nix {
      server = config.rust-project.crates.card-printer-server.crane.outputs.drv.crate;
      inherit web;
    };
  in {
    packages =
      {
        card-printer-server = server;
        card-printer = cardPrinter;
        default = cardPrinter;
      }
      // lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
        card-printer-server-image = pkgs.callPackage ./nix/packages/card-printer-server-image.nix {
          inherit server;
        };
      };

    apps.card-printer-server.program = "${server}/bin/card-printer-server";
    apps.card-printer-cli.program = "${self'.packages.card-printer-cli}/bin/card-printer-cli";
    apps.card-printer.program = "${cardPrinter}/bin/card-printer";
    apps.default = self'.apps.card-printer;
  };
}
