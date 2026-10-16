{
  dockerTools,
  cacert,
  server,
  tag ? "latest",
}:
# A runnable server image: the SPA-baked wrapper plus CA certificates (the server
# talks to Stripe over TLS). Everything else is the server's own environment —
# see the README's "Configuration (server environment)"; set ADMIN_TOKEN and the
# Stripe variables at run time. Load with `docker load < result`.
dockerTools.buildLayeredImage {
  name = "card-printer-server";
  inherit tag;
  contents = [server cacert];
  config = {
    Cmd = ["${server}/bin/card-printer-server"];
    Env = [
      "SSL_CERT_FILE=${cacert}/etc/ssl/certs/ca-bundle.crt"
      "DATA_DIR=/data"
      "BIND_ADDR=0.0.0.0:8080"
    ];
    ExposedPorts."8080/tcp" = {};
    Volumes."/data" = {};
  };
}
