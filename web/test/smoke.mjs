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
  // The bundle calls fetch('/api/…'); Node's fetch needs an absolute URL.
  // jsdom does not implement fetch, so keep a handle on Node's.
  globalThis.fetch = (input, init) =>
    realFetch(typeof input === 'string' ? new URL(input, BASE).href : input, init)

  delete require.cache[require.resolve(bundlePath)]
  require(bundlePath)
  await new Promise((r) => setTimeout(r, 1200))
  return window
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

  // ---- scenario C: the auth boundary, then a frozen project ---------------
  console.log('\nC. auth boundary and the print freeze')
  check('list prefixes without a token -> 401', (await listPrefixes('')) === 401)
  check('list prefixes with a wrong token -> 401', (await listPrefixes('nope')) === 401)
  check('list prefixes with the admin token -> 200', (await listPrefixes(TOKEN)) === 200)
  check('set status without a token -> 401',
    (await setStatus(prefix, 'submitted', { token: '' })) === 401)

  for (const s of ['submitted', 'paid', 'queued', 'printing']) {
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

console.log(failures === 0 ? '\nALL CHECKS PASSED' : `\n${failures} CHECK(S) FAILED`)
process.exit(failures === 0 ? 0 : 1)
