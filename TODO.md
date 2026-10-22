# TODO

## Image upload validation

`GET /api/prefixes/{prefix}/cards/{id}/image/front` serves back whatever
`Content-Type` the uploader sent, so it is a stored-XSS vector on the app's own
origin. Verified: an upload with `Content-Type: text/html` is accepted (201),
and the endpoint then returns `content-type: text/html` with no
`X-Content-Type-Options: nosniff`.

1. Restrict the _served_ content type to `image/png|jpeg|webp|gif`. The bytes
   can still be stored exactly as uploaded — it is the served type that must not
   be attacker-chosen. (`extension_for` in `server/src/routes.rs` already knows
   the accepted set.)
2. Send `X-Content-Type-Options: nosniff` on the image response.

Worth doing on its own, independent of the admin UI. Code: `store_card_form` and
`get_card_image`.

## Markdown in the back text

`back_text` is markdown, and it is rendered into a card's back. Markdown is just
another back _source_ (`back_source = 'markdown'`): the ribbon decides the
colour depth, the renderer produces the raster the spooler prints, and the pane
shows that same server artifact like every other back. Nothing about a back is
rendered in the browser.

- The server-side renderer is the only renderer, and it is still a **stub** —
  `render::render_back_markdown_stub` reports `failed` with its reason, so a
  markdown-backed card is visibly not printable rather than coming out with a
  blank side. A real renderer (an external markdown→image binary such as Markie)
  lands behind the same signature, which already takes the ribbon's `BackDepth`.
- The subset it will render is small: headings, bold, italic and lists. Links are
  just text on a card, never an `<a>`, so a `javascript:` href is inert; raw HTML
  is dropped whole, tag and text. Images are **refused outright** — the whole
  construct is dropped, alt text included — and the editor *says* so rather than
  leaving a gap where the artwork was. CommonMark only: no GFM tables,
  strikethrough or autolinks.
- The back-text editor is the only place the text is laid out for the customer,
  and it is due to become WYSIWYG; the pane stays the server's render.
  `web/src/lib/backText.ts` now holds nothing but what that editor has to say
  about the typed text.

## Done — both ribbons are the same except in the print raster

Neither ribbon restricts a back source any more. The ribbon decides one thing,
the colour depth the back is rendered in, and both renderers take the same
picture: 24-bit colour on YMCKO, a 1-bit thresholded raster on YMCKOKO's second
black panel. A picture on a black back is allowed and comes out in black and
white; the preview is the depth-accurate render, so the pane repaints when the
ribbon changes.

Consequences worth keeping: switching ribbon is lossless and re-renders only
the derived artifacts (`PATCH /api/prefixes/{prefix}`), `Ribbon::allows_back`,
`cards_incompatible_with_ribbon` and the SPA's `ribbonAllowsBack` are gone, and
billing follows the ribbon's own unit — a printed side on YMCKO, a whole card on
YMCKOKO (`Ribbon::billing_unit`/`billed_units`, `unit` in the shop config).
The old always-two rule charged two card prices for a single-sided YMCKOKO card.

The rasters are scaled to fit the printer's internal bitmap (`fit_to`, letterboxed
on white), which is what the preview shows, one scale factor apart; the overscan
is the only difference. Centre-cropping to fill it was the bug that ate the edges
of every picture whose aspect differed from the panel's.
