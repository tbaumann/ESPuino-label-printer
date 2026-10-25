{
  lib,
  symlinkJoin,
  makeWrapper,
  server,
  web,
  markie,
}:
# The server with the store-built SPA baked in and the markdown backend pointed
# at: STATIC_DIR and MARKIE_BIN both name a store path, so this package depends
# on nothing in the working tree. `server` is the plain crate binary
# (rust-flake's `packages.card-printer-server`), `web` is the built SPA, and
# `markie` is what lays a `markdown` back's text out into a picture.
symlinkJoin {
  name = "card-printer-server";
  paths = [server];
  nativeBuildInputs = [makeWrapper];
  postBuild = ''
    wrapProgram $out/bin/card-printer-server \
      --set-default STATIC_DIR ${web} \
      --set-default MARKIE_BIN ${lib.getExe markie}
  '';
}
