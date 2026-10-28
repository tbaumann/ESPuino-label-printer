{ ... }: {
  # Turn on the nix-oci flake outputs: `packages.oci-card-printer-server` plus
  # the `oci-load-docker-`/`oci-load-podman-`/`oci-push-`/`oci-sandbox-` apps.
  oci.enabled = true;
  oci.enableFlakeOutputs = true;

  perSystem =
    {
      pkgs,
      self',
      ...
    }:
    let
      # packages.card-printer-server is the crate binary with the SPA baked in.
      server = self'.packages.card-printer-server;

      # A `markdown` back is laid out by markie, which renders with the fonts it
      # finds and refuses to start when it finds none. Its only reader is
      # fontdb's pure-Rust fontconfig parser (nothing here links libfontconfig),
      # so this one file is the whole runtime contract: /etc/fonts/fonts.conf is
      # where it looks when FONTCONFIG_FILE is unset, and the image root is a
      # buildEnv that links each dependency's /etc. The <dir> store path is a
      # reference of this derivation, so the font rides into the image with it.
      #
      # The aliases pin the generic families markie asks for to faces that
      # exist: fontdb takes only the FIRST <prefer> of an <alias>, and the first
      # one in fontconfig's own conf.d names a family this image does not carry.
      fontsConf = pkgs.writeTextDir "etc/fonts/fonts.conf" ''
        <?xml version="1.0"?>
        <!DOCTYPE fontconfig SYSTEM "urn:fontconfig:fonts.dtd">
        <fontconfig>
          <dir>${pkgs.dejavu_fonts}/share/fonts</dir>
          <alias>
            <family>sans-serif</family>
            <prefer><family>DejaVu Sans</family></prefer>
          </alias>
          <alias>
            <family>serif</family>
            <prefer><family>DejaVu Serif</family></prefer>
          </alias>
          <alias>
            <family>monospace</family>
            <prefer><family>DejaVu Sans Mono</family></prefer>
          </alias>
        </fontconfig>
      '';
    in
    {
      oci.containers.card-printer-server = {
        package = server;
        # The wrapper is a symlinkJoin with no meta.mainProgram, so nix-oci cannot
        # derive the command — name it.
        entrypoint = [ "${server}/bin/card-printer-server" ];
        registry = "ghcr.io/tbaumann";
        name = "card-printer-server";
        # Off by default in nix-oci; this is what creates
        # `nix run .#oci-push-card-printer-server`.
        push = true;
        # Non-root (nix-oci's default): runs as UID/GID 4000 and gets
        # /etc/passwd + a home directory. /data below must be a writable mount.
        user = "card-printer";
        ports = [ "8080:8080" ];
        # cacert: the server verifies TLS when it talks to Stripe.
        # fontsConf: /etc/fonts/fonts.conf + the font it names, for `markdown`.
        dependencies = [
          pkgs.cacert
          fontsConf
        ];
        environment = {
          DATA_DIR = "/data";
          BIND_ADDR = "0.0.0.0:8080";
          SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
        };
        declaredVolumes = [ "/data" ];
        # No healthcheck: nix2container drops Healthcheck from the image config
        # (upstream bug #197), so it would only cost image size here. nix-oci's
        # deploy modules re-inject it as `podman --health-cmd` at runtime.
        cve.trivy.enabled = false; # or grype, vulnix
        sbom.syft.enabled = true;
        credentialsLeak.trivy.enabled = true;
        test.containerStructureTest.enabled = true; # or dive, dgoss
      };
    };
}
