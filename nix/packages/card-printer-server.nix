{
  symlinkJoin,
  makeWrapper,
  server,
  web,
}:
# The server with the store-built SPA baked in: STATIC_DIR points into the
# store, so this package depends on nothing in the working tree. `server` is the
# plain crate binary (rust-flake's `packages.card-printer-server`); `web` is the
# built SPA.
symlinkJoin {
  name = "card-printer-server";
  paths = [server];
  nativeBuildInputs = [makeWrapper];
  postBuild = ''
    wrapProgram $out/bin/card-printer-server \
      --set-default STATIC_DIR ${web}
  '';
}
