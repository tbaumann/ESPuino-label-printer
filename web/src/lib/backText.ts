import { Marked } from 'marked'
import DOMPurify from 'dompurify'

// Render `back_text` (markdown) while the customer is typing.
//
// This is the *editing* view, not the printed card. What prints is the server's
// raster, produced from the same markdown by `server/src/render.rs` — which is
// still a stub for a markdown back, so such a card reports itself not printable
// rather than coming out with a blank side. Keep the subset below in step with
// that renderer: it takes the ribbon's colour depth and draws the same small
// set, and a preview that offered more than the print could produce would be a
// lie about the card the customer is about to order.
//
// Supported subset (everything else degrades to its text content):
//   - headings: `#`..`######`  -> <h1>..<h6>
//   - bold:     `**x**`/`__x__` -> <strong>
//   - italic:   `*x*`/`_x_`     -> <em>
//   - lists:    `-`/`*`/`+` and `1.` -> <ul>/<ol>/<li>, nested allowed
//   - paragraphs and line breaks
//
// Deliberate refusals:
//   - links are rendered as their plain text — there is nothing to click on a
//     card, so no <a> is ever emitted (and a `javascript:` href is inert);
//   - images are REFUSED outright: the whole construct is dropped, no alt
//     text, no placeholder;
//   - raw HTML is dropped whole — tag and text — never passed through, so
//     nothing in the markdown is ever treated as markup.
//
// Parsing is CommonMark only (`gfm: false`), which is what the server renderer
// parses too: tables, `~~strikethrough~~` and bare-URL autolinks are GFM, they
// are not in the subset, and a preview that rendered them would be promising a
// card the printer cannot produce.
//
// The output is sanitised with DOMPurify against an allowlist of exactly the
// subset tags, so `{@html}` in the component is safe to use on it.
//
// The subset is asserted, not assumed: the smoke test renders this markdown
// through the preview and checks each refusal (`web/test/smoke.mjs`, scenario
// B2).

const ALLOWED_TAGS = [
  'h1', 'h2', 'h3', 'h4', 'h5', 'h6',
  'p', 'strong', 'em', 'ul', 'ol', 'li', 'br',
]

const marked = new Marked({
  // Sync only: the renderer is pure string-in/string-out, no async extensions.
  async: false,
  // CommonMark only, matching the server renderer's `Options::empty()`. GFM
  // would parse tables, `~~strikethrough~~` and bare-URL autolinks here that
  // the print side does not implement, so `~~x~~` would preview as "x" and
  // print as "~~x~~". Neither side parses them; both print the literal text.
  gfm: false,
  renderer: {
    // Refuse images outright: drop the whole construct, alt text included.
    image() {
      return ''
    },
    // Links are plain text on a card. Render the inner markdown (so a bold
    // link stays bold) but never emit an <a>.
    link(token) {
      return this.parser.parseInline(token.tokens ?? [])
    },
    // Raw HTML is dropped whole, text included — the server renderer never
    // passes a tag through either. DOMPurify on its own would strip the tag but
    // keep its text, which the print side would then not have.
    html() {
      return ''
    },
  },
})

/** Render markdown `back_text` to sanitised HTML for the card back. */
export function renderBackText(markdown: string): string {
  const html = marked.parse(markdown, { async: false })
  return DOMPurify.sanitize(html, { ALLOWED_TAGS, ALLOWED_ATTR: [] })
}

/**
 * Whether the markdown asks for an image, which the renderer refuses outright
 * (the whole construct is dropped, alt text included).
 *
 * This is a courtesy for the editor, not part of the two-renderer contract: it
 * only decides whether to *say* that an image was dropped, so a customer aiming
 * for artwork on a 1-bit back is told instead of shown a gap. Deliberately a
 * loose match — a false positive costs one sentence, a false negative costs the
 * customer their picture without a word.
 */
export function hasImageSyntax(markdown: string): boolean {
  return /!\[[^\]]*\]\(/.test(markdown)
}
