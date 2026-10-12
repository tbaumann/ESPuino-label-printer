# ESPuino Label Printer

Print-on-demand service that prints RFID/NFC cards for ESPuino, phoniebox,
TONUino, SonUINO and similar RFID-controlled music players.

A Cargo workspace with two crates:

| Crate                 | Path      | What it is                                            |
|-----------------------|-----------|-------------------------------------------------------|
| `espuino-label-printer` | `server/` | REST API (Axum + utoipa OpenAPI + SQLite/sqlx + blobs) |
| `esplp-cli`             | `cli/`    | `esplp` command-line client                           |
| — (Svelte SPA)          | `web/`    | browser UI, served by the server process              |

Stack: **Axum** (HTTP) + **utoipa** (automatic OpenAPI 3.0 + Swagger UI) +
**SQLite via sqlx** (metadata) + **filesystem blobs** (card images). Packaged
with a **Nix flake** (rust-flake / crane).

## Run (development)

```sh
nix develop -c cargo run -p espuino-label-printer   # server
```

Then open http://127.0.0.1:8080/swagger-ui — the live, generated API docs.
The raw spec is at http://127.0.0.1:8080/api-docs/openapi.json.

## Run (Nix)

```sh
nix run .#espuino-label-printer          # server, serving the web UI
nix run .#esplp-cli -- --help            # CLI
nix build .#                             # default = server + SPA, self-contained
nix build .#espuino-label-printer-web    # just the built SPA
nix build .#esplp-cli
```

The SPA is built from `web/` **in the Nix store** by `buildNpmPackage`
(`packages.espuino-label-printer-web`), and the default package wraps the server
binary so `STATIC_DIR` points straight at that store path. Nothing is read from
`server/static`, and the deployed result depends on nothing in the working tree:

```sh
cd /tmp && /path/to/checkout/result/bin/espuino-label-printer   # web UI works
```

`STATIC_DIR` is applied with `--set-default`, so exporting your own value still
overrides the built-in store path.

`nix build` needs the flake tree git-tracked (Nix only sees tracked files once
the directory is a git repo), and `Cargo.lock` must be committed — not just
present — or crane refuses to vendor:

```sh
git init && git add -A && git commit -m init
nix build .#
```

That also matters for speed: in a *non-git* checkout Nix copies the entire
directory into the store for evaluation, `web/node_modules` included, which
makes every `nix build` / `nix run` slow. Once the tree is tracked (and
`web/node_modules` is ignored) only the source is copied.

## CLI (`esplp`)

```sh
esplp --server http://127.0.0.1:8080 <command>     # or export ESPLP_SERVER=...
esplp --token <admin-token> prefix list           # or export ESPLP_TOKEN=...
```

Commands (every API field is a flag):

| Command | Purpose |
|---------|---------|
| `prefix create [--uuid U]` | create a prefix (server assigns uuid if omitted) |
| `prefix list` / `prefix get <P>` | list prefixes / show one + card count |
| `card list <P>` | index of cards in a prefix |
| `card get <P> <ID>` / `card delete <P> <ID>` | card metadata / delete |
| `upload <P> <FILE> [--id N] [--name N] [--serial-number S] [--back-text T]` | upload one card |
| `batch <P> <DIR> [--recursive] [--start-id N] [--name-template T] [--serial-prefix P] [--serial-width W] [--back-text T] [--dry-run]` | upload a folder |

Notes:

- `upload` defaults `--name` to the file's stem. With `--id` it PUTs (create or
  replace at that index); without it the server assigns the next id (POST).
- `batch` collects image files (`png jpg jpeg webp gif`), sorted for a
  deterministic order, and skips everything else. `--name-template` supports
  `{stem}`, `{filename}`, `{index}`. `--serial-prefix` produces
  `SN-001`, `SN-002`, ... (zero-padded to `--serial-width`). `--dry-run` prints
  the plan without uploading.
- `--quiet` prints just the uuid (prefix commands) or id (card commands) — handy
  for scripting: `P=$(esplp prefix create --quiet)`.
- Not yet covered by the CLI: the `status` transitions and the `?status=` filter
  — use the REST API or Swagger UI for those for now.
- `prefix list` is admin-only, so it needs the operator token: `--token <tok>`
  or `ESPLP_TOKEN`. Every other command is customer-level and needs no token.
  Passing the token anyway is harmless — the server ignores it on those routes,
  so one exported `ESPLP_TOKEN` is fine for everything.

## Web UI (`web/`)

A Svelte 5 + Vite + TypeScript SPA, served by the Rust process at `/` — same
origin as the API, no second service, no SSR.

For local work it builds into `server/static/`. Under Nix it is built into the
store instead and the binary is pointed at that path, so `nix build` output is
self-contained (see "Run (Nix)" above).

```sh
npm --prefix web install
npm --prefix web run build      # writes server/static; then just run the server
npm --prefix web run dev        # dev server, /api proxied to 127.0.0.1:8080
npm --prefix web run check      # svelte-check + tsc
npm --prefix web test           # jsdom smoke test; see below for the server it needs
```

The smoke test mounts the built bundle in jsdom and drives it against a live
server, so it needs a build in `server/static` plus a server on `:8099` started
with an `ADMIN_TOKEN` — the same token must be in the test's environment:

```sh
DATA_DIR=/tmp/esplp-smoke ADMIN_TOKEN=secret BIND_ADDR=127.0.0.1:8099 ./target/debug/espuino-label-printer &
ADMIN_TOKEN=secret npm --prefix web test
```

Its last scenario (the Pay button) needs payment configured on that server and is
skipped with a note when it is not; `scripts/stripe_e2e_test.py` is the test that
covers payment properly.

Opening `/` creates a new project and redirects to `/p/<uuid>` — the project
*is* the prefix. Use "Open" to return to an existing project code; recent
projects are remembered in `localStorage`. A project page holds the drop zone
(drag images, or a whole folder) and one row per card: front and back preview
panes, name, serial number, back text, and delete.

Front and back previews are rendered **client-side** — the front is the stored
image, the back is `back_text` laid out in a CR80-aspect panel. The server
stores no rendered back image.

### Admin UI (`/admin`)

The same SPA on an operator route. `/admin` asks for the token, trades it for
the session cookie, then lists every prefix with its status, card count and
order reference. Each row takes a new status, and there is a delete button;
`/admin/p/<uuid>` shows that prefix's cards as a read-only **print preview** —
front image and back panel, with no editor, no uploader and nothing that writes
to a card.

The admin views are just two more routes under `web/src/routes/`; there is no
separate bundle. Since the session cookie is `HttpOnly`, the UI cannot read it,
so every mutation goes through the shared `req()` helper in `web/src/lib/api.ts`,
which attaches the `x-esplp-csrf` header automatically.

## Payments (Stripe)

Checkout goes through **Stripe Checkout**, which collects the shipping address
and the money. There is no shop system and no second service: the server does
exactly two things — open a session, and listen for the webhook that says the
money arrived.

1. the customer clicks Pay on the project page. There is no submit step: paying
   is what makes an order real, and nothing is printed until it is paid;
2. `POST /api/prefixes/{prefix}/checkout` moves the prefix to `payment_pending`,
   creates a Checkout Session, and returns its `url`; the browser is sent there;
3. Stripe takes the address and the payment, then redirects back to
   `/p/<uuid>?paid=1` **and** calls `POST /api/webhooks/stripe`;
4. the webhook is the only thing that marks an order paid. It verifies the
   `Stripe-Signature` header, reads the prefix uuid out of `client_reference_id`,
   moves `payment_pending → paid`, and stores the Stripe session id as
   `order_ref`.

**The redirect is not proof of payment.** It is a separate event from the webhook
and can arrive first — or be typed in by hand. So the project page never trusts
`?paid=1`: it shows "waiting for the payment to be confirmed" and polls the
prefix until the status changes. Only the signed webhook writes `paid`.

Payment methods are not chosen in code. Stripe removed the
`payment_method_types` parameter from Checkout Session creation: the methods you
accept are configured in the Stripe Dashboard (Settings → Payment methods), and
Stripe decides which of them to show each customer, and in what order. Enable
PayPal, Klarna and SEPA direct debit there — they are worth having for a German
shop, and none of them needs a code change. Leaving that section empty means
card-only. (The old `STRIPE_PAYMENT_METHODS` variable is now inert, and logs a
warning if you still have it set.)

A session can still narrow that list, or be pinned to one:

```sh
STRIPE_EXCLUDED_PAYMENT_METHODS=klarna          # never offer these, whatever the Dashboard says
STRIPE_PAYMENT_METHOD_CONFIGURATION=pmc_1234    # pin sessions to a Dashboard pmc_… configuration
```

Shipping is one flat charge (`STRIPE_SHIPPING_CENTS`) for every destination, and
that is a Stripe limitation rather than a shortcut. A Shipping Rate has no
destination field — `active`, `created`, `delivery_estimate`, `display_name`,
`fixed_amount`, `livemode`, `metadata`, `tax_behavior`, `tax_code`, `type`, and
nothing else — so a rate cannot vary by country. Region-based shipping exists
only in the embedded Checkout form and Elements integrations, where Stripe calls
an endpoint of yours with the address and you return the price; hosted Checkout
has no such hook. Note that `STRIPE_COUNTRIES` is not a pricing knob: it restricts
which addresses are accepted at all, and the same shipping line is charged
whatever is entered.

Delayed methods are handled properly: Klarna and SEPA report
`checkout.session.completed` while still unpaid, and only settle with a later
`checkout.session.async_payment_succeeded`. The server waits for
`payment_status == "paid"` either way, so credit is never given early.

A payment that lands on a prefix which is neither `payment_pending` nor `paid` — a
draft, or a cancelled order — is deliberately **not** forced through. It is logged
at `WARN` and left for the operator to refund or honour, because guessing either
way is worse than asking. That refusal is also why the checkout moves the status
*before* opening the session: a customer can pay the moment they land on Stripe's
page, and a slow status write would otherwise strand a real payment on that WARN
line.

### Setting it up

1. create the Stripe account and stay in **test mode** to begin with;
2. copy the secret key (Developers → API keys) into `STRIPE_SECRET_KEY`;
3. add a webhook endpoint (Developers → Webhooks) at
   `https://<your-host>/api/webhooks/stripe`, subscribed to
   `checkout.session.completed` and `checkout.session.async_payment_succeeded`,
   and copy its signing secret into `STRIPE_WEBHOOK_SECRET`;
4. set `PUBLIC_BASE_URL` to the public origin, e.g. `https://cards.example.com`
   — that is where Stripe sends the customer back;
5. set the price: `STRIPE_PER_CARD_CENTS`, `STRIPE_SHIPPING_CENTS`,
   `STRIPE_CURRENCY`;
6. only then switch to live keys.

During local development the Stripe CLI replaces steps 2–3:

```sh
stripe listen --forward-to 127.0.0.1:8080/api/webhooks/stripe   # prints a whsec_…
```

`STRIPE_SECRET_KEY` unset switches payments off: `/api/shop/config` reports
`enabled: false`, the customer UI shows no way to pay, and both payment endpoints
answer `503`. Same convention as `ADMIN_TOKEN` — unset is never open. Unset
`STRIPE_WEBHOOK_SECRET` is the one combination to avoid: checkout still works,
but every webhook is rejected and no order ever becomes `paid`.

### Testing payments without a Stripe account

`scripts/mock_stripe.py` stands in for `api.stripe.com` and records the form body
it receives. `scripts/stripe_e2e_test.py` drives the real API against it — it
creates a prefix, uploads cards, asks for a checkout session, asserts on exactly
what was sent, then delivers webhook events to the live endpoint, signed the way
Stripe signs them:

```sh
python3 scripts/mock_stripe.py 12111 &
DATA_DIR=/tmp/esplp-stripe STRIPE_SECRET_KEY=sk_test_x \
  STRIPE_WEBHOOK_SECRET=whsec_test_secret STRIPE_API_BASE=http://127.0.0.1:12111 \
  PUBLIC_BASE_URL=http://127.0.0.1:8080 \
  STRIPE_PER_CARD_CENTS=500 STRIPE_SHIPPING_CENTS=250 \
  ./target/debug/espuino-label-printer &
python3 scripts/stripe_e2e_test.py http://127.0.0.1:8080
```

## Configuration (server environment)

| Variable       | Default                        | Meaning                              |
|----------------|--------------------------------|--------------------------------------|
| `BIND_ADDR`    | `127.0.0.1:8080`               | listen address                       |
| `DATA_DIR`     | `data`                         | root for the DB and image blobs      |
| `DATABASE_URL` | `sqlite://$DATA_DIR/cards.db`  | sqlx SQLite URL                      |
| `RUST_LOG`     | `info,tower_http=info`         | tracing filter                       |
| `STATIC_DIR`   | `server/static`                | built SPA to serve at `/`            |
| `ADMIN_TOKEN`  | *(unset)*                      | operator bearer token; unset switches the admin API off |
| `SESSION_COOKIE_SECURE` | *(unset)*             | force the `Secure` flag on the admin session cookie; unset = infer from `X-Forwarded-Proto` |
| `STRIPE_SECRET_KEY` | *(unset)*                 | Stripe API key; unset switches checkout and the webhook off (`503`) |
| `STRIPE_WEBHOOK_SECRET` | *(unset)*             | signing secret for the webhook; without it every webhook is rejected |
| `STRIPE_API_BASE` | `https://api.stripe.com`     | API base; override to point at a mock |
| `PUBLIC_BASE_URL` | *(unset)*                   | public origin for the post-payment redirect; unset falls back to the request `Host` |
| `STRIPE_CURRENCY` | `eur`                       | ISO 4217 code |
| `STRIPE_PER_CARD_CENTS` | `500`                 | price of one card, in minor units (cents) |
| `STRIPE_SHIPPING_CENTS` | `0`                   | flat shipping charge; `0` adds no shipping line |
| `STRIPE_COUNTRIES` | `DE,AT,CH`                 | allowed shipping-address countries |
| `STRIPE_EXCLUDED_PAYMENT_METHODS` | *(none)*       | methods to never offer in a session |
| `STRIPE_PAYMENT_METHOD_CONFIGURATION` | *(unset)*  | pin sessions to a Dashboard `pmc_…` configuration |

Layout on disk:

```
data/
  cards.db                   # prefix + card metadata (the single index)
  blobs/<prefix>/<id>.<ext>  # card front images
```

## Data model

- **Prefix** — a UUID that groups one customer's cards (one order/batch). The
  customer picks a UUID or the server assigns one. `card_count` is derived.
  Carries a `status` (see lifecycle below), `status_changed_at` and an optional
  `order_ref` (external shop id).
- **Card** — `id` (1-based index *within its prefix*), `name`, optional
  `serial_number`, optional `back_text` (black-and-white only half-panel back
  for song lists), and a front image (ISO/IEC 7810 / CR80, scaled to fit).

## Prefix lifecycle

```
draft ──Pay──▶ payment_pending ──paid──▶ paid ──▶ queued ──▶ printing ──▶ printed
  └─────────────────── cancelled (reachable from any pre-printing state) ───────────────────┘
```

- The customer's Pay button opens a checkout (`POST .../checkout`), which moves a
  draft to `payment_pending`; Stripe's webhook marks it paid; the print queue
  advances `queued → printing → printed`. There is no submit step, and
  `POST .../submit` no longer exists — it answers `410`.
- A `payment_pending` prefix can be paid for again: a closed window or a declined
  card is not a dead end, and nothing has to be reset by hand.
- A customer may edit cards only while the prefix is `draft`; the moment they
  click Pay the project is sealed for them, and from `payment_pending` onward
  they get the read-only view. The operator may still edit while the prefix is
  `draft`, `payment_pending`, `paid` or `queued`; once it reaches `printing`
  even the operator's card writes (PUT/POST/DELETE) return `409`. Reads always
  work.
- `GET /api/prefixes?status=paid` is the print queue's work list.
- Transitions are validated; an invalid one returns `409`. Re-applying the
  current status is a no-op, so shop/queue callbacks can safely retry.
- There are no accounts: a customer's credential is the prefix uuid they know,
  and the operator additionally presents a bearer token. See "Who may do what".

## Who may do what

There are no accounts. Two kinds of caller:

- **Customer** — anyone holding a prefix uuid. That uuid *is* the credential,
  and it cannot be discovered: listing is admin-only, so there is no
  enumeration route. A customer can read their own prefix and its cards, edit
  them and delete the prefix while it is still a `draft`, and pay for it. Once
  it leaves `draft` the project is sealed for them.
- **Operator** — either sends `Authorization: Bearer $ADMIN_TOKEN` (the CLI), or
  logs in through the admin UI, which trades the token for a session cookie.
  Can list every prefix, set any status at any time, delete a prefix in any
  state, and write cards even while one is printing.

| Operation                                  | Customer      | Operator |
|--------------------------------------------|---------------|----------|
| list prefixes                              | ✗             | ✓        |
| create a prefix                            | ✓             | ✓        |
| read a prefix / its cards / an image       | ✓             | ✓        |
| add, edit or delete cards                  | ✓ if writable | ✓ always |
| pay (`draft → payment_pending`)            | ✓             | ✓        |
| set status (`PATCH .../status`)            | ✗             | ✓ (any transition) |
| delete a prefix                            | ✓ if writable | ✓ always |

For a **customer**, "writable" means `draft` only: the moment they click Pay
the project is sealed for them, and from `payment_pending` onward they get the
read-only view. For the **operator**, "writable" means `draft`, `payment_pending`,
`paid` or `queued`; from `printing` onward even the operator's card writes are
frozen, though they can still set any status — which is what makes a stuck
print job recoverable, since a customer has no legal edge out of `printing`.

When `ADMIN_TOKEN` is unset the admin API is switched off: admin-only calls
answer `503` with an explanatory body while everything customer-facing keeps
working. An unset token is never an open door.

The token is compared in constant time. Failed admin-credential attempts are
rate-limited per client address: 5 misses inside 15 minutes (a wrong bearer
token on any request, or a wrong token at login) lock that address out for 15
minutes, during which further attempts answer `429`. The counters are
in-memory, so a restart clears them. The key is the left-most `X-Forwarded-For`
entry (or `X-Real-Ip`, or a shared constant for direct clients), so put the
server behind a proxy that sets it or every direct client shares one bucket.

### Admin session

`POST /api/admin/session` with `{"token": "…"}` sets:

```
esplp_admin=<random>; Path=/api; HttpOnly; SameSite=Strict; Max-Age=43200
```

`HttpOnly` keeps script from reading it, so a stored-XSS bug cannot steal the
credential — which is why the admin UI gets a cookie rather than holding the
token in JS. The price is that cookies are *ambient*: a browser attaches them to
cross-site requests too. Two locks answer that:

  - `SameSite=Strict` — the cookie is not sent from another site at all;
  - every non-GET request that carries the cookie must also send an
    `x-esplp-csrf` header, or it gets `403`. A cross-site page cannot add one.

Bearer-token callers are exempt from the header rule: a browser never sets that
header by itself, so there is nothing to forge.

`GET /api/admin/session` answers `200`/`401` so the UI can tell whether it is
already logged in. `DELETE /api/admin/session` logs out, and is idempotent.
Sessions live in memory and expire after 12h, so restarting the server logs the
operator out. `Secure` is added to the cookie when `X-Forwarded-Proto: https` is
present; force it with `SESSION_COOKIE_SECURE=true` (or `=false` to suppress).

## API

`(admin)` marks operations that need the admin credential — the bearer token or
an admin session.

| Method | Path                                   | Purpose                       |
|--------|----------------------------------------|-------------------------------|
| POST   | `/api/prefixes`                        | create prefix (optional uuid) |
| GET    | `/api/prefixes`                        | list prefixes, `?status=` filter **(admin)** |
| GET    | `/api/prefixes/{prefix}`               | get one prefix + card count   |
| DELETE | `/api/prefixes/{prefix}`               | delete prefix + all its cards and blobs (writable, or admin) |
| POST   | `/api/prefixes/{prefix}/submit`        | **removed** — answers `410`, use checkout |
| PATCH  | `/api/prefixes/{prefix}/status`        | set status, any transition **(admin)** |
| GET    | `/api/prefixes/{prefix}/cards`         | index of cards in the prefix  |
| POST   | `/api/prefixes/{prefix}/cards`         | add card, server-assigned id  |
| DELETE | `/api/prefixes/{prefix}/cards`         | delete every card in the prefix |
| GET    | `/api/prefixes/{prefix}/cards/{id}`    | card metadata                 |
| PUT    | `/api/prefixes/{prefix}/cards/{id}`    | create/replace card at id (image required) |
| PATCH  | `/api/prefixes/{prefix}/cards/{id}`    | update name / serial / back text only |
| DELETE | `/api/prefixes/{prefix}/cards/{id}`    | delete card + its blob        |
| GET    | `/api/prefixes/{prefix}/cards/{id}/image` | card front image bytes     |
| GET    | `/api/shop/config`                     | price, currency, and whether payment is on |
| POST   | `/api/prefixes/{prefix}/checkout`      | open a Stripe Checkout session (draft or payment_pending only) |
| POST   | `/api/webhooks/stripe`                 | Stripe's payment callback (signature-authenticated) |
| POST   | `/api/admin/session`                   | log in: token → session cookie |
| GET    | `/api/admin/session`                   | am I logged in? **(admin)**   |
| DELETE | `/api/admin/session`                   | log out                       |

Card create/update (`POST`/`PUT`) takes `multipart/form-data` with fields
`name`, `serial_number`, `back_text` and a file part `image`.

`PUT` is full-replace and **requires** the image; use `PATCH` to edit
name/serial/back text without re-uploading it. Re-uploading an image in a
different format replaces the old blob (no orphan files). Deleting a card (or
clearing a prefix) removes the corresponding blobs.

## Smoke test

```sh
nix shell nixpkgs#python3 -c python3 scripts/make_png.py /tmp/card.png
export ESPLP_SERVER=http://127.0.0.1:8080
P=$(./target/debug/esplp prefix create --quiet)
./target/debug/esplp upload "$P" /tmp/card.png --name Conni --serial-number SN-001
./target/debug/esplp card list "$P"
```