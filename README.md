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
- Not yet covered by the CLI: the `submit` / `status` transitions and the
  `?status=` filter — use the REST API or Swagger UI for those for now.
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

Opening `/` creates a new project and redirects to `/p/<uuid>` — the project
*is* the prefix. Use "Open" to return to an existing project code; recent
projects are remembered in `localStorage`. A project page holds the drop zone
(drag images, or a whole folder) and one row per card: front and back preview
panes, name, serial number, back text, and delete.

Front and back previews are rendered **client-side** — the front is the stored
image, the back is `back_text` laid out in a CR80-aspect panel. The server
stores no rendered back image.

## Configuration (server environment)

| Variable       | Default                        | Meaning                              |
|----------------|--------------------------------|--------------------------------------|
| `BIND_ADDR`    | `127.0.0.1:8080`               | listen address                       |
| `DATA_DIR`     | `data`                         | root for the DB and image blobs      |
| `DATABASE_URL` | `sqlite://$DATA_DIR/cards.db`  | sqlx SQLite URL                      |
| `RUST_LOG`     | `info,tower_http=info`         | tracing filter                       |
| `STATIC_DIR`   | `server/static`                | built SPA to serve at `/`            |
| `ADMIN_TOKEN`  | *(unset)*                      | operator bearer token; unset switches the admin API off |

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
draft ──submit──▶ submitted ──paid──▶ paid ──▶ queued ──▶ printing ──▶ printed
  └──────────── cancelled (reachable from any pre-printing state) ────────┘
```

- The customer submits (`POST .../submit`); the shop marks it paid
  (`PATCH .../status`); the print queue advances `queued → printing → printed`.
- Cards are editable while the prefix is `draft`, `submitted`, `paid` or
  `queued`. Once it reaches `printing` the prefix is **frozen** and card writes
  (PUT/POST/DELETE) return `409`. Reads always work.
- `GET /api/prefixes?status=paid` is the print queue's work list.
- Transitions are validated; an invalid one returns `409`. Re-applying the
  current status is a no-op, so shop/queue callbacks can safely retry.
- There are no accounts: a customer's credential is the prefix uuid they know,
  and the operator additionally presents a bearer token. See "Who may do what".

## Who may do what

There are no accounts. Two kinds of caller:

- **Customer** — anyone holding a prefix uuid. That uuid *is* the credential,
  and it cannot be discovered: listing is admin-only, so there is no
  enumeration route. A customer can read and modify their own prefix and its
  cards, submit it, and delete it while it is still writable.
- **Operator** — sends `Authorization: Bearer $ADMIN_TOKEN`. Can list every
  prefix, set any status at any time, delete a prefix in any state, and write
  cards even while one is printing.

| Operation                                  | Customer      | Operator |
|--------------------------------------------|---------------|----------|
| list prefixes                              | ✗             | ✓        |
| create a prefix                            | ✓             | ✓        |
| read a prefix / its cards / an image       | ✓             | ✓        |
| add, edit or delete cards                  | ✓ if writable | ✓ always |
| submit (`draft → submitted`)               | ✓             | ✓        |
| set status (`PATCH .../status`)            | ✗             | ✓ (any transition) |
| delete a prefix                            | ✓ if writable | ✓ always |

"Writable" means `draft`, `submitted`, `paid` or `queued`. From `printing`
onwards a prefix is frozen for customers; the operator can still work on it,
which is what makes a stuck print job recoverable — a customer has no legal
edge out of `printing`, and the operator can set any status regardless.

When `ADMIN_TOKEN` is unset the admin API is switched off: admin-only calls
answer `503` with an explanatory body while everything customer-facing keeps
working. An unset token is never an open door.

The token is compared in constant time. There is no rate limiting yet.

## API

`(admin)` marks operations that need the bearer token.

| Method | Path                                   | Purpose                       |
|--------|----------------------------------------|-------------------------------|
| POST   | `/api/prefixes`                        | create prefix (optional uuid) |
| GET    | `/api/prefixes`                        | list prefixes, `?status=` filter **(admin)** |
| GET    | `/api/prefixes/{prefix}`               | get one prefix + card count   |
| DELETE | `/api/prefixes/{prefix}`               | delete prefix + all its cards and blobs (writable, or admin) |
| POST   | `/api/prefixes/{prefix}/submit`        | submit for payment (draft→submitted) |
| PATCH  | `/api/prefixes/{prefix}/status`        | set status, any transition **(admin)** |
| GET    | `/api/prefixes/{prefix}/cards`         | index of cards in the prefix  |
| POST   | `/api/prefixes/{prefix}/cards`         | add card, server-assigned id  |
| DELETE | `/api/prefixes/{prefix}/cards`         | delete every card in the prefix |
| GET    | `/api/prefixes/{prefix}/cards/{id}`    | card metadata                 |
| PUT    | `/api/prefixes/{prefix}/cards/{id}`    | create/replace card at id (image required) |
| PATCH  | `/api/prefixes/{prefix}/cards/{id}`    | update name / serial / back text only |
| DELETE | `/api/prefixes/{prefix}/cards/{id}`    | delete card + its blob        |
| GET    | `/api/prefixes/{prefix}/cards/{id}/image` | card front image bytes     |

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