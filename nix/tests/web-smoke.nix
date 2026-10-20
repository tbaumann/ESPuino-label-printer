{
  pkgs,
  server,
  web,
  webNodeModules,
}:
let
  # The suite mounts the *built* bundle in jsdom and reads three things
  # relative to its own location: `server/static` (the bundle), `scripts/` (the
  # PNG) and `node_modules` (jsdom, resolved by walking up from the test file).
  # Lay that tree out so the real script runs unmodified.
  tree = pkgs.runCommand "card-printer-web-smoke-tree" { } ''
    mkdir -p $out/web/test $out/server/static $out/scripts
    cp ${../../web/test/smoke.mjs} $out/web/test/smoke.mjs
    cp -r ${web}/. $out/server/static/
    cp ${../../scripts/test-card.png} $out/scripts/test-card.png
    cp -r ${webNodeModules} $out/node_modules
    chmod -R u+w $out
  '';
in
pkgs.testers.nixosTest {
  name = "card-printer-web-smoke";

  nodes.machine = {pkgs, ...}: {
    virtualisation.memorySize = 2048;

    environment.systemPackages = [pkgs.nodejs];

    # The suite's payment scenario needs a Stripe to talk to; it skips itself
    # otherwise, which would let the check pass without covering checkout.
    systemd.services.mock-stripe = {
      wantedBy = ["multi-user.target"];
      serviceConfig.ExecStart = "${pkgs.python3}/bin/python3 ${../../scripts/mock_stripe.py} 12111";
    };

    systemd.services.card-printer = {
      wantedBy = ["multi-user.target"];
      after = ["mock-stripe.service"];
      # Small rasters: every card write re-renders synchronously, and the
      # 2032x1296 default turns the suite into a timing lottery.
      environment = {
        DATA_DIR = "/var/lib/card-printer";
        BIND_ADDR = "0.0.0.0:8099";
        ADMIN_TOKEN = "secret";
        PRINT_RASTER_WIDTH = "508";
        PRINT_RASTER_HEIGHT = "324";
        STRIPE_SECRET_KEY = "sk_test_x";
        STRIPE_WEBHOOK_SECRET = "whsec_test_secret";
        STRIPE_API_BASE = "http://127.0.0.1:12111";
        STRIPE_SHIPPING_CENTS = "250";
        STRIPE_CURRENCY = "eur";
        PUBLIC_BASE_URL = "http://127.0.0.1:8099";
      };

      serviceConfig = {
        ExecStart = "${server}/bin/card-printer-server";
        StateDirectory = "card-printer";
      };
    };
  };

  testScript = ''
    start_all()
    machine.wait_for_unit("mock-stripe.service")
    machine.wait_for_unit("card-printer.service")
    machine.wait_for_open_port(8099)
    print(machine.succeed("ADMIN_TOKEN=secret node ${tree}/web/test/smoke.mjs http://127.0.0.1:8099"))
  '';
}
