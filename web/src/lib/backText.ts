import { Marked } from 'marked'
import DOMPurify from 'dompurify'

// Render `back_text` (markdown) into the black-only card back.
//
// This is the SPA half of a two-renderer contract. The server-side renderer
// the print queue uses (server/src/backtext.rs) MUST produce the same output
// for the same input — the card the customer approves here is the card that
// gets printed. Keep the two in lockstep; the subset below is the agreement.
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
//   - raw HTML is stripped, never passed through.
//
// The output is sanitised with DOMPurify against an allowlist of exactly the
// subset tags, so `{@html}` in the component is safe to use on it.

const ALLOWED_TAGS = [
  'h1', 'h2', 'h3', 'h4', 'h5', 'h6',
  'p', 'strong', 'em', 'ul', 'ol', 'li', 'br',
]

const marked = new Marked({
  // Sync only: the renderer is pure string-in/string-out, no async extensions.
  async: false,
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
  },
})

/** Render markdown `back_text` to sanitised HTML for the card back. */
export function renderBackText(markdown: string): string {
  const html = marked.parse(markdown, { async: false })
  return DOMPurify.sanitize(html, { ALLOWED_TAGS, ALLOWED_ATTR: [] })
}
