{
  lib,
  buildNpmPackage,
}:
buildNpmPackage {
  pname = "card-printer-server-web";
  version = "0.1.0";
  # Only files the build actually needs are copied in: no node_modules, no
  # dist, no tests. Nothing is read from a build output checked into the tree.
  src = lib.fileset.toSource {
    root = ../../web;
    fileset = lib.fileset.unions [
      ../../web/index.html
      ../../web/package.json
      ../../web/package-lock.json
      ../../web/svelte.config.js
      ../../web/tsconfig.json
      ../../web/tsconfig.app.json
      ../../web/tsconfig.node.json
      ../../web/vite.config.ts
      ../../web/public
      ../../web/src
    ];
  };
  npmDepsHash = "sha256-m7rIhYggK2Lp9kTGAxY+1to04nbgckNf69CvUumF0kI=";
  # vite would otherwise write to ../server/static, i.e. outside the source
  # tree. Keep the output inside and install it from there.
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
}
