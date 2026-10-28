// Smoke test: mount the *built* SPA bundle in jsdom against a live server.
//
// Usage:  ADMIN_TOKEN=<token> node test/smoke.mjs [baseUrl]
// The server must be running, started with the same ADMIN_TOKEN, and serving
// server/static (the vite build output).
//
// Note for a *debug* server: every card write re-renders the print rasters
// synchronously, and an unoptimised Lanczos resize to the 2032x1296 default
// takes seconds. Run it with a small `PRINT_RASTER_WIDTH`/`PRINT_RASTER_HEIGHT`
// (the size is server config anyway), or build in release.
//
// Why jsdom: the browser tooling refuses private/loopback addresses, and this
// exercises the real compiled bundle (runes, router, fetch wiring) rather than
// a source-level mock.

import { JSDOM } from 'jsdom'
import { readFileSync, readdirSync } from 'node:fs'
import { createRequire } from 'node:module'
import { fileURLToPath } from 'node:url'

// The bundle has no ESM syntax, so Node loads it as CommonJS. We require it
// per scenario and evict the cache to force a fresh execution (dynamic import
// with a query string does NOT re-run a CJS module).
const require = createRequire(import.meta.url)

const BASE = process.argv[2] ?? 'http://127.0.0.1:8099'
const realFetch = globalThis.fetch // Node's fetch, before we shim it per-scenario
// The admin session cookie the bundle's own login sets. undici keeps no cookie
// jar, so without this the session would never be replayed on later requests.
let cookieJar = ''
// Mutations the bundle made, so the CSRF header can be asserted on.
const seen = []
// Node's own FormData/Blob/File — the per-scenario globals are jsdom's, which
// undici (Node fetch) cannot serialize into a multipart body.
const NodeFormData = globalThis.FormData
const NodeBlob = globalThis.Blob
const staticDir = new URL('../../server/static/', import.meta.url)
const bundleName = readdirSync(new URL('assets/', staticDir)).find((f) => f.endsWith('.js'))
if (!bundleName) throw new Error('no JS bundle in server/static/assets — run `npm run build`')
const bundlePath = fileURLToPath(new URL(`assets/${bundleName}`, staticDir))

const PNG = readFileSync(fileURLToPath(new URL('../../scripts/test-card.png', import.meta.url)))

let failures = 0
function check(name, cond, detail = '') {
  const mark = cond ? 'ok  ' : 'FAIL'
  if (!cond) failures++
  console.log(`  ${mark} ${name}${detail ? ` — ${detail}` : ''}`)
}

/** The bit depth of a card's back print raster, straight off the endpoint the
 * spooler reads — the one thing the ribbon is allowed to change about a card.
 * Returns the HTTP status as a string when the raster is not ready. */
async function rasterBits(prefix, id = 1) {
  const res = await realFetch(`${BASE}/api/prefixes/${prefix}/cards/${id}/print-raster/back`)
  if (!res.ok) return `HTTP ${res.status}`
  const buf = Buffer.from(await res.arrayBuffer())
  // biBitCount in the BITMAPINFOHEADER at offset 28.
  return buf.readUInt16LE(28)
}

/** Canonical structural form of an HTML fragment: tags by name, text with
 * whitespace collapsed. Insignificant whitespace between tags is dropped, so
 * the SPA renderer and the server renderer are compared on the tag structure
 * and the text, which is what the card is, not on pretty-printing. */
function canonHtml(html) {
  const host = document.createElement('div')
  host.innerHTML = html
  const out = []
  const walk = (n) => {
    for (const c of n.childNodes) {
      if (c.nodeType === 3) {
        const t = c.textContent.replace(/\s+/g, ' ').trim()
        if (t) out.push(`#${t}`)
      } else if (c.nodeType === 1) {
        const tag = c.tagName.toLowerCase()
        out.push(`<${tag}>`)
        walk(c)
        out.push(`</${tag}>`)
      }
    }
  }
  walk(host)
  return out.join(' ')
}

/** Mount the bundle in a fresh jsdom at `path` (busts the module cache each time). */
async function mount(path, { bust }) {
  const dom = new JSDOM('<!doctype html><html><body><div id="app"></div></body></html>', {
    url: BASE + path,
    pretendToBeVisual: true,
  })
  const { window } = dom
  for (const k of [
    'window', 'document', 'navigator', 'location', 'history', 'localStorage',
    'Element', 'HTMLElement', 'HTMLInputElement', 'HTMLTextAreaElement',
    'HTMLMediaElement', 'SVGSVGElement', 'Node', 'Text', 'Comment', 'DocumentFragment',
    'DOMTokenList', 'CustomEvent', 'Event',
    // NB: deliberately NOT FormData/File/Blob — undici only recognises its own,
    // and the bundle only touches them inside the (untested here) upload path.
    'MutationObserver', 'ResizeObserver', 'getComputedStyle', 'matchMedia',
    'requestAnimationFrame', 'cancelAnimationFrame',
  ]) {
    if (window[k] === undefined) continue
    // navigator (and friends) are getter-only on Node's globalThis.
    Object.defineProperty(globalThis, k, { value: window[k], writable: true, configurable: true })
  }
  // jsdom's confirm is unimplemented; the admin UI asks before deleting.
  Object.defineProperty(globalThis, 'confirm', {
    value: () => true,
    writable: true,
    configurable: true,
  })

  // The bundle calls fetch('/api/…'); Node's fetch needs an absolute URL.
  // jsdom does not implement fetch, so keep a handle on Node's. Beyond URL
  // resolution this shim replays the session cookie and records mutations, so
  // the admin login and the CSRF header are exercised for real.
  globalThis.fetch = async (input, init) => {
    const url = typeof input === 'string' ? new URL(input, BASE).href : input
    const opts = { ...(init ?? {}) }
    const method = (opts.method ?? 'GET').toUpperCase()

    const headers = new Headers(opts.headers ?? {})
    if (cookieJar && !headers.has('cookie')) headers.set('cookie', cookieJar)
    opts.headers = headers
    if (method !== 'GET')
      seen.push({ method, csrf: headers.get('x-esplp-csrf'), url: String(url) })

    const res = await realFetch(url, opts)

    const setCookie = res.headers.get('set-cookie')
    if (setCookie) {
      const m = setCookie.match(/esplp_admin=([^;]*)/)
      if (m) cookieJar = m[1] ? `esplp_admin=${m[1]}` : ''
    }
    return res
  }

  delete require.cache[require.resolve(bundlePath)]
  require(bundlePath)
  await settle()
  return window
}

/** Let promise callbacks and Svelte's microtask flush settle. */
const settle = (ms = 1200) => new Promise((r) => setTimeout(r, ms))

/**
 * Poll until `pred` holds, or give up after `ms`. Returns whether it held.
 *
 * A fixed sleep is a guess at how long a round trip takes; the checks that
 * follow a write are really asking "did the row catch up", so they wait for
 * that instead of hoping.
 */
async function waitFor(pred, ms = 8000) {
  const deadline = Date.now() + ms
  for (;;) {
    try {
      if (await pred()) return true
    } catch {
      /* a fetch that is not there yet: keep waiting */
    }
    if (Date.now() > deadline) return false
    await settle(150)
  }
}

/** Wait for `sel` to appear under `root`, and return it (null if it never does). */
async function waitForEl(root, sel, ms = 8000) {
  const deadline = Date.now() + ms
  for (;;) {
    const el = root.querySelector(sel)
    if (el) return el
    if (Date.now() > deadline) return null
    await settle(150)
  }
}

/** The editing surface of a card's markdown editor: the ProseMirror document. */
const mdSurface = (w) => w.document.querySelector('.md-editor .ProseMirror')

/** Whether the markdown editor is editable — it is held read-only while a write
 * of the card is still in flight. */
const mdEditable = (w) => mdSurface(w)?.getAttribute('contenteditable') === 'true'

/**
 * Put markdown into the editor the way a customer would: paste it.
 *
 * jsdom cannot type into a contenteditable — a browser synthesises
 * beforeinput/input events for that and jsdom does not — but a paste is an
 * ordinary DOM event carrying the text the editor parses itself. It is also the
 * gesture a customer actually uses to get a song list onto a card.
 */
function pasteIntoEditor(w, markdown) {
  const surface = mdSurface(w)
  if (!surface) return false
  const ev = new w.Event('paste', { bubbles: true, cancelable: true })
  Object.defineProperty(ev, 'clipboardData', {
    value: {
      types: ['text/plain'],
      files: [],
      getData: (t) => (t === 'text/plain' ? markdown : ''),
    },
    configurable: true,
  })
  surface.dispatchEvent(ev)
  return true
}

const api = async (path, init) => {
  const res = await fetch(BASE + path, init)
  const text = await res.text()
  if (!res.ok) throw new Error(`${path} -> ${res.status} ${text}`)
  return text ? JSON.parse(text) : undefined
}

const TOKEN = process.env.ADMIN_TOKEN
if (!TOKEN) {
  console.error('set ADMIN_TOKEN to the token the server was started with')
  process.exit(2)
}

/** PATCH a prefix status. Returns the HTTP status code. */
async function setStatus(prefix, status, { token = TOKEN } = {}) {
  const headers = { 'content-type': 'application/json' }
  if (token) headers.authorization = `Bearer ${token}`
  const res = await realFetch(`${BASE}/api/prefixes/${prefix}/status`, {
    method: 'PATCH',
    headers,
    body: JSON.stringify({ status }),
  })
  return res.status
}

/** GET /api/prefixes with an optional token. Returns the HTTP status code. */
async function listPrefixes(token) {
  const headers = token ? { authorization: `Bearer ${token}` } : {}
  return (await realFetch(`${BASE}/api/prefixes`, { headers })).status
}

console.log(`bundle: ${bundleName}\nbase:   ${BASE}\n`)

// ---- scenario A: landing auto-creates a project and redirects --------------
// What the browser is allowed to keep of the served frontend. Every file in a
// Nix store path carries the same mtime, so a `Last-Modified`-based answer is a
// lie: the shell has to be uncacheable and never conditionally answered, the
// content-hashed assets keepable, and an asset that is not there a 404 — never
// the shell, which a tab holding an old hash would run as if it were a module.
console.log('\n0. what the browser may keep of the frontend')
{
  const shell = await realFetch(`${BASE}/`)
  check('the shell is not cacheable', shell.headers.get('cache-control') === 'no-store')
  check('the shell carries no date to revalidate against', shell.headers.get('last-modified') === null)
  const html = await shell.text()
  const asset = html.match(/\/assets\/[\w.-]+\.js/)?.[0]
  check('the shell points at a bundle', !!asset)

  const cond = await realFetch(`${BASE}/`, {
    headers: { 'if-modified-since': 'Thu, 01 Jan 1970 00:00:01 GMT' },
  })
  check('a visitor with an old copy still gets the shell', cond.status === 200)

  const js = await realFetch(`${BASE}${asset}`)
  check('the bundle is served immutable', /immutable/.test(js.headers.get('cache-control') ?? ''))

  const stale = await realFetch(`${BASE}/assets/index-DEADBEEF.js`)
  check(
    'a missing asset is a 404, not the shell',
    stale.status === 404 && !/html/.test(stale.headers.get('content-type') ?? ''),
  )
}

console.log('A. landing page redirects into a new project')
{
  const w = await mount('/', { bust: 'a' })
  const p = w.location.pathname
  check('redirected to /p/<uuid>', /^\/p\/[0-9a-f-]{36}$/.test(p), p)
  const text = w.document.body.textContent ?? ''
  check('renders the project page', text.includes('Project') && text.includes('Cards'))
  check('renders the upload dropzone', !!w.document.querySelector('.dropzone'))
  // Regression: a file input inside .dropzone gets its click bubbled to the
  // dropzone handler, which opens the wrong picker.
  check('file inputs are outside the dropzone', !w.document.querySelector('.dropzone input[type=file]'))
  check('has a folder input with webkitdirectory',
    w.document.querySelector('input[webkitdirectory]') !== null)
  check('shows draft status badge', w.document.querySelector('.badge')?.textContent?.trim() === 'draft')
  check('project exists on the server', !!(await api(`/api/prefixes/${p.slice(3)}`)))
}

// ---- scenario B: a seeded card renders with front/back previews ------------
console.log('\nB. project page renders a card')
{
  const prefix = (
    await api('/api/prefixes', { method: 'POST', headers: { 'content-type': 'application/json' }, body: '{}' })
  ).uuid

  const fd = new NodeFormData()
  fd.append('name', 'Sample Card')
  fd.append('serial_number', 'SN-0001')
  // The back is chosen explicitly; text only means anything on a markdown back.
  fd.append('back', 'markdown')
  fd.append('back_text', '01 Song One\n02 Song Two')
  fd.append('image', new NodeBlob([PNG], { type: 'image/png' }), 'sample.png')
  await api(`/api/prefixes/${prefix}/cards`, { method: 'POST', body: fd })

  const w = await mount(`/p/${prefix}`, { bust: 'b' })
  const text = w.document.body.textContent ?? ''
  // NB: input values are DOM properties, not text content.
  check('name input holds the name', w.document.querySelector('input#name-1')?.value === 'Sample Card')
  // The serial number is an API-level field: it is not surfaced in the UI at all.
  check('no serial input in the UI', !w.document.querySelector('input#serial-1'))
  // The back text is edited as the formatting it will have — the markdown is
  // what leaves the editor, and it is what the backend renders the back from.
  check('the markdown editor holds the text',
    /01 Song One/.test(mdSurface(w)?.textContent ?? ''),
    mdSurface(w)?.textContent ?? '')
  // A WYSIWYG editor, not a plain box: the toolbar is what applies the
  // constructs, and the value it hands back is markdown.
  check('the editor offers the formatting controls',
    w.document.querySelectorAll('.md-editor .md-toolbar button').length >= 6,
    String(w.document.querySelectorAll('.md-editor .md-toolbar button').length))
  // The pane is the server's artifact for the back, whatever the back source is:
  // the row never draws a back itself. This card's back is markdown, which the
  // server renders into a picture like any other back — so the pane shows that
  // picture, not the song list laid out in the browser.
  {
    const pane = w.document.querySelector('.preview.back')
    check('the back pane draws nothing from the text itself',
      !pane?.querySelector('.panel') && !/02 Song Two/.test(pane?.innerHTML ?? ''),
      pane?.innerHTML ?? '')
    check('the back pane shows the server-rendered markdown back',
      !!pane?.querySelector(`img[src^="/api/prefixes/${prefix}/cards/1/display/back"]`),
      pane?.innerHTML ?? '')
  }
  // The preview shows the *processed* image the server derived, not the raw
  // upload — one renderer owns what a card looks like.
  check('front preview points at the display/front endpoint',
    w.document.querySelector(`img[src^="/api/prefixes/${prefix}/cards/1/display/front"]`) !== null)
  check('renders a back preview pane', !!w.document.querySelector('.preview.back'))
  check('the back choice is marked on the card',
    w.document.querySelector('.back-choice.selected strong')?.textContent?.trim() === 'Text')
  check('renders a delete button', /Delete/.test(text))
  check('shows 1 card', text.includes('1 card'))
  check('name input is editable', !w.document.querySelector('input#name-1')?.disabled)

  // ---- scenario B2: a markdown back is the server's to render --------------
  // `back_text` is markdown, and markdown is just another back *source* — so the
  // pane treats it like every other back: the server's `display/back` artifact,
  // nothing else. What that artifact holds is the *backend's* layout of the text,
  // so the card carries a picture rather than the SPA drawing a preview of its own
  // that no printer would reproduce. The text is stored exactly as typed.
  console.log('\nB2. a markdown back is the server\'s to render')
  {
    const mdPrefix = (
      await api('/api/prefixes', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        // Explicitly the BLACK-BACK ribbon: this scenario is about what the
        // black panel does with a rendered document (1-bit, a `black` back in
        // the queue, one billed card). The default is the colour one.
        body: JSON.stringify({ ribbon: 'ymckok' }),
      })
    ).uuid

    const md = [
      '# Title',
      '',
      '**bold** and *italic*',
      '',
      '- one',
      '- two',
      '',
      '[link](https://example.com)',
      '',
      // A strikethrough is GFM, which the backend lays out like anything else —
      // there is no list of constructs here that is parsed and constructs that
      // are not.
      '~~struck~~',
    ].join('\n')
    const fd = new NodeFormData()
    fd.append('name', 'MD Card')
    fd.append('back', 'markdown')
    fd.append('back_text', md)
    fd.append('image', new NodeBlob([PNG], { type: 'image/png' }), 'md.png')
    await api(`/api/prefixes/${mdPrefix}/cards`, { method: 'POST', body: fd })

    const w = await mount(`/p/${mdPrefix}`, { bust: 'b2' })
    const pane = w.document.querySelector('.preview.back')
    // What the customer typed reached the server...
    // ...modulo line endings. A multipart text field carries its newlines as
    // CRLF, whatever the sender's convention — that is the transport's, not the
    // author's. Markdown reads CR, LF and CRLF alike, and nothing here renders
    // or hashes the text, so the only thing that must match is the text itself.
    // Compare on a canonical line ending rather than on the wire bytes.
    const canonical = (s) => (s ?? '').replace(/\r\n?/g, '\n')
    check('the text is stored on the card',
      canonical((await api(`/api/prefixes/${mdPrefix}/cards/1`)).back_text) === canonical(md))
    // ...and the pane draws no card of its own: no client-side markdown, no
    // panel of its own markup, just the server's artifact.
    // The typed markdown is not rendered here: no heading, no list, nothing the
    // browser drew from `back_text`.
    check('the pane renders no markup from the text',
      !/<(h1|h2|strong|em|ul|ol|li)\b/.test(pane?.innerHTML ?? ''), pane?.innerHTML ?? '')
    check('the pane shows the server\'s render of the text',
      !!pane?.querySelector(`img[src^="/api/prefixes/${mdPrefix}/cards/1/display/back"]`),
      pane?.innerHTML ?? '')
    check('the card is not reported unprintable',
      !/not printable yet/i.test(pane?.textContent ?? ''), (pane?.textContent ?? '').trim())
    // NB: the per-card caption that used to name the print depth here was
    // removed on purpose ("Tone down the site verbosity"): the depth is named
    // once, on the ribbon choice, and not repeated on every card.
    // What prints is a raster from that same picture, at the ribbon's depth.
    check('the markdown back rasterises to 1 bit on this ribbon',
      (await rasterBits(mdPrefix)) === 1, String(await rasterBits(mdPrefix)))

    // The print queue carries no rendering: the spooler prints the server's
    // raster. What the queue does carry is what the card's back *is*.
    const queue = await api(`/api/prefixes/${mdPrefix}/print-queue`, {
      headers: { authorization: `Bearer ${TOKEN}` },
    })
    check('the queue reports the black back', queue.cards[0]?.back === 'black',
      JSON.stringify(queue.cards[0]))
    check('the queue reports the ribbon to load', queue.ribbon === 'ymckok',
      String(queue.ribbon))
    check('a YMCKOK card is billed as one card, whatever its back',
      queue.cards[0]?.billed_units === 1, String(queue.cards[0]?.billed_units))
  }

  // ---- scenario C: the auth boundary, then a frozen project ---------------
  console.log('\nC. auth boundary and the print freeze')
  check('list prefixes without a token -> 401', (await listPrefixes('')) === 401)
  check('list prefixes with a wrong token -> 401', (await listPrefixes('nope')) === 401)
  check('list prefixes with the admin token -> 200', (await listPrefixes(TOKEN)) === 200)
  check('set status without a token -> 401',
    (await setStatus(prefix, 'payment_pending', { token: '' })) === 401)

  // The customer's write window closes the moment the prefix leaves draft:
  // payment_pending is already read-only for them (only the operator can still
  // edit). This is the seal-on-pay behaviour, asserted here so it cannot drift.
  check('operator sets status -> payment_pending', (await setStatus(prefix, 'payment_pending')) === 200)
  const wSealed = await mount(`/p/${prefix}`, { bust: 'c1' })
  const textSealed = wSealed.document.body.textContent ?? ''
  check('sealed at payment_pending: no upload dropzone', !wSealed.document.querySelector('.dropzone'))
  check('sealed at payment_pending: says it can no longer be changed', /can no longer be changed/.test(textSealed))
  check('sealed at payment_pending: name input disabled', wSealed.document.querySelector('input#name-1')?.disabled === true)

  for (const s of ['paid', 'queued', 'printing']) {
    check(`operator sets status -> ${s}`, (await setStatus(prefix, s)) === 200)
  }
  const w2 = await mount(`/p/${prefix}`, { bust: 'c' })
  const text2 = w2.document.body.textContent ?? ''
  check('shows the printing badge', w2.document.querySelector('.badge')?.textContent?.trim() === 'printing')
  check('no upload dropzone', !w2.document.querySelector('.dropzone'))
  check('says it can no longer be changed', /can no longer be changed/.test(text2))
  check('name input disabled', w2.document.querySelector('input#name-1')?.disabled === true)

  // ---- scenario D: 404 for an unknown project -----------------------------
  console.log('\nD. unknown project')
  const w3 = await mount('/p/00000000-0000-0000-0000-000000000000', { bust: 'd' })
  check('shows an error', /404/.test(w3.document.body.textContent ?? ''))
}

// ---- scenario E: the admin route asks for the token first -----------------
console.log('\nE. admin login page')
{
  const w = await mount('/admin', { bust: 'e' })
  const text = w.document.body.textContent ?? ''
  check('admin route renders the login form', !!w.document.querySelector('input#admin-token'))
  check('token field is a password input',
    w.document.querySelector('input#admin-token')?.type === 'password')
  check('offers a log in button', /Log in/.test(text))
  check('no prefix list before logging in', !w.document.querySelector('.prow'))
}

// ---- scenario F: the admin UI — list, status, print preview, delete -------
console.log('\nF. admin: list, status change, print preview, delete')
{
  const prefix = (
    await api('/api/prefixes', { method: 'POST', headers: { 'content-type': 'application/json' }, body: '{}' })
  ).uuid

  const fd = new NodeFormData()
  fd.append('name', 'Admin Preview Card')
  fd.append('serial_number', 'ADM-1')
  fd.append('back', 'markdown')
  fd.append('back_text', '01 Admin Song')
  fd.append('image', new NodeBlob([PNG], { type: 'image/png' }), 'admin.png')
  await api(`/api/prefixes/${prefix}/cards`, { method: 'POST', body: fd })

  // Log in through the real form, so the cookie and CSRF wiring are exercised.
  const w = await mount('/admin', { bust: 'f1' })
  const tok = w.document.querySelector('#admin-token')
  tok.value = TOKEN
  tok.dispatchEvent(new w.Event('input', { bubbles: true }))
  await settle(150)
  w.document.querySelector('form').dispatchEvent(
    new w.Event('submit', { bubbles: true, cancelable: true }),
  )
  await settle(1500)

  const text = w.document.body.textContent ?? ''
  check('logged in, prefix list renders', !!w.document.querySelector('.prow'))
  check('the seeded prefix is listed', text.includes(prefix))
  check('row has a status select', !!w.document.querySelector(`select#status-${prefix}`))
  check('login POST carried the CSRF header',
    seen.some((r) => r.method === 'POST' && r.csrf))

  // Change a status straight from the row.
  const sel = w.document.querySelector(`select#status-${prefix}`)
  sel.value = 'paid'
  sel.dispatchEvent(new w.Event('change', { bubbles: true }))
  await settle(1500)
  const after = await api(`/api/prefixes/${prefix}`)
  check('status change took effect', after.status === 'paid', after.status)
  check('status PATCH carried the CSRF header',
    seen.some((r) => r.method === 'PATCH' && r.csrf))

  // Print preview: read-only, no card editor at all.
  const w2 = await mount(`/admin/p/${prefix}`, { bust: 'f2' })
  await settle(1500)
  const t2 = w2.document.body.textContent ?? ''
  check('print preview renders the card', !!w2.document.querySelector('.card.preview-only'))
  check('preview shows the front image',
    !!w2.document.querySelector(`img[src^="/api/prefixes/${prefix}/cards/1/display/front"]`))
  // The back panel is the same single artifact the customer's pane shows, so the
  // operator previews what prints — for this card a markdown back, which the
  // server rendered from the card's text.
  check('preview shows the back panel',
    !!w2.document.querySelector('.preview.back'))
  check('the previewed back is the server\'s render of the text',
    !!w2.document.querySelector(`img[src^="/api/prefixes/${prefix}/cards/1/display/back"]`) &&
      !w2.document.querySelector('.preview.back .panel'),
    (w2.document.querySelector('.preview.back')?.innerHTML ?? '').slice(0, 200))
  check('preview shows the name as text', t2.includes('Admin Preview Card'))
  check('preview has no card editor inputs', !w2.document.querySelector('input#name-1'))
  check('preview has no upload dropzone', !w2.document.querySelector('.dropzone'))
  check('preview offers no way to add cards', !/drop some images|upload/i.test(t2))
  check('has a prefix delete button', !!w2.document.querySelector('button.danger'))

  // A card carried over from an earlier run, marked printed. Queuing the prefix
  // has to clear that mark — `queued` is the reprint lever, not a resume.
  const queueOf = () =>
    realFetch(`${BASE}/api/prefixes/${prefix}/print-queue`, {
      headers: { authorization: `Bearer ${TOKEN}` },
    }).then((r) => r.json())
  const marked = await realFetch(`${BASE}/api/prefixes/${prefix}/cards/1/printed`, {
    method: 'POST',
    headers: { authorization: `Bearer ${TOKEN}` },
  })
  check('a card can be marked printed', marked.status === 200, String(marked.status))
  check('the queue reports it printed',
    await waitFor(async () => (await queueOf()).unprinted_count === 0),
    JSON.stringify((await queueOf()).unprinted_count))

  // The detail form also carries the order reference.
  const st = w2.document.querySelector('#admin-status')
  st.value = 'queued'
  st.dispatchEvent(new w2.Event('change', { bubbles: true }))
  const ref = w2.document.querySelector('#admin-order-ref')
  ref.value = 'ADM-9'
  ref.dispatchEvent(new w2.Event('input', { bubbles: true }))
  await settle(150)
  ;[...w2.document.querySelectorAll('button')].find((b) => /Apply/.test(b.textContent)).click()
  await settle(1500)
  const after2 = await api(`/api/prefixes/${prefix}`)
  check('status set from the detail page', after2.status === 'queued', after2.status)
  check('order reference saved alongside it', after2.order_ref === 'ADM-9', String(after2.order_ref))
  // ...and the card that was marked printed is back in the run.
  const q = await queueOf()
  check('queuing cleared the card\'s printed mark',
    q.cards[0]?.printed_at === null && q.unprinted_count === 1,
    JSON.stringify(q.cards[0]))
  check('a fresh queue is not a resume', q.resume === false, String(q.resume))

  // Re-applying `queued` is not a transition, so a mark set since then survives
  // it: a retried callback must not silently reprint a whole order.
  await realFetch(`${BASE}/api/prefixes/${prefix}/cards/1/printed`, {
    method: 'POST',
    headers: { authorization: `Bearer ${TOKEN}` },
  })
  const again = await realFetch(`${BASE}/api/prefixes/${prefix}/status`, {
    method: 'PATCH',
    headers: { 'content-type': 'application/json', authorization: `Bearer ${TOKEN}` },
    body: JSON.stringify({ status: 'queued' }),
  })
  check('re-applying queued is accepted', again.status === 200, String(again.status))
  check('and clears nothing, because it is not a transition',
    await waitFor(async () => (await queueOf()).unprinted_count === 0),
    JSON.stringify((await queueOf()).unprinted_count))

  // Delete the prefix, from the detail page.
  w2.document.querySelector('button.danger').click()
  await settle(1500)
  check('delete returns to the prefix list', w2.location.pathname === '/admin', w2.location.pathname)
  const gone = await realFetch(`${BASE}/api/prefixes/${prefix}`, {
    headers: { authorization: `Bearer ${TOKEN}` },
  })
  check('prefix is gone from the server', gone.status === 404, String(gone.status))
}

// ---- scenario G: the Pay button, straight from a draft ---------------------
// Only meaningful when the server has Stripe configured, so it is skipped on a
// deployment without it — same idea as the admin scenarios and ADMIN_TOKEN.
console.log('\nG. pay button')
{
  // Scenario F logged in, and the shim's jar would otherwise carry that admin
  // session into this customer flow — where a cookie-authenticated mutation
  // needs the CSRF header the test's own api() helper does not send.
  cookieJar = ''

  const shop = await realFetch(`${BASE}/api/shop/config`).then((r) => r.json())
  if (!shop.enabled) {
    console.log('  skip no payment configured on this server (STRIPE_SECRET_KEY unset)')
  } else {
    const prefix = (
      await api('/api/prefixes', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: '{}',
      })
    ).uuid

    // Created with no ribbon at all, so this is the default — and the default is
    // the COLOUR ribbon: YMCKOK is a choice, not a fallback.
    const fresh = await api(`/api/prefixes/${prefix}`)
    check('a new project is on the default ribbon — ymcko',
      fresh.ribbon === 'ymcko', String(fresh.ribbon))

    const fd = new NodeFormData()
    fd.append('name', 'Payable Card')
    fd.append('image', new NodeBlob([PNG], { type: 'image/png' }), 'pay.png')
    await api(`/api/prefixes/${prefix}/cards`, { method: 'POST', body: fd })

    // No submit step any more: a draft is payable straight away, and paying is
    // what makes the order real.
    const w = await mount(`/p/${prefix}`, { bust: 'g1' })
    await settle(1400)
    const text = w.document.body.textContent ?? ''
    check(
      'a draft project offers to pay straight away',
      /Pay now/.test(text),
      text.slice(0, 160),
    )
    check('there is no submit button', !/Submit/.test(text), text.slice(0, 200))
    // On the default (colour) ribbon a single-sided card is ONE printed side at
    // 500 + 250 shipping = 750 minor units. The old always-two rule charged two
    // sides for it and would double the total. Match the digits, not the
    // currency formatting, which depends on the host locale.
    check('the total is shown', /7[.,]50/.test(text), text.slice(0, 300))
    check('a single-sided card is billed as one printed side, not two',
      /1 side ×/.test(text), text.slice(0, 300))

    const before = seen.length
    const payButton = [...w.document.querySelectorAll('button')].find((b) =>
      /Pay now/.test(b.textContent),
    )
    check('the Pay button is a real button', !!payButton)
    payButton?.click()
    await settle(1600)

    const posted = seen.slice(before)
    check(
      'clicking Pay opens a checkout session on the server',
      posted.some(
        (r) => r.method === 'POST' && r.url.includes(`/api/prefixes/${prefix}/checkout`),
      ),
      JSON.stringify(posted),
    )
  }
}

// ---- scenario H: ribbon availability --------------------------------------
// One stock flag per ribbon, and only two ribbons: the ribbon is the one choice
// made per job, because a batch cannot mix ribbon loads. An unavailable ribbon
// must stay *visible* and be disabled — never dropped from the chooser.
console.log('\nH. ribbon availability: disabled, not absent')
{
  cookieJar = ''

  /** Flip a ribbon's stock flag through the real admin API. */
  const setRibbon = async (ribbon, in_stock) => {
    const res = await realFetch(`${BASE}/api/shop/ribbons/${ribbon}`, {
      method: 'PUT',
      headers: { 'content-type': 'application/json', authorization: `Bearer ${TOKEN}` },
      body: JSON.stringify({ in_stock }),
    })
    return res.status
  }
  check('operator can clear a ribbon flag', (await setRibbon('ymcko', false)) === 200)

  const config = await realFetch(`${BASE}/api/shop/config`).then((r) => r.json())
  const availability = Object.fromEntries(config.prices.map((p) => [p.ribbon, p.available]))
  check('the cleared ribbon is out of stock', availability.ymcko === false,
    JSON.stringify(availability))
  check('the other ribbon is untouched', availability.ymckok === true,
    JSON.stringify(availability))
  check('one price row per ribbon', config.prices?.length === 2,
    JSON.stringify(config.prices))
  check('both stock flags are served', config.ribbons?.length === 2,
    JSON.stringify(config.ribbons))

  // A project on the out-of-stock ribbon. It needs a card, or the checkout would
  // refuse it for having nothing to print and the 409 below would prove the
  // wrong thing.
  const prefix = (
    await api('/api/prefixes', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ ribbon: 'ymcko' }),
    })
  ).uuid

  const fd = new NodeFormData()
  fd.append('name', 'Ribbon Card')
  fd.append('image', new NodeBlob([PNG], { type: 'image/png' }), 'ribbon.png')
  await api(`/api/prefixes/${prefix}/cards`, { method: 'POST', body: fd })

  const w = await mount(`/p/${prefix}`, { bust: 'h1' })
  await settle(1200)
  const options = [...w.document.querySelectorAll('.mode')]
  const byRibbon = (ribbon) =>
    options.find((el) => el.querySelector(`input[value="${ribbon}"]`))

  check('every ribbon is still offered, none removed', options.length === 2,
    String(options.length))
  check('an out-of-stock ribbon is rendered disabled',
    byRibbon('ymcko')?.querySelector('input')?.disabled === true)
  check('the in-stock ribbon is selectable',
    byRibbon('ymckok')?.querySelector('input')?.disabled === false)
  check('the disabled option says it is out of stock',
    /out of stock/i.test(byRibbon('ymcko')?.textContent ?? ''))
  check('the unavailable option is marked, not hidden',
    byRibbon('ymcko')?.classList.contains('unavailable') === true)

  // Paying for a ribbon that cannot be printed is refused on both layers: the
  // button here, and a 409 from the server. Both need payment configured — an
  // unconfigured deployment answers 503 before it ever looks at the ribbon.
  if (!config.enabled) {
    console.log('  skip pay refusal: no payment configured (STRIPE_SECRET_KEY unset)')
  } else {
    const payButton = [...w.document.querySelectorAll('button')].find((b) =>
      /Pay now/.test(b.textContent),
    )
    check('the Pay button is disabled while the chosen ribbon is out',
      payButton?.disabled === true)
    const refused = await realFetch(`${BASE}/api/prefixes/${prefix}/checkout`, {
      method: 'POST',
    })
    check('the server refuses the checkout with 409', refused.status === 409,
      String(refused.status))
  }

  // Switching to an available ribbon goes through the real PATCH route.
  const pick = byRibbon('ymckok').querySelector('input')
  pick.checked = true
  pick.dispatchEvent(new w.Event('change', { bubbles: true }))
  await settle(900)
  check('choosing another ribbon PATCHes the prefix',
    seen.some((r) => r.method === 'PATCH' && r.url.includes(`/api/prefixes/${prefix}`)),
    JSON.stringify(seen.slice(-3)))
  const after = await api(`/api/prefixes/${prefix}`)
  check('the ribbon change took effect', after.ribbon === 'ymckok', after.ribbon)

  // Restock: the options come back with no state to reset.
  check('operator can restock', (await setRibbon('ymcko', true)) === 200)
  const w2 = await mount(`/p/${prefix}`, { bust: 'h2' })
  await settle(1200)
  const after2 = [...w2.document.querySelectorAll('.mode input')]
  check('every ribbon is selectable again', after2.every((i) => !i.disabled),
    after2.map((i) => i.disabled).join(','))
}

// ---- scenario I: the per-card back choice ----------------------------------
// The job chooses the ribbon; the *card* chooses what its back is — and the two
// are independent: the same four sources are offered on either ribbon, and the
// only thing the ribbon changes is the colour depth the back is rendered in
// (asserted below from the raster bytes themselves, not from a flag).
console.log('\nI. one panel, the same back choices on either ribbon')
{
  cookieJar = ''

  const makePrefix = async (ribbon) =>
    (
      await api('/api/prefixes', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(ribbon ? { ribbon } : {}),
      })
    ).uuid

  /** Upload a card. Only the front image: a card is created single-sided. */
  const addCard = async (prefix, name = 'Back Card') => {
    const fd = new NodeFormData()
    fd.append('name', name)
    fd.append('image', new NodeBlob([PNG], { type: 'image/png' }), 'front.png')
    return api(`/api/prefixes/${prefix}/cards`, { method: 'POST', body: fd })
  }

  const choice = (w, source) => w.document.querySelector(`.back-choice input[value="${source}"]`)
  const pick = (w, source) => {
    const el = choice(w, source)
    el.checked = true
    el.dispatchEvent(new w.Event('change', { bubbles: true }))
  }

  // --- a new card is single-sided -----------------------------------------
  {
    const prefix = await makePrefix('ymcko')
    const card = await addCard(prefix)
    check('a card is created single-sided', card.back === 'none', String(card.back))

    const w = await mount(`/p/${prefix}`, { bust: 'i1' })
    const t = w.document.body.textContent ?? ''
    check('the panel says the card is single-sided', /single-sided/i.test(t))
    check('no back pane for a single-sided card', !w.document.querySelector('.preview.back'))
    check(
      'no back-image chooser and no text editor yet',
      !w.document.querySelector('input#back-image-1') &&
        !w.document.querySelector('.md-editor'),
    )
    check(
      'the source chooser is in the editor, not the pane',
      !!w.document.querySelector('.side.editor .back-choices input[value="image"]'),
    )
    check(
      'all four back sources are offered',
      ['none', 'front', 'image', 'markdown'].every((s) => !!choice(w, s)),
      [...w.document.querySelectorAll('.back-choice input')].map((i) => i.value).join(','),
    )
  }

  // --- a colour ribbon: the back is its own uploaded image ----------------
  {
    const prefix = await makePrefix('ymcko')
    await addCard(prefix)
    const w = await mount(`/p/${prefix}`, { bust: 'i2' })
    pick(w, 'image')
    // Svelte renders asynchronously, so even the DOM has to be waited for.
    check(
      'the file chooser sits under the pane, not in it',
      (await waitForEl(w.document, 'input#back-image-1[type=file]')) !== null &&
        !w.document.querySelector('.preview.back input#back-image-1[type=file]'),
    )
    check(
      'the pane is a drop area while it has no picture',
      !!w.document.querySelector('.preview.back .back-drop'),
    )
    check('choosing a back writes it to the card',
      await waitFor(async () => (await api(`/api/prefixes/${prefix}/cards/1`)).back === 'image'))
    // The page re-counts what is still missing once the row has written and the
    // response has landed.
    check('the page reports the card as incomplete',
      await waitFor(() => /without a finished back/.test(w.document.body.textContent ?? '')))

    // Upload the back through the row's own input, the way a customer would:
    // the bundle builds a real multipart request, so this is the upload
    // endpoint exercised end to end rather than a mocked call.
    const input = await waitForEl(w.document, 'input#back-image-1')
    const back = new File([PNG], 'back.png', { type: 'image/png' })
    Object.defineProperty(input, 'files', { value: [back], configurable: true })
    input.dispatchEvent(new w.Event('change', { bubbles: true }))

    const stored = await waitFor(async () =>
      !!(await api(`/api/prefixes/${prefix}/cards/1`)).back_image_content_type,
    )
    check('the server stored the back image', stored)
    const backImgSrc = () =>
      w.document.querySelector(`img[src^="/api/prefixes/${prefix}/cards/1/display/back"]`)
        ?.getAttribute('src') ?? ''
    check(
      'the uploaded back is previewed',
      await waitFor(() => !!backImgSrc()),
      backImgSrc(),
    )
    check(
      'and its URL is versioned, so a re-render is a new URL rather than a cached one',
      /\?v=/.test(backImgSrc()),
      backImgSrc(),
    )
    check('the card is no longer reported as incomplete',
      await waitFor(() => !/without a finished back/.test(w.document.body.textContent ?? '')))

    // The same picture, but printed on the back instead of uploaded.
    pick(w, 'front')
    check(
      'a front copy needs no upload of its own',
      await waitFor(async () => (await api(`/api/prefixes/${prefix}/cards/1`)).back === 'front'),
    )
    check('the card counts as complete',
      !/without a finished back/.test(w.document.body.textContent ?? ''))
  }

  // --- switching the ribbon is seamless and lossless ----------------------
  // The point of the unification: nothing about a card is tied to the ribbon,
  // so the switch only re-renders. The card keeps every byte of its data — its
  // back, its picture — and what changes is the depth of the raster the spooler
  // fetches.
  {
    const prefix = await makePrefix('ymcko')
    await addCard(prefix)
    const fd = new NodeFormData()
    fd.append('back_image', new NodeBlob([PNG], { type: 'image/png' }), 'back.png')
    await realFetch(`${BASE}/api/prefixes/${prefix}/cards/1/image/back`, {
      method: 'PUT',
      body: fd,
    })

    const before = await api(`/api/prefixes/${prefix}/cards/1`)
    check('a picture back on a colour ribbon rasterises in 24-bit',
      (await rasterBits(prefix)) === 24, String(await rasterBits(prefix)))

    const switchTo = async (ribbon) =>
      (
        await realFetch(`${BASE}/api/prefixes/${prefix}`, {
          method: 'PATCH',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify({ ribbon }),
        })
      ).status

    check('switching to the black-panel ribbon is accepted',
      (await switchTo('ymckok')) === 200)
    const after = await api(`/api/prefixes/${prefix}/cards/1`)
    check('the card keeps its back source and its picture',
      after.back === before.back &&
        !!after.back_image_content_type === !!before.back_image_content_type,
      `${before.back} -> ${after.back}`)
    check('the same picture is re-rendered as a 1-bit raster',
      (await rasterBits(prefix)) === 1, String(await rasterBits(prefix)))

    check('switching back is accepted too', (await switchTo('ymcko')) === 200)
    const back = await api(`/api/prefixes/${prefix}/cards/1`)
    check('still nothing lost, and the raster is 24-bit again',
      back.back === before.back &&
        !!back.back_image_content_type &&
        (await rasterBits(prefix)) === 24,
      `back=${back.back} bits=${await rasterBits(prefix)}`)

    // The preview has to follow those bytes. A browser keeps whatever URL it was
    // given, so the row must hand it a new one when the render changes — this is
    // what repaints the back pane on a ribbon switch instead of leaving the pane
    // showing the previous depth, which is the bug the version is here for.
    const wp = await mount(`/p/${prefix}`, { bust: 'i4' })
    const backSrc = () =>
      wp.document.querySelector('.preview.back .backart')?.getAttribute('src') ?? ''
    const beforeSrc = backSrc()
    check('the back preview URL is versioned against the render', /\?v=/.test(beforeSrc), beforeSrc)

    const blackRadio = [...wp.document.querySelectorAll('.mode input')].find(
      (i) => i.value === 'ymckok',
    )
    check('the ribbon chooser offers the black-panel ribbon', !!blackRadio)
    blackRadio.checked = true
    blackRadio.dispatchEvent(new wp.Event('change', { bubbles: true }))
    check('switching the ribbon repaints the preview, by URL',
      await waitFor(() => /\?v=/.test(backSrc()) && backSrc() !== beforeSrc),
      `${beforeSrc} -> ${backSrc()}`)

    // And the artifact itself can say "still current" instead of being trusted:
    // every derived artifact carries a validator the browser can revalidate on.
    const art = await realFetch(`${BASE}/api/prefixes/${prefix}/cards/1/display/back`)
    const etag = art.headers.get('etag')
    check('an artifact carries an ETag and must be revalidated',
      !!etag && art.headers.get('cache-control') === 'no-cache',
      `${etag} / ${art.headers.get('cache-control')}`)
    const cond = await realFetch(`${BASE}/api/prefixes/${prefix}/cards/1/display/back`, {
      headers: { 'if-none-match': etag },
    })
    check('a matching If-None-Match answers 304', cond.status === 304, String(cond.status))
  }

  // --- a ribbon switch repaints a *front-copy* back too -------------------
  // Every back source renders at the ribbon's depth, so the pane has to show the
  // depth-accurate `display/back` artifact — not the front's colour display,
  // which never changes with the ribbon. Pointing that pane at the front is the
  // bug: on a black panel a copy of the front prints in black and white, while
  // the pane keeps advertising it in colour.
  {
    const prefix = await makePrefix('ymcko')
    await addCard(prefix)
    const w = await mount(`/p/${prefix}`, { bust: 'i4f' })
    pick(w, 'front')
    check('the front-copy back is stored',
      await waitFor(async () => (await api(`/api/prefixes/${prefix}/cards/1`)).back === 'front'))

    const backArt = () => w.document.querySelector('.preview.back .backart')
    const backSrc = () => backArt()?.getAttribute('src') ?? ''
    // The row re-renders from the card the PATCH returned, so the picture is not
    // in the DOM the instant the server has the write — wait for the element, or
    // this reads an empty src and the check fails on the race, not on the code.
    const el = await waitForEl(w.document, '.preview.back .backart')
    const beforeSrc = el?.getAttribute('src') ?? ''
    check('a front-copy back previews the back artifact, not the front',
      beforeSrc.startsWith(`/api/prefixes/${prefix}/cards/1/display/back`), beforeSrc)
    // ...and the version carries the ribbon, because the depth is the only thing
    // about a back the ribbon decides.
    check('that version names the ribbon the depth came from',
      beforeSrc.endsWith('-ymcko'), beforeSrc)

    const blackRadio = [...w.document.querySelectorAll('.mode input')].find(
      (i) => i.value === 'ymckok',
    )
    check('the ribbon chooser offers the black-panel ribbon', !!blackRadio)
    blackRadio.checked = true
    blackRadio.dispatchEvent(new w.Event('change', { bubbles: true }))
    check('switching to the black panel repaints the front-copy back',
      await waitFor(() => backSrc() !== beforeSrc && backSrc().endsWith('-ymckok')),
      `${beforeSrc} -> ${backSrc()}`)
  }

  // --- a black-panel ribbon: the same choices, rendered in 1 bit ----------
  // The unification, asserted end to end: a YMCKOK project offers every back,
  // the server stores a picture back on it, and what differs is the raster the
  // spooler fetches — 1 bit here, 24-bit on a colour-back project.
  {
    const prefix = await makePrefix('ymckok')
    await addCard(prefix)
    const w = await mount(`/p/${prefix}`, { bust: 'i3' })
    const offered = [...w.document.querySelectorAll('.back-choice input')].map((i) => i.value)
    check('a black-panel ribbon offers the same four backs',
      offered.join(',') === 'none,front,image,markdown', offered.join(','))

    // No guard server-side either: a picture back is stored, and reduced to
    // black and white when it is rendered.
    const accepted = await realFetch(`${BASE}/api/prefixes/${prefix}/cards/1`, {
      method: 'PATCH',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ back: 'image' }),
    })
    check('the server accepts a picture back on a black-panel ribbon',
      accepted.status === 200, String(accepted.status))

    const backFd = new NodeFormData()
    backFd.append('back_image', new NodeBlob([PNG], { type: 'image/png' }), 'back.png')
    const uploaded = await realFetch(
      `${BASE}/api/prefixes/${prefix}/cards/1/image/back`,
      { method: 'PUT', body: backFd },
    )
    check('the server stores it', uploaded.status === 200, String(uploaded.status))
    check('an uploaded picture back rasterises to 1 bit on this ribbon',
      (await rasterBits(prefix)) === 1, String(await rasterBits(prefix)))

    // The pane still shows the picture — the server's depth-accurate preview,
    // which for this ribbon is the black-and-white reduction.
    const wPic = await mount(`/p/${prefix}`, { bust: 'i3b' })
    check('the black-and-white back is previewed in the pane',
      !!wPic.document.querySelector(
        `img[src^="/api/prefixes/${prefix}/cards/1/display/back"]`),
    )
    // NB: the per-card caption that used to say this here was removed on purpose
    // ("Tone down the site verbosity"): the panel beside the pane shows the
    // server's depth-accurate render, and the ribbon choice is where the depth
    // is named, once — not repeated under every card.

    pick(w, 'markdown')
    const surface = await waitForEl(w.document, '.md-editor .ProseMirror')
    check('choosing text reveals the markdown editor', surface !== null)
    // The editor stays read-only until the source write has landed: text may not
    // be sent for a back the server still reads as single-sided.
    check('the editor becomes usable', await waitFor(() => mdEditable(w)))

    const md = '## Songs\n\n- one\n- two\n'
    check('the text can be typed into it', pasteIntoEditor(w, md))

    check(
      'the text is stored on the card',
      await waitFor(async () =>
        ((await api(`/api/prefixes/${prefix}/cards/1`)).back_text ?? '').includes('## Songs'),
      ),
    )
    const paneText = () =>
      w.document.querySelector('.preview.back')?.textContent ?? ''
    const backArt = () => w.document.querySelector('.preview.back .backart')
    // The pane is the server's artifact for this back and nothing else: the
    // markdown is not rendered in the browser, so there is no heading to find —
    // what replaces it is the picture the backend laid the text out into.
    check('the pane draws no markdown of its own',
      !w.document.querySelector('.preview.back .panel'))
    check('the pane shows the server\'s render of the text',
      await waitFor(() => !!backArt() && !/not printable yet/i.test(paneText())),
      paneText().trim())
    // ...and that render is versioned with the ribbon, so a ribbon switch
    // repaints it: the depth is the only thing the ribbon decides about a back.
    check('the text back\'s preview names the ribbon it was rendered for',
      backArt()?.getAttribute('src')?.endsWith('-ymckok') === true,
      backArt()?.getAttribute('src') ?? '')
    // A text back needs no picture, so its pane is not a drop area.
    check('a text pane is not a drop area', !w.document.querySelector('.preview.back .back-drop'))
    // A markdown back with text is complete: the page stops counting it as
    // unfinished once that card's write has landed.
    check('a text back is complete with no upload at all',
      await waitFor(() => !/without a finished back/.test(w.document.body.textContent ?? '')))
  }

  // --- dropping a picture onto the empty back pane -------------------------
  // The empty image area *is* the upload: a picture dropped anywhere on it
  // becomes the card's back, rather than the picker being the only way in.
  {
    const prefix = await makePrefix('ymcko')
    await addCard(prefix)
    const w = await mount(`/p/${prefix}`, { bust: 'i5' })
    pick(w, 'image') // an image back with no picture yet: the pane has the drop area
    const drop = await waitForEl(w.document, '.preview.back .back-drop')
    check('an image back with no picture is a drop area', drop !== null)

    // Dropped on the overlay that covers the pane — the element a real drop
    // lands on, since drop events bubble up rather than down.
    const ev = new w.Event('drop', { bubbles: true, cancelable: true })
    Object.defineProperty(ev, 'dataTransfer', {
      value: { files: [new File([PNG], 'dropped.png', { type: 'image/png' })] },
    })
    drop.dispatchEvent(ev)

    check(
      'a picture dropped on the pane becomes the card’s back picture',
      await waitFor(async () => {
        const c = await api(`/api/prefixes/${prefix}/cards/1`)
        return c.back === 'image' && !!c.back_image_content_type
      }),
    )
    check(
      'the dropped picture is previewed in the pane',
      await waitFor(
        () =>
          !!w.document.querySelector(`img[src^="/api/prefixes/${prefix}/cards/1/display/back"]`),
      ),
    )
  }

  // --- the text is written when the customer stops, not while they write ---
  // Every write of a card re-renders its rasters, and the response used to be
  // copied back into the box — which ate whatever had been typed while the
  // request was in flight. So: nothing goes out mid-sentence, leaving the box
  // writes at once, and what is in the box outlives the response.
  {
    const prefix = await makePrefix('ymcko')
    await addCard(prefix)
    const w = await mount(`/p/${prefix}`, { bust: 'i6' })
    const stored = async () =>
      (await api(`/api/prefixes/${prefix}/cards/1`)).back_text ?? ''

    pick(w, 'markdown')
    // Choosing the back is a write of its own, so wait for it to land before
    // counting writes of the text.
    await waitFor(async () => (await api(`/api/prefixes/${prefix}/cards/1`)).back === 'markdown')
    check('the text editor is usable', await waitFor(() => mdEditable(w)))

    check('the text can be typed into it', pasteIntoEditor(w, 'first line'))
    await settle(400)
    const midTyping = await stored()
    check('nothing is written while the customer is still writing',
      !midTyping.includes('first line'), midTyping)

    // Leaving the box writes it, without waiting for the idle timer.
    mdSurface(w).dispatchEvent(new w.Event('blur', { bubbles: true }))
    check('leaving the editor writes the text',
      await waitFor(async () => (await stored()).includes('first line')), await stored())

    // Typing that stops is written on its own.
    pasteIntoEditor(w, ' second')
    check('typing that stops is written by itself',
      await waitFor(async () => (await stored()).includes('second')), await stored())

    // The response is never reflected back into the box: it holds what was
    // typed, which is what the next write carries.
    const inBox = mdSurface(w)?.textContent ?? ''
    check('the box keeps what was typed', /first line[\s\S]*second/.test(inBox), inBox)
  }
}

// ---- scenario J: the admin edits prices and stock -------------------------
// Both are database state with a write path of their own, so the panel is
// driven for real: type a price, save it, read it back off the public shop
// config; take a ribbon out of stock, watch the customer page lose the option.
console.log('\nJ. admin: ribbon prices and stock')
{
  cookieJar = ''

  const w = await mount('/admin', { bust: 'j1' })
  const tok = w.document.querySelector('#admin-token')
  tok.value = TOKEN
  tok.dispatchEvent(new w.Event('input', { bubbles: true }))
  await settle(150)
  w.document.querySelector('form').dispatchEvent(
    new w.Event('submit', { bubbles: true, cancelable: true }),
  )
  await settle(1500)

  // What the shop looked like before this scenario touched it, so it can be put
  // back exactly: a rerun against a persistent server must start where the first
  // one did.
  const shop0 = await realFetch(`${BASE}/api/shop/config`).then((r) => r.json())
  const original = Object.fromEntries(shop0.prices.map((p) => [p.ribbon, p.price_cents]))

  const priceBox = (ribbon) => w.document.querySelector(`input#price-${ribbon}`)
  const saveButton = (ribbon) =>
    [...w.document.querySelectorAll('.srow')]
      .find((r) => r.querySelector(`#price-${ribbon}`))
      ?.querySelector('button')
  const stockBox = (ribbon) => w.document.querySelector(`input#stock-${ribbon}`)
  const shop = async () => realFetch(`${BASE}/api/shop/config`).then((r) => r.json())
  const priceOf = async (ribbon) =>
    (await shop()).prices.find((p) => p.ribbon === ribbon)

  check('the shop panel renders one row per ribbon',
    w.document.querySelectorAll('.srow').length === 2,
    String(w.document.querySelectorAll('.srow').length))
  check('each row shows its unit', /per printed side|per card/.test(
    w.document.body.textContent ?? ''))
  check('both price boxes are seeded from the server',
    priceBox('ymcko')?.value === '5.00' && priceBox('ymckok')?.value === '5.00',
    `${priceBox('ymcko')?.value} / ${priceBox('ymckok')?.value}`)
  check('an unchanged price cannot be re-saved', saveButton('ymcko')?.disabled === true)

  // A real edit: the box, then Save, then the server.
  priceBox('ymckok').value = '7.20'
  priceBox('ymckok').dispatchEvent(new w.Event('input', { bubbles: true }))
  await settle(150)
  check('editing a price enables its Save button', saveButton('ymckok')?.disabled === false)
  saveButton('ymckok').click()
  check('the new price is stored', await waitFor(async () =>
    (await priceOf('ymckok'))?.price_cents === 720),
  String((await priceOf('ymckok'))?.price_cents))
  check('the stored unit is the ribbon\'s, not guessed',
    (await priceOf('ymckok'))?.unit === 'card', String((await priceOf('ymckok'))?.unit))
  check('the button says it saved', await waitFor(() => /Saved/.test(
    saveButton('ymckok')?.textContent ?? '')))
  check('the other ribbon was left alone', (await priceOf('ymcko'))?.price_cents === 500)

  // The customer pays the new price: one card on YMCKOK, plus shipping.
  //
  // The customer calls go out unauthenticated — the admin session cookie would
  // make them cookie-authenticated, and the test's own api() helper does not
  // send the CSRF header that then becomes mandatory. Keep it aside and put it
  // back for the rest of the panel's writes.
  const adminCookie = cookieJar
  cookieJar = ''
  const prefix = (
    await api('/api/prefixes', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: '{}',
    })
  ).uuid
  const fd = new NodeFormData()
  fd.append('name', 'Repriced Card')
  fd.append('image', new NodeBlob([PNG], { type: 'image/png' }), 'repriced.png')
  await api(`/api/prefixes/${prefix}/cards`, { method: 'POST', body: fd })
  const wc = await mount(`/p/${prefix}`, { bust: 'j2' })
  await settle(1400)
  check('the customer sees the new price', /7[.,]20/.test(wc.document.body.textContent ?? ''),
    (wc.document.body.textContent ?? '').slice(0, 260))

  cookieJar = adminCookie

  // Garbage in the box is refused in the UI, and nothing is written.
  const before = (await priceOf('ymcko')).price_cents
  priceBox('ymcko').value = 'five'
  priceBox('ymcko').dispatchEvent(new w.Event('input', { bubbles: true }))
  await settle(150)
  saveButton('ymcko').click()
  await settle(600)
  check('a price that is not a number is refused before it is sent',
    /not a price/i.test(w.document.body.textContent ?? ''))
  check('and nothing was written', (await priceOf('ymcko')).price_cents === before)

  // The comma is what a German keyboard produces, so it has to work.
  priceBox('ymcko').value = '6,50'
  priceBox('ymcko').dispatchEvent(new w.Event('input', { bubbles: true }))
  await settle(150)
  saveButton('ymcko').click()
  check('a comma decimal separator is understood',
    await waitFor(async () => (await priceOf('ymcko')).price_cents === 650),
    String((await priceOf('ymcko')).price_cents))

  // Stock: uncheck, and the customer's chooser greys that ribbon out.
  stockBox('ymcko').checked = false
  stockBox('ymcko').dispatchEvent(new w.Event('change', { bubbles: true }))
  await settle(1200)
  check('the flag is stored', await waitFor(async () =>
    (await shop()).prices.find((p) => p.ribbon === 'ymcko').available === false))
  check('the panel says it is out of stock',
    /out of stock/i.test(w.document.body.textContent ?? ''))
  const wc2 = await mount('/p/' + prefix, { bust: 'j3' })
  await settle(1400)
  const byRibbon = (win, ribbon) =>
    [...win.document.querySelectorAll('.mode')].find((el) =>
      el.querySelector(`input[value="${ribbon}"]`))
  check('the customer cannot pick the out-of-stock ribbon',
    byRibbon(wc2, 'ymcko')?.querySelector('input')?.disabled === true)
  check('but it is still offered, not removed',
    wc2.document.querySelectorAll('.mode').length === 2)

  // Put both ribbons back exactly as they were found.
  const writePrice = async (ribbon, cents) => {
    const box = priceBox(ribbon)
    box.value = (cents / 100).toFixed(2)
    box.dispatchEvent(new w.Event('input', { bubbles: true }))
    await settle(150)
    saveButton(ribbon).click()
    return waitFor(async () => (await priceOf(ribbon)).price_cents === cents)
  }
  check('restoring a price through the same box works too',
    await writePrice('ymckok', original.ymckok),
    String((await priceOf('ymckok')).price_cents))
  await writePrice('ymcko', original.ymcko)
  stockBox('ymcko').checked = true
  stockBox('ymcko').dispatchEvent(new w.Event('change', { bubbles: true }))
  await settle(1200)
  check('the shop is back to how it was found',
    (await shop()).prices.every((p) => p.available && p.price_cents === original[p.ribbon]),
    JSON.stringify((await shop()).prices))
}

console.log(failures === 0 ? '\nALL CHECKS PASSED' : `\n${failures} CHECK(S) FAILED`);
process.exit(failures === 0 ? 0 : 1)
