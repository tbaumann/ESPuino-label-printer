{...}: {
  perSystem = {
    pkgs,
    lib,
    self',
    ...
  }: let
    web = pkgs.callPackage ./nix/packages/card-printer-server-web.nix {};
    evolisSdk = pkgs.callPackage ./nix/packages/evolis-sdk.nix {};
    cardPrinter = pkgs.callPackage ./nix/packages/card-printer.nix {inherit evolisSdk;};
    # Not exposed as a package attr: the name `card-printer-server` belongs to
    # rust-flake's plain crate binary. This wrapper (SPA baked in) is what the
    # `card-printer-server` app runs.
    serverWithWeb = pkgs.callPackage ./nix/packages/card-printer-server.nix {
      server = self'.packages.card-printer-server;
      inherit web;
    };
  in {
    packages =
      {
        card-printer-server-web = web;
        card-printer = cardPrinter;
        default = cardPrinter;
      }
      // lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
        card-printer-server-image = pkgs.callPackage ./nix/packages/card-printer-server-image.nix {
          server = serverWithWeb;
        };
      };

    apps.card-printer-server.program = "${serverWithWeb}/bin/card-printer-server";
    apps.card-printer-cli.program = "${self'.packages.card-printer-cli}/bin/card-printer-cli";
    apps.card-printer.program = "${cardPrinter}/bin/card-printer";
    apps.default = self'.apps.card-printer;
  };
}
