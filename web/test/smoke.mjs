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
  check('back textarea holds the text', w.document.querySelector('textarea#back-1')?.value === '01 Song One\n02 Song Two')
  check('back preview shows the text', text.includes('02 Song Two'))
  // The preview shows the *processed* image the server derived, not the raw
  // upload — one renderer owns what a card looks like.
  check('front preview points at the display-image endpoint',
    w.document.querySelector(`img[src="/api/prefixes/${prefix}/cards/1/display-image"]`) !== null)
  check('renders a back preview pane', !!w.document.querySelector('.preview.back'))
  check('the back choice is marked on the card',
    w.document.querySelector('.back-choice.selected strong')?.textContent?.trim() === 'Text')
  check('renders a delete button', /Delete/.test(text))
  check('shows 1 card', text.includes('1 card'))
  check('name input is editable', !w.document.querySelector('input#name-1')?.disabled)

  // ---- scenario B2: markdown back_text renders sanitised in the preview ----
  console.log('\nB2. markdown back_text renders sanitised in the back preview')
  {
    const mdPrefix = (
      await api('/api/prefixes', { method: 'POST', headers: { 'content-type': 'application/json' }, body: '{}' })
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
      '![img](https://example.com/x.png)',
      '',
      '<script>alert(1)</script>',
      '',
      // GFM and raw HTML: not in the subset. Both renderers have to treat them
      // the same way or the preview promises a card the print does not deliver:
      // `~~x~~` stays literal (no strikethrough is parsed) and an HTML block is
      // dropped with its text (no DOMPurify-only "keep the content").
      '~~struck~~',
      '',
      '<div>blocked</div>',
    ].join('\n')
    const fd = new NodeFormData()
    fd.append('name', 'MD Card')
    fd.append('back', 'markdown')
    fd.append('back_text', md)
    fd.append('image', new NodeBlob([PNG], { type: 'image/png' }), 'md.png')
    await api(`/api/prefixes/${mdPrefix}/cards`, { method: 'POST', body: fd })

    const w = await mount(`/p/${mdPrefix}`, { bust: 'b2' })
    // The markdown back's pane; its inner panel carries the sanitised markup.
    const half = w.document.querySelector('.preview.back.black-only .panel')
    const html = half?.innerHTML ?? ''
    check('renders a heading', /<h1>Title<\/h1>/.test(html), html)
    check('renders bold', /<strong>bold<\/strong>/.test(html))
    check('renders italic', /<em>italic<\/em>/.test(html))
    check('renders a list', /<ul>/.test(html) && /<li>one<\/li>/.test(html))
    check('link is plain text, no <a>', !/<a\b/.test(html) && /link/.test(html))
    check('image is refused', !/<img\b/.test(html))
    check('raw HTML is stripped', !/<script\b/.test(html) && !/alert\(1\)/.test(html))
    // Neither side parses GFM...
    check('gfm strikethrough stays literal',
      html.includes('~~struck~~') && !/<del\b/.test(html), html)
    // ...and raw HTML goes whole, text included, not tag-only as DOMPurify
    // alone would do it.
    check('an HTML block is dropped with its text',
      !/<div\b/.test(html) && !html.includes('blocked'), html)

    // The print queue carries no rendering any more: the spooler prints the
    // server's raster, and the pane above is only the editing view. What the
    // queue does carry is what the card's back *is*.
    const queue = await api(`/api/prefixes/${mdPrefix}/print-queue`, {
      headers: { authorization: `Bearer ${TOKEN}` },
    })
    check('the queue reports the black back', queue.cards[0]?.back === 'black',
      JSON.stringify(queue.cards[0]))
    check('the queue reports the ribbon to load', queue.ribbon === 'ymckoko',
      String(queue.ribbon))
    check('a YMCKOKO card is billed as a full double-sided one',
      queue.cards[0]?.billed_sides === 2, String(queue.cards[0]?.billed_sides))
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
    !!w2.document.querySelector(`img[src="/api/prefixes/${prefix}/cards/1/display-image"]`))
  check('preview shows the back text', t2.includes('01 Admin Song'))
  check('preview shows the name as text', t2.includes('Admin Preview Card'))
  check('preview has no card editor inputs', !w2.document.querySelector('input#name-1'))
  check('preview has no upload dropzone', !w2.document.querySelector('.dropzone'))
  check('preview offers no way to add cards', !/drop some images|upload/i.test(t2))
  check('has a prefix delete button', !!w2.document.querySelector('button.danger'))

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
    // The default ribbon is YMCKOKO, which bills every card as a full
    // double-sided one: 2 sides at 500 + 250 shipping = 1250 minor units. Match
    // the digits, not the currency formatting, which depends on the host locale.
    check('the total is shown', /12[.,]50/.test(text), text.slice(0, 300))
    check('the total counts printed sides, not cards', /2 printed sides/.test(text), text.slice(0, 300))

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
  check('the other ribbon is untouched', availability.ymckoko === true,
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
    byRibbon('ymckoko')?.querySelector('input')?.disabled === false)
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
  const pick = byRibbon('ymckoko').querySelector('input')
  pick.checked = true
  pick.dispatchEvent(new w.Event('change', { bubbles: true }))
  await settle(900)
  check('choosing another ribbon PATCHes the prefix',
    seen.some((r) => r.method === 'PATCH' && r.url.includes(`/api/prefixes/${prefix}`)),
    JSON.stringify(seen.slice(-3)))
  const after = await api(`/api/prefixes/${prefix}`)
  check('the ribbon change took effect', after.ribbon === 'ymckoko', after.ribbon)

  // Restock: the options come back with no state to reset.
  check('operator can restock', (await setRibbon('ymcko', true)) === 200)
  const w2 = await mount(`/p/${prefix}`, { bust: 'h2' })
  await settle(1200)
  const after2 = [...w2.document.querySelectorAll('.mode input')]
  check('every ribbon is selectable again', after2.every((i) => !i.disabled),
    after2.map((i) => i.disabled).join(','))
}

// ---- scenario I: the per-card back choice ----------------------------------
// The job fixes the ribbon; the *card* chooses what its back is, and the ribbon
// bounds the choices: YMCKO's back is a colour panel set (any source), YMCKOKO's
// is the second black panel (text only). One panel offers those choices and the
// input each one needs — the colour depth is the ribbon's, never a third choice.
console.log('\nI. one panel, the back choices the ribbon allows')
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
      'no back-image chooser and no text field yet',
      !w.document.querySelector('input#back-image-1') &&
        !w.document.querySelector('textarea#back-1'),
    )
    check(
      'the source chooser is in the editor, not the pane',
      !!w.document.querySelector('.side.editor .back-choices input[value="image"]'),
    )
    check(
      'every back the colour ribbon can print is offered',
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
      'the file chooser is inside the back pane',
      (await waitForEl(w.document, 'input#back-image-1[type=file]')) !== null &&
        !!w.document.querySelector('.preview.back input#back-image-1[type=file]'),
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
    check(
      'the uploaded back is previewed',
      await waitFor(
        () =>
          !!w.document.querySelector(`img[src="/api/prefixes/${prefix}/cards/1/display-back"]`),
      ),
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

  // --- a black-panel ribbon: text is the only back it can print -----------
  {
    const prefix = await makePrefix('ymckoko')
    await addCard(prefix)
    const w = await mount(`/p/${prefix}`, { bust: 'i3' })
    const offered = [...w.document.querySelectorAll('.back-choice input')].map((i) => i.value)
    check('a black-panel ribbon offers no picture back',
      !offered.includes('image') && !offered.includes('front'), offered.join(','))
    check('it offers single-sided and text',
      offered.includes('none') && offered.includes('markdown'), offered.join(','))

    // The server holds the same rule, not just the panel that renders it: a
    // picture back cannot be stored on this ribbon at all.
    const refused = await realFetch(`${BASE}/api/prefixes/${prefix}/cards/1`, {
      method: 'PATCH',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ back: 'image' }),
    })
    check('the server refuses a picture back on a black-panel ribbon',
      refused.status === 400, String(refused.status))

    pick(w, 'markdown')
    const ta = await waitForEl(w.document, 'textarea#back-1')
    check('choosing text reveals the markdown editor', ta !== null)
    // The editor stays disabled until the source write has landed: text may not
    // be sent for a back the server still reads as single-sided.
    check('the editor becomes usable', ta !== null && (await waitFor(() => ta.disabled === false)))

    const md = '## Songs\n\n- one\n- two\n\n![art](https://example.com/a.png)\n'
    if (ta) {
      ta.value = md
      ta.dispatchEvent(new w.Event('input', { bubbles: true }))
    }

    check(
      'the text is stored on the card',
      await waitFor(async () =>
        ((await api(`/api/prefixes/${prefix}/cards/1`)).back_text ?? '').includes('## Songs'),
      ),
    )
    check(
      'the markdown is rendered into the pane',
      await waitFor(() => /<h2>Songs<\/h2>/.test(w.document.querySelector('.preview.back')?.innerHTML ?? '')),
    )

    const pane = w.document.querySelector('.preview.back.black-only')
    const t = w.document.body.textContent ?? ''
    check('the back renders its 1-bit pane', !!pane)
    check('the pane is labelled black only', /black only \(1 bit\)/i.test(t))
    // A text back needs no picture, so its pane is not a drop area — and on this
    // ribbon a picture back cannot be printed at all.
    check('a text pane is not a drop area', !w.document.querySelector('.preview.back .back-drop'))
    check('no image is rendered on the black back', !/<img\b/.test(pane?.innerHTML ?? ''))
    // The medium refuses a picture, so the editor says so instead of leaving a
    // gap where the customer's artwork would have been.
    check('the editor says image markup is not printed',
      /image markup/i.test(t) && /cannot\s+print/i.test(t))
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
          !!w.document.querySelector(`img[src="/api/prefixes/${prefix}/cards/1/display-back"]`),
      ),
    )
  }
}

console.log(failures === 0 ? '\nALL CHECKS PASSED' : `\n${failures} CHECK(S) FAILED`);
process.exit(failures === 0 ? 0 : 1)
