// The markdown back's editing aides.
//
// Nothing here renders a back. Every back — the uploaded picture, a copy of the
// front, and the markdown text alike — is the server's `display/back` artifact,
// produced by `server/src/render.rs` at the ribbon's colour depth and shown by
// the pane as exactly those bytes. There is one renderer, and it is not this
// module: a back drawn here could promise a card the printer would not produce.
//
// The markdown renderer on that side is still a stub, so a markdown-backed card
// reports itself not printable rather than coming out with a blank side. The
// subset it will render — headings, bold, italic, lists; CommonMark only, with
// links as plain text, images refused outright and raw HTML dropped whole — is
// documented with it in TODO.md.
//
// What stays client-side is only what the *editor* has to say about the typed
// text, which is not a rendering of the card at all.

/**
 * Whether the markdown asks for an image, which the renderer refuses outright
 * (the whole construct is dropped, alt text included).
 *
 * A courtesy for the back-text editor, not part of any rendering contract: it
 * only decides whether to *say* that an image was dropped, so a customer aiming
 * for artwork on a 1-bit back is told instead of left with a gap where it went.
 * Deliberately a loose match — a false positive costs one sentence, a false
 * negative costs the customer their picture without a word.
 */
export function hasImageSyntax(markdown: string): boolean {
  return /!\[[^\]]*\]\(/.test(markdown)
}
