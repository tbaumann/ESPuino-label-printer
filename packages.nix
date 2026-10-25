{...}: {
  perSystem = {
    config,
    pkgs,
    self',
    ...
  }: let
    web = pkgs.callPackage ./nix/packages/card-printer-server-web.nix {};
    evolisSdk = pkgs.callPackage ./nix/packages/evolis-sdk.nix {};
    cardPrinter = pkgs.callPackage ./nix/packages/card-printer.nix {inherit evolisSdk;};
    markie = pkgs.callPackage ./nix/packages/markie.nix {};
    # The server always ships with the SPA baked in: the crate binary wrapped so
    # STATIC_DIR points at the store-built SPA. flake.nix drops "crate" from this
    # crate's autoWire, so this derivation can own the `card-printer-server` name.
    server = pkgs.callPackage ./nix/packages/card-printer-server.nix {
      server = config.rust-project.crates.card-printer-server.crane.outputs.drv.crate;
      inherit web markie;
    };
  in {
    # nix-lib (pulled in by the nix-oci module) otherwise exports a docs package
    # for the `nix-lib.lib.*` functions nix-oci declares. We have no use for it.
    nix-lib.docs.enableOutput = false;

    packages.card-printer-server = server;
    packages.card-printer = cardPrinter;
    packages.markie = markie;
    packages.default = cardPrinter;

    apps.card-printer-server.program = "${server}/bin/card-printer-server";
    apps.card-printer-cli.program = "${self'.packages.card-printer-cli}/bin/card-printer-cli";
    apps.card-printer.program = "${cardPrinter}/bin/card-printer";
    apps.markie.program = "${markie}/bin/markie";
    apps.default = self'.apps.card-printer;
  };
}
