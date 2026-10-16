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
        dependencies = [ pkgs.cacert ];
        environment = {
          DATA_DIR = "/data";
          BIND_ADDR = "0.0.0.0:8080";
          SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
        };
        declaredVolumes = [ "/data" ];
        # No healthcheck: nix2container drops Healthcheck from the image config
        # (upstream bug #197), so it would only cost image size here. nix-oci's
        # deploy modules re-inject it as `podman --health-cmd` at runtime.
        cve.trivy.enabled = true; # or grype, vulnix
        sbom.syft.enabled = true;
        credentialsLeak.trivy.enabled = true;
        test.containerStructureTest.enabled = true; # or dive, dgoss
      };
    };
}
