//! Server-side render of `back_text` (markdown) into the black-only card back.
//!
//! This is the server half of a two-renderer contract. The SPA half is
//! `web/src/lib/backText.ts` (marked + DOMPurify), which renders the preview the
//! customer approves. The print queue uses *this* renderer, and the two MUST
//! produce the same card: what the customer approved is what gets printed.
//! Keep them in lockstep; the subset below is the agreement, and each side has
//! a test over the same inputs (the SPA's in `web/test/smoke.mjs`, here in the
//! module tests).
//!
//! Supported subset (everything else degrades to its text content):
//!   - headings: `#`..`######`  -> `<h1>`..`<h6>`
//!   - bold:     `**x**`/`__x__` -> `<strong>`
//!   - italic:   `*x*`/`_x_`     -> `<em>`
//!   - lists:    `-`/`*`/`+` and `1.` -> `<ul>`/`<ol>`/`<li>`, nested allowed
//!   - paragraphs and line breaks
//!
//! Deliberate refusals:
//!   - links are rendered as their plain text — there is nothing to click on a
//!     card, so no `<a>` is ever emitted (and a `javascript:` href is inert);
//!   - images are REFUSED outright: the whole construct is dropped, alt text
//!     included, leaving at most an empty `<p>`;
//!   - raw HTML is dropped outright, never passed through. (The SPA's DOMPurify
//!     keeps the *text* of a stripped element; here the markup is dropped whole.
//!     Both are safe — nothing from the markdown is ever treated as markup — and
//!     a card back has no business containing raw HTML.)
//!
//! The output is built tag by tag from an allowlist, so it is safe to treat as
//! HTML: no attribute, no tag and no text outside the subset is ever emitted,
//! and all text is escaped. It is not byte-identical to the SPA's output —
//! whitespace between tags differs, and HTML ignores that — but the tag
//! structure and the text are the same, which is what the print cares about.

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// Escape text for HTML. Matches the entities `marked` emits, so the two
/// renderers agree on the (decoded) text as well as on the tags.
fn escape(text: &str, out: &mut String) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// Render markdown `back_text` into the sanitised HTML subset of the card back.
///
/// The result is HTML for the black panel: the spooler still has to rasterise
/// it to the printer's 1-bit K plane. What this guarantees is that the markup
/// and text are exactly what the SPA preview showed.
pub fn render_back_text(markdown: &str) -> String {
    let mut out = String::new();
    // Images are refused as a whole construct, alt text included, so their
    // children (the alt text) must be skipped rather than rendered. Nested
    // images do not occur, but counting keeps the skip correct anyway.
    let mut image_depth = 0usize;

    for event in Parser::new_ext(markdown, Options::empty()) {
        if image_depth > 0 {
            match event {
                Event::Start(Tag::Image { .. }) => image_depth += 1,
                Event::End(TagEnd::Image) => image_depth -= 1,
                _ => {}
            }
            continue;
        }

        match event {
            Event::Start(Tag::Paragraph) => out.push_str("<p>"),
            Event::End(TagEnd::Paragraph) => out.push_str("</p>\n"),
            Event::Start(Tag::Heading { level, .. }) => {
                out.push_str(&format!("<h{}>", heading_level(level)))
            }
            Event::End(TagEnd::Heading(level)) => {
                out.push_str(&format!("</h{}>\n", heading_level(level)))
            }
            Event::Start(Tag::Strong) => out.push_str("<strong>"),
            Event::End(TagEnd::Strong) => out.push_str("</strong>"),
            Event::Start(Tag::Emphasis) => out.push_str("<em>"),
            Event::End(TagEnd::Emphasis) => out.push_str("</em>"),
            Event::Start(Tag::List(None)) => out.push_str("<ul>\n"),
            Event::Start(Tag::List(Some(_))) => out.push_str("<ol>\n"),
            Event::End(TagEnd::List(false)) => out.push_str("</ul>\n"),
            Event::End(TagEnd::List(true)) => out.push_str("</ol>\n"),
            Event::Start(Tag::Item) => out.push_str("<li>"),
            Event::End(TagEnd::Item) => out.push_str("</li>\n"),
            Event::Text(text) => escape(&text, &mut out),
            // Inline code has no <code> in the subset; DOMPurify strips the tag
            // on the SPA side and keeps the text, so keep exactly the text.
            Event::Code(text) => escape(&text, &mut out),
            Event::SoftBreak => out.push('\n'),
            Event::HardBreak => out.push_str("<br>\n"),
            Event::Start(Tag::Image { .. }) => image_depth += 1,
            // Everything else — links, block quotes, code blocks, rules, raw
            // HTML — emits nothing at all. For a link or a quote the inner
            // events still flow, so the text survives and only the tag is
            // dropped: exactly what the sanitiser does to the SPA's output.
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same inputs the SPA smoke test feeds its renderer, so the two
    /// renderers are pinned to the same cases.
    #[test]
    fn renders_the_agreed_subset() {
        assert_eq!(render_back_text("# Title\n"), "<h1>Title</h1>\n");
        assert_eq!(render_back_text("###### Six\n"), "<h6>Six</h6>\n");
        assert_eq!(
            render_back_text("**bold** and *italic*\n"),
            "<p><strong>bold</strong> and <em>italic</em></p>\n"
        );
        assert_eq!(
            render_back_text("## **Bold** heading\n"),
            "<h2><strong>Bold</strong> heading</h2>\n"
        );
        assert_eq!(
            render_back_text("- one\n- two\n"),
            "<ul>\n<li>one</li>\n<li>two</li>\n</ul>\n"
        );
        assert_eq!(
            render_back_text("1. a\n2. b\n"),
            "<ol>\n<li>a</li>\n<li>b</li>\n</ol>\n"
        );
        assert_eq!(
            render_back_text("- a\n  - b\n"),
            "<ul>\n<li>a<ul>\n<li>b</li>\n</ul>\n</li>\n</ul>\n"
        );
        assert_eq!(render_back_text("one\n\ntwo\n"), "<p>one</p>\n<p>two</p>\n");
        // A soft break stays a newline inside the paragraph; a hard break
        // (two trailing spaces) is a <br>.
        assert_eq!(render_back_text("a\nb\n"), "<p>a\nb</p>\n");
        assert_eq!(render_back_text("a  \nb\n"), "<p>a<br>\nb</p>\n");
    }

    #[test]
    fn links_render_as_plain_text() {
        let html = render_back_text("[link](https://example.com)\n");
        assert_eq!(html, "<p>link</p>\n");
        assert!(!html.contains("<a"), "{html}");
        // A javascript: href has nothing to attach to.
        let html = render_back_text("[click](javascript:alert(1))\n");
        assert_eq!(html, "<p>click</p>\n");
        // Formatting inside a link survives; only the anchor is dropped.
        assert_eq!(
            render_back_text("[**b**](https://x)\n"),
            "<p><strong>b</strong></p>\n"
        );
    }

    #[test]
    fn images_are_refused_whole() {
        // The construct is dropped: no <img>, no alt text, no placeholder.
        let html = render_back_text("![alt text](https://example.com/x.png)\n");
        assert!(!html.contains("<img"), "{html}");
        assert!(!html.contains("alt text"), "{html}");
        // An inline image in a sentence leaves the sentence intact.
        assert_eq!(
            render_back_text("before ![alt](x.png) after\n"),
            "<p>before  after</p>\n"
        );
    }

    #[test]
    fn raw_html_is_never_passed_through() {
        let html = render_back_text("<script>alert(1)</script>\n");
        assert!(!html.contains("<script"), "{html}");
        assert!(!html.contains("alert(1)"), "{html}");
        assert_eq!(html, "");
        // Text is escaped, so a stray '<' cannot open a tag.
        assert_eq!(
            render_back_text("a < b & c > d\n"),
            "<p>a &lt; b &amp; c &gt; d</p>\n"
        );
    }

    #[test]
    fn constructs_outside_the_subset_degrade_to_their_text() {
        // DOMPurify strips the wrapper tags on the SPA side and keeps the text;
        // do the same here.
        assert_eq!(render_back_text("> quoted\n"), "<p>quoted</p>\n");
        assert_eq!(render_back_text("a `code` b\n"), "<p>a code b</p>\n");
        assert_eq!(render_back_text("```\nfn x() {}\n```\n"), "fn x() {}\n");
        assert_eq!(render_back_text("a\n\n---\n\nb\n"), "<p>a</p>\n<p>b</p>\n");
    }

    #[test]
    fn empty_input_renders_nothing() {
        assert_eq!(render_back_text(""), "");
    }
}
