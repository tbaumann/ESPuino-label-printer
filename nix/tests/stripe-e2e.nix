{
  pkgs,
  server,
}:
let
  # The suite loads test-card.png from its own directory, so it must run from a
  # tree: as a lone store file its siblings do not exist.
  tree = pkgs.runCommand "card-printer-stripe-e2e-tree" { } ''
    mkdir -p $out
    cp ${../../scripts/stripe_e2e_test.py} $out/stripe_e2e_test.py
    cp ${../../scripts/test-card.png} $out/test-card.png
  '';
in
# The checkout/webhook end-to-end suite against the real server, with both
# stand-ins running beside it: the mock records what we sent to Stripe and to
# Resend, and the test asserts on the recordings (so webhook signatures and the
# one-email-per-order rule are exercised, not stubbed).
#
# The runner needs the same RESEND_API_KEY/EMAIL_FROM/OPERATOR_EMAIL the server
# was started with — its email section derives its expected mail count from them
# and skips itself otherwise.
pkgs.testers.nixosTest {
  name = "card-printer-stripe-e2e";

  nodes.machine = {pkgs, ...}: {
    virtualisation.memorySize = 2048;

    systemd.services.mock-stripe = {
      wantedBy = ["multi-user.target"];
      serviceConfig.ExecStart = "${pkgs.python3}/bin/python3 ${../../scripts/mock_stripe.py} 12111";
    };

    systemd.services.mock-resend = {
      wantedBy = ["multi-user.target"];
      serviceConfig.ExecStart = "${pkgs.python3}/bin/python3 ${../../scripts/mock_resend.py} 12112";
    };

    systemd.services.card-printer = {
      wantedBy = ["multi-user.target"];
      after = [
        "mock-stripe.service"
        "mock-resend.service"
      ];
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
        RESEND_API_KEY = "re_test_x";
        RESEND_API_BASE = "http://127.0.0.1:12112";
        EMAIL_FROM = "ESPuino <orders@example.test>";
        OPERATOR_EMAIL = "shop@example.test";
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
    machine.wait_for_unit("mock-resend.service")
    machine.wait_for_unit("card-printer.service")
    machine.wait_for_open_port(8099)
    print(
        machine.succeed(
            "env ADMIN_TOKEN=secret RESEND_API_KEY=re_test_x "
            "'EMAIL_FROM=ESPuino <orders@example.test>' OPERATOR_EMAIL=shop@example.test "
            "STRIPE_SECRET_KEY=sk_test_x STRIPE_WEBHOOK_SECRET=whsec_test_secret "
            "${pkgs.python3}/bin/python3 ${tree}/stripe_e2e_test.py http://127.0.0.1:8099"
        )
    )
  '';
}
