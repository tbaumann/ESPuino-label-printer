// Smoke test: mount the *built* SPA bundle in jsdom against a live server.
//
// Usage:  ADMIN_TOKEN=<token> node test/smoke.mjs [baseUrl]
// The server must be running, started with the same ADMIN_TOKEN, and serving
// server/static (the vite build output).
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
  fd.append('back_text', '01 Song One\n02 Song Two')
  fd.append('image', new NodeBlob([PNG], { type: 'image/png' }), 'sample.png')
  await api(`/api/prefixes/${prefix}/cards`, { method: 'POST', body: fd })

  const w = await mount(`/p/${prefix}`, { bust: 'b' })
  const text = w.document.body.textContent ?? ''
  // NB: input values are DOM properties, not text content.
  check('name input holds the name', w.document.querySelector('input#name-1')?.value === 'Sample Card')
  check('serial input holds the serial', w.document.querySelector('input#serial-1')?.value === 'SN-0001')
  check('back textarea holds the text', w.document.querySelector('textarea#back-1')?.value === '01 Song One\n02 Song Two')
  check('back preview shows the text', text.includes('02 Song Two'))
  check('front preview points at the image endpoint',
    w.document.querySelector(`img[src="/api/prefixes/${prefix}/cards/1/image"]`) !== null)
  check('renders a back preview pane', !!w.document.querySelector('.preview.back'))
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
    ].join('\n')
    const fd = new NodeFormData()
    fd.append('name', 'MD Card')
    fd.append('back_text', md)
    fd.append('image', new NodeBlob([PNG], { type: 'image/png' }), 'md.png')
    await api(`/api/prefixes/${mdPrefix}/cards`, { method: 'POST', body: fd })

    const w = await mount(`/p/${mdPrefix}`, { bust: 'b2' })
    const half = w.document.querySelector('.preview.back .halfpanel')
    const html = half?.innerHTML ?? ''
    check('renders a heading', /<h1>Title<\/h1>/.test(html), html)
    check('renders bold', /<strong>bold<\/strong>/.test(html))
    check('renders italic', /<em>italic<\/em>/.test(html))
    check('renders a list', /<ul>/.test(html) && /<li>one<\/li>/.test(html))
    check('link is plain text, no <a>', !/<a\b/.test(html) && /link/.test(html))
    check('image is refused', !/<img\b/.test(html))
    check('raw HTML is stripped', !/<script\b/.test(html) && !/alert\(1\)/.test(html))
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
    !!w2.document.querySelector(`img[src="/api/prefixes/${prefix}/cards/1/image"]`))
  check('preview shows the back half-panel text', t2.includes('01 Admin Song'))
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
    // 1 card at 500 + 250 shipping = 750 minor units. Match the digits, not the
    // currency formatting, which depends on the host locale.
    check('the total is shown', /7[.,]50/.test(text), text.slice(0, 300))

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
// One stock flag (YMCKO) covers three-of-four modes: single-sided and
// double-sided colour share the ribbon, so they go out together, and the
// black-back mode is independent. The unavailable options must stay *visible*
// and be disabled — never dropped from the chooser.
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
  const availability = Object.fromEntries(config.prices.map((p) => [p.print_mode, p.available]))
  check('both colour modes go out of stock together',
    availability.single_sided === false && availability.double_sided_color === false,
    JSON.stringify(availability))
  check('the black-back mode is untouched',
    availability.color_front_black_back === true, JSON.stringify(availability))
  check('both stock flags are served', config.ribbons?.length === 2,
    JSON.stringify(config.ribbons))

  // A project on the out-of-stock ribbon: single-sided is YMCKO. It needs a
  // card, or the checkout would refuse it for having nothing to print and the
  // 409 below would prove the wrong thing.
  const prefix = (
    await api('/api/prefixes', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ print_mode: 'single_sided' }),
    })
  ).uuid

  const fd = new NodeFormData()
  fd.append('name', 'Ribbon Card')
  fd.append('image', new NodeBlob([PNG], { type: 'image/png' }), 'ribbon.png')
  await api(`/api/prefixes/${prefix}/cards`, { method: 'POST', body: fd })

  const w = await mount(`/p/${prefix}`, { bust: 'h1' })
  await settle(1200)
  const options = [...w.document.querySelectorAll('.mode')]
  const byMode = (mode) =>
    options.find((el) => el.querySelector(`input[value="${mode}"]`))

  check('every mode is still offered, none removed', options.length === 3,
    String(options.length))
  check('an out-of-stock mode is rendered disabled',
    byMode('single_sided')?.querySelector('input')?.disabled === true)
  check('its ribbon-mate is disabled too',
    byMode('double_sided_color')?.querySelector('input')?.disabled === true)
  check('the in-stock mode is selectable',
    byMode('color_front_black_back')?.querySelector('input')?.disabled === false)
  check('the disabled option says it is out of stock',
    /out of stock/i.test(byMode('single_sided')?.textContent ?? ''))
  check('the unavailable option is marked, not hidden',
    byMode('single_sided')?.classList.contains('unavailable') === true)

  // Paying for a mode that cannot be printed is refused on both layers: the
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

  // Switching to an available mode goes through the real PATCH route.
  const pick = byMode('color_front_black_back').querySelector('input')
  pick.checked = true
  pick.dispatchEvent(new w.Event('change', { bubbles: true }))
  await settle(900)
  check('choosing another mode PATCHes the prefix',
    seen.some((r) => r.method === 'PATCH' && r.url.includes(`/api/prefixes/${prefix}`)),
    JSON.stringify(seen.slice(-3)))
  const after = await api(`/api/prefixes/${prefix}`)
  check('the mode change took effect', after.print_mode === 'color_front_black_back',
    after.print_mode)

  // Restock: the options come back with no state to reset.
  check('operator can restock', (await setRibbon('ymcko', true)) === 200)
  const w2 = await mount(`/p/${prefix}`, { bust: 'h2' })
  await settle(1200)
  const after2 = [...w2.document.querySelectorAll('.mode input')]
  check('every mode is selectable again', after2.every((i) => !i.disabled),
    after2.map((i) => i.disabled).join(','))
}

console.log(failures === 0 ? '\nALL CHECKS PASSED' : `\n${failures} CHECK(S) FAILED`)
process.exit(failures === 0 ? 0 : 1)
