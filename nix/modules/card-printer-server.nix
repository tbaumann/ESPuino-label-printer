{self}: {
  config,
  lib,
  pkgs,
  ...
}:
# Deployment module for the shop server: a systemd service with every non-secret
# setting as an option, and the secrets in an `environmentFile` — point that at a
# sops secret (see the README's "Deploy (NixOS)" section).
#
# Option name → environment variable is name to SCREAMING_SNAKE (bindAddress and
# port make up BIND_ADDR; stripeShippingCents is STRIPE_SHIPPING_CENTS). The four
# secrets have no option, on purpose: ADMIN_TOKEN, STRIPE_SECRET_KEY,
# STRIPE_WEBHOOK_SECRET and RESEND_API_KEY live only in that file.
#
# `package` defaults to this flake's own build — the crate binary with the SPA
# baked in and MARKIE_BIN pointed at its store path — so a deployment needs
# nothing from a working tree and no second nixpkgs has to rebuild the Rust
# workspace.
let
  cfg = config.services.card-printer-server;
  stateDir = "/var/lib/card-printer-server";
  # "NAME=value" for an option that may be unset. An absent variable and an empty
  # one mean the same thing to the server, so an unset option is left out
  # entirely rather than written as `NAME=`.
  optionalEnv = name: value: lib.optional (value != null) "${name}=${value}";
in {
  options.services.card-printer-server = {
    enable = lib.mkEnableOption "the ESPuino label printer shop (card-printer-server)";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.card-printer-server;
      defaultText =
        lib.literalExpression
        "self.packages.\${pkgs.stdenv.hostPlatform.system}.card-printer-server";
      description = ''
        The server to run. The default is this flake's own package: the crate
        binary wrapped so `STATIC_DIR` points at the store-built SPA and
        `MARKIE_BIN` at the packaged markdown backend (`--set-default`, so the
        environment can still override both).
      '';
    };

    user = lib.mkOption {
      type = lib.types.str;
      default = "card-printer-server";
      description = ''
        User (and group) the service runs as; created as a system account when
        the service is enabled.
      '';
    };

    bindAddress = lib.mkOption {
      type = lib.types.str;
      default = "0.0.0.0";
      description = ''
        Address to listen on. The default matches the container image; a
        deployment behind a reverse proxy on the same host can use "127.0.0.1"
        and leave the firewall closed.
      '';
    };

    port = lib.mkOption {
      type = lib.types.port;
      default = 8080;
      description = "TCP port to listen on.";
    };

    openFirewall = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Open `port` in the firewall. Off by default: the shop is normally
        reached through a proxy or tunnel, and the customer pages must be
        reachable from the internet through that, not directly.
      '';
    };

    dataDir = lib.mkOption {
      type = lib.types.path;
      default = stateDir;
      description = ''
        Root for the SQLite database, the card image blobs and the markdown
        backend's scratch directory. At the default path systemd creates and
        owns it (`StateDirectory`); a custom path must already exist, be
        writable by the service user, and be in your preservation list on a host
        with an impermanent root — losing it loses every order.
      '';
    };

    publicBaseUrl = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "https://cards.example.com";
      description = ''
        Public origin of this shop, without a trailing slash. Used for the
        post-payment redirect and the link in the order email; unset falls back
        to the request's `Host` header, which only works behind a proxy that
        passes the customer's own host through.
      '';
    };

    logLevel = lib.mkOption {
      type = lib.types.str;
      default = "info,tower_http=info";
      description = "`RUST_LOG` filter; raise to `debug` while diagnosing.";
    };

    sessionCookieSecure = lib.mkOption {
      type = lib.types.nullOr lib.types.bool;
      default = null;
      description = ''
        Force the `Secure` flag on the admin session cookie. Unset infers it from
        `X-Forwarded-Proto` per request, which is what a proxy in front of an
        https shop does; set `true` to force it regardless (a shop not on https
        needs `false` to keep the admin UI usable).
      '';
    };

    printRasterWidth = lib.mkOption {
      type = lib.types.int;
      default = 2032;
      description = ''
        Width of the print raster, in pixels: the printer's INTERNAL bitmap, not
        the card outline (the overscan past it is deliberate). Leave at the
        Primacy's 600 dpi default unless the printer differs — the wrong value
        prints scaled or cropped cards.
      '';
    };

    printRasterHeight = lib.mkOption {
      type = lib.types.int;
      default = 1296;
      description = "Height of the print raster, in pixels (see `printRasterWidth`).";
    };

    stripeCurrency = lib.mkOption {
      type = lib.types.str;
      default = "eur";
      description = "ISO 4217 code every price is charged in.";
    };

    stripeShippingCents = lib.mkOption {
      type = lib.types.int;
      default = 0;
      description = "Flat shipping charge in minor units; `0` adds no shipping line.";
    };

    stripeCountries = lib.mkOption {
      type = lib.types.str;
      default = "DE,AT,CH";
      description = "Comma-separated countries Checkout will collect shipping addresses in.";
    };

    stripeExcludedPaymentMethods = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "sepa_debit";
      description = ''
        Comma-separated payment methods to never offer; unset offers whatever the
        Stripe account has enabled.
      '';
    };

    stripePaymentMethodConfiguration = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "pmc_…";
      description = ''
        Pin every session to a Dashboard payment-method configuration, instead of
        letting the shop pick the methods itself.
      '';
    };

    stripeApiBase = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = ''
        Stripe API base, for pointing the shop at a mock. Unset means the real
        one; the server warns when a live key is used against anything else.
      '';
    };

    emailFrom = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "ESPuino <orders@example.com>";
      description = ''
        Sending address for the order emails, on a domain verified in Resend.
        Unset (or no `RESEND_API_KEY` in the environment file) switches order
        email off, with a warning in the log.
      '';
    };

    operatorEmail = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = ''
        Where the operator's copy of a paid order goes; unset mails the customer
        only.
      '';
    };

    resendApiBase = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "Resend API base, for pointing email at a mock.";
    };

    environmentFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      example = lib.literalExpression ''config.sops.secrets."card-printer-env".path'';
      description = ''
        systemd environment file holding the shop's secrets, which have no option
        of their own:

        - `ADMIN_TOKEN` — operator bearer token; unset switches the admin API
          off, and with it the spooler's queue reads and print marks;
        - `STRIPE_SECRET_KEY` — unset switches checkout and the webhook off
          (they answer `503`);
        - `STRIPE_WEBHOOK_SECRET` — without it every webhook delivery is
          rejected;
        - `RESEND_API_KEY` — the Resend key for the order emails.

        A sops secret's path is the intended value. systemd reads this file as
        root before dropping privileges, so it may stay root-owned and mode
        `0400`, which is what sops-nix renders by default. Settings here WIN over
        the options above, so do not duplicate a non-secret in it.
      '';
    };

    environment = lib.mkOption {
      type = lib.types.attrsOf (lib.types.oneOf [lib.types.str lib.types.int]);
      default = {};
      example = {RUST_BACKTRACE = "1";};
      description = ''
        Extra environment variables for the unit, for the settings this module
        does not name (`DATABASE_URL`, `STATIC_DIR`, `MARKIE_BIN`, a mock's
        `STRIPE_API_BASE`, …). These are applied last and override the options.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    users.users.${cfg.user} = {
      isSystemUser = true;
      group = cfg.user;
    };
    users.groups.${cfg.user} = {};

    networking.firewall = lib.mkIf cfg.openFirewall {
      allowedTCPPorts = [cfg.port];
    };

    systemd.services.card-printer-server = {
      description = "ESPuino label printer — card shop API, web UI and print queue";
      wantedBy = ["multi-user.target"];
      after = ["network.target" "network-online.target"];
      wants = ["network-online.target"];
      unitConfig.RequiresMountsFor = [(toString cfg.dataDir)];
      serviceConfig = {
        Type = "simple";
        User = cfg.user;
        Group = cfg.user;
        # systemd creates and owns /var/lib/card-printer-server (0750); the
        # server creates its own subdirectories under DATA_DIR.
        WorkingDirectory = stateDir;
        StateDirectory = "card-printer-server";
        StateDirectoryMode = "0750";
        ExecStart = "${cfg.package}/bin/card-printer-server";
        Restart = "on-failure";
        RestartSec = 2;

        Environment =
          [
            "BIND_ADDR=${cfg.bindAddress}:${toString cfg.port}"
            # toString: a path VALUE interpolated into a string would be copied
            # into the store and the unit would name that copy.
            "DATA_DIR=${toString cfg.dataDir}"
            "RUST_LOG=${cfg.logLevel}"
            "PRINT_RASTER_WIDTH=${toString cfg.printRasterWidth}"
            "PRINT_RASTER_HEIGHT=${toString cfg.printRasterHeight}"
            "STRIPE_CURRENCY=${cfg.stripeCurrency}"
            "STRIPE_SHIPPING_CENTS=${toString cfg.stripeShippingCents}"
            "STRIPE_COUNTRIES=${cfg.stripeCountries}"
          ]
          ++ optionalEnv "PUBLIC_BASE_URL" cfg.publicBaseUrl
          ++ optionalEnv "STRIPE_EXCLUDED_PAYMENT_METHODS" cfg.stripeExcludedPaymentMethods
          ++ optionalEnv "STRIPE_PAYMENT_METHOD_CONFIGURATION" cfg.stripePaymentMethodConfiguration
          ++ optionalEnv "STRIPE_API_BASE" cfg.stripeApiBase
          ++ optionalEnv "EMAIL_FROM" cfg.emailFrom
          ++ optionalEnv "OPERATOR_EMAIL" cfg.operatorEmail
          ++ optionalEnv "RESEND_API_BASE" cfg.resendApiBase
          ++ lib.optionals (cfg.sessionCookieSecure != null) [
            "SESSION_COOKIE_SECURE=${lib.boolToString cfg.sessionCookieSecure}"
          ]
          # Last, so it wins.
          ++ lib.mapAttrsToList (name: value: "${name}=${value}") cfg.environment;

        # The secrets. Absent for a shop with no Stripe/payment and no admin
        # token, which is a supported state — the server says so in the log.
        EnvironmentFile = lib.mkIf (cfg.environmentFile != null) cfg.environmentFile;

        # Hardening. The card data (customer images, shipping addresses) is worth
        # more than the service is, and it only needs its own state directory
        # writable: the SPA, markie, fonts and CA store are read-only store or
        # /etc paths.
        PrivateTmp = true;
        ProtectSystem = "strict";
        ReadWritePaths = [(toString cfg.dataDir)];
        NoNewPrivileges = true;
        ProtectHome = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectKernelLogs = true;
        ProtectControlGroups = true;
        ProtectClock = true;
        ProtectHostname = true;
        LockPersonality = true;
        RestrictSUIDSGID = true;
        RestrictRealtime = true;
        RestrictNamespaces = true;
        PrivateDevices = true;
        ProtectProc = "invisible";
        ProcSubset = "pid";
        SystemCallArchitectures = "native";
        # AF_NETLINK is for glibc's getaddrinfo (it reads route information for
        # its address sorting); TLS is native-tls, so the outbound calls to
        # Stripe and Resend resolve through the C library, not in-process.
        RestrictAddressFamilies = [
          "AF_INET"
          "AF_INET6"
          "AF_UNIX"
          "AF_NETLINK"
        ];
        # Order data stays private even if a permission is ever wrong.
        UMask = "0077";
        # An empty string renders `CapabilityBoundingSet=` (drop all); an empty
        # LIST would render nothing and silently keep the defaults.
        CapabilityBoundingSet = [""];
      };
    };
  };
}
