# TODO

## Markdown in the back text

`back_text` is markdown, and it is rendered into a card's back. Markdown is just
another back _source_ (`back_source = 'markdown'`): the ribbon decides the
colour depth, the renderer produces the raster the spooler prints, and the pane
shows that same server artifact like every other back. Nothing about a back is
rendered in the browser.

- The backend is **`markie`** (`nix/packages/markie.nix`, upstream
  [lsj5031/MarkieCli](https://github.com/lsj5031/MarkieCli)) — a native
  markdown→PNG renderer, handed the text on stdin and asked for one picture.
  `render::MarkdownRenderer` is the whole integration: `MARKIE_BIN` (the server
  package's wrapper points it at its store path) and `DATA_DIR/tmp` for the one
  file it writes.
- There is deliberately **no subset**, and nothing in this repo parses markdown
  any more (the `pulldown-cmark` dependency is gone with the old approach). The
  backend lays out whatever it is given and the card prints what came out; the
  earlier list of allowed constructs, and the refusals that went with it, were
  the wrong layer for the job.
- The document is laid out **as wide as the card is long** (`render::MARKDOWN_WIDTH`:
  the CR80 long edge at the scale where the backend's fixed 16 px body text lands
  around 2.7 mm on the card) and rasterised from there at whatever `--png-scale`
  reaches the printer's bitmap. Text size and wrapping are therefore properties of
  the *card*, not of the print DPI: a change of raster size re-renders sharper, it
  does not re-flow the text.
- What the backend returns is then just an image, and it takes the same depth
  reduction (`render_back_display` / `render_back_raster`) as an uploaded picture
  — which is what makes the pane, the raster and the spooler agree. It is
  **placed differently**, though: a rendered document is anchored at the panel's
  top-left at the card's own width and clipped at the panel's edges, while an
  uploaded picture is fitted inside the panel and centred. That is
  `render::Placement`, named at the call site in `rerender_card` — a long list
  keeps the text size the layout gave it and loses its tail, rather than being
  shrunk until all of it fits.
- **The page margin is the theme's, and it is still there.** A markdown picture is
  anchored at the panel's top-left, but its first ink sits at the `github_light`
  theme's own body margin — ~33 px of a 508-wide layout, ~2.7 mm on the card — so
  the text does not start at the frame's edge. A theme (or a stylesheet override)
  with no page margin is a later task; the anchoring landed, and it is what will
  make that show up at the panel's edge.
- **A linked picture is fetched by the backend**, not by this server: a remote
  `![x](https://…)` makes the render issue one HTTP GET to a host the customer
  names (10 s timeout, 10 MiB cap). One that cannot be fetched does *not* fail
  the render — the backend warns and lays out the alt text in its place, so the
  card prints without the picture, and the warning is logged (`markdown render:
  …`) because nothing on the card says the artwork went missing. Blocking or
  rewriting remote references is an open decision; nothing blocks them today.
- **Fonts are the runtime's, not the package's.** The backend renders with the
  fonts it can find (fontdb reading fontconfig's config, plus a `fonts` directory
  beside its working directory) and refuses to start without any. A fontconfig
  host therefore needs nothing, and the OCI image carries its own
  `/etc/fonts/fonts.conf` naming the DejaVu it ships (`oci.nix`), so a markdown
  back renders there too.
- The back-text editor **is WYSIWYG** (`web/src/lib/cards/MarkdownEditor.svelte`,
  Tiptap v3): its value in and out is the markdown itself, its toolbar applies
  the constructs, and its schema carries everything a back can hold — the starter
  set plus pictures, tables and task lists, so an edit cannot drop a construct
  the backend would have printed. The pane stays the server's render, so
  `web/src/lib/backText.ts` is gone along with the image-refusal courtesy it
  carried.
- The editor is most of the SPA's bundle now — 586 kB from 82 kB (~187 kB
  gzipped from 29), of which ProseMirror is ~467 kB and markdown parsing ~62 kB.
  If that matters, the lever is a dynamic `import()` of the editor component so
  only a markdown back pulls it in.
- Nothing is written per keystroke: a card's name and its back text are both
  written when typing stops (1200 ms) or the field is left, and a response is
  never copied back into the field it came from.
