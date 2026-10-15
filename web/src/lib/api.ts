export type PrefixStatus =
  | 'draft'
  | 'payment_pending'
  | 'paid'
  | 'queued'
  | 'printing'
  | 'printed'
  | 'cancelled'

/**
 * The ribbon a job is printed with: the one choice per job, because a batch
 * cannot mix ribbon loads. Everything else is chosen per card.
 */
export type Ribbon = 'ymcko' | 'ymckoko'

/**
 * What a card has on its back side — the per-card single/double-sided choice.
 * `none` *is* a single-sided card, so there is no second flag to keep in step
 * with it.
 */
export type BackSource = 'none' | 'front' | 'image' | 'markdown'

export interface Prefix {
  uuid: string
  created_at: string
  status: PrefixStatus
  status_changed_at: string | null
  ribbon: Ribbon
  paid_ribbon?: Ribbon | null
  order_ref: string | null
  card_count: number
}

export interface Card {
  id: number
  name: string
  serial_number: string | null
  /** What this card's back is. `none` is a single-sided card. */
  back: BackSource
  /**
   * Markdown a `markdown` back is rendered from, in the ribbon's colour depth.
   * `null` for every other back, and for a markdown card with no text yet.
   */
  back_text: string | null
  image_content_type: string
  /**
   * Present only once a back image has been uploaded for an `image` back. The
   * content type is the signal that one exists — the stored path is never
   * serialized. Omitted by the server when there is none, so a card whose back
   * image is still missing reads as `undefined`.
   */
  back_image_content_type?: string | null
  created_at: string
  updated_at: string
  /** Server-side render availability: `pending` | `ready` | `failed`. */
  display_front: 'pending' | 'ready' | 'failed'
  /** Absent when the mode has no back to render. */
  display_back?: 'pending' | 'ready' | 'failed'
  print_front: 'pending' | 'ready' | 'failed'
  print_back?: 'pending' | 'ready' | 'failed'
}

/**
 * Whether this card is a double-sided one that is still missing the material its
 * back is made of: an `image` back with no upload, or a `markdown` back with no
 * text. A single-sided card, and a `front` copy (which needs nothing extra), are
 * never incomplete.
 *
 * A card exists from its front alone (the dropzone uploads a folder at a time),
 * so this is what the card row offers to fill in, and what the project page
 * counts before it offers to pay — the server refuses such an order too (409),
 * because that is where it must hold.
 */
export function backIncomplete(card: Card): boolean {
  if (card.back === 'image') return !card.back_image_content_type
  if (card.back === 'markdown') return !card.back_text || card.back_text.trim() === ''
  return false
}

export interface CardIndex {
  prefix: string
  cards: Card[]
}

export interface CardPatch {
  name?: string
  serial_number?: string
  /** The card's back side: how a card becomes, or stops being, double-sided. */
  back?: BackSource
  back_text?: string
}

/** One ribbon's stock flag, as the shop config serves it. */
export interface RibbonStock {
  ribbon: Ribbon
  in_stock: boolean
}

/**
 * One ribbon as the shop offers it. `available: false` means it is out of
 * stock: the option is rendered disabled, never dropped, so the customer can
 * see that it exists and is out.
 *
 * `price_cents` is the price of one billing *unit*, and `unit` says what that
 * is: `side` (YMCKO, a card costs one or two of them — `billedUnits`) or `card`
 * (YMCKOKO, one per card whatever its back is). The two ribbons therefore price
 * the same order differently, which is why the unit is served rather than
 * assumed.
 */
export interface RibbonPrice {
  ribbon: Ribbon
  price_cents: number
  /** What `price_cents` buys: one printed side, or one whole card. */
  unit: 'side' | 'card'
  available: boolean
}

/** Public shop settings. `enabled: false` means this deployment takes no money. */
export interface ShopConfig {
  enabled: boolean
  currency: string
  prices: RibbonPrice[]
  /** The same two stock flags, as they are stored. */
  ribbons: RibbonStock[]
  shipping_cents: number
  countries: string[]
}

/**
 * Customer-facing names for the two ribbons, and what each one means for the
 * back of a card. One place, so the chooser, the card panels and the read-only
 * preview cannot drift apart.
 */
export const RIBBONS: { ribbon: Ribbon; label: string; summary: string }[] = [
  {
    ribbon: 'ymckoko',
    label: 'Black back',
    summary:
      'Full-colour front, black-and-white back. Any back a card can have prints there in black only — text is what it is good at. Billed per card.',
  },
  {
    ribbon: 'ymcko',
    label: 'Colour back',
    summary:
      'Full-colour front, and a back in full colour: a second picture, a copy of the front, or text. Billed per printed side.',
  },
]

/**
 * What a card's back can be. The panel offers these in order, and what each one
 * needs from the customer is the whole of the per-card editor.
 */
export const BACK_SOURCES: { source: BackSource; label: string; summary: string }[] = [
  {
    source: 'none',
    label: 'Single-sided',
    summary: 'Front only, blank back. One ribbon panel per card.',
  },
  {
    source: 'front',
    label: 'Copy of the front',
    summary: 'The same picture printed on the back. Costs a second panel.',
  },
  {
    source: 'image',
    label: 'Second image',
    summary: 'A picture of its own, uploaded per card.',
  },
  {
    source: 'markdown',
    label: 'Text',
    summary: 'A song list and the like, rendered from markdown.',
  },
]

/**
 * What the two ribbons mean for a card's back, in one line each, so the card
 * rows and the ribbon chooser cannot describe the same choice differently.
 */
export const DEPTH_NOTE: Record<'color' | 'black', string> = {
  color: 'printed in full colour',
  black: 'printed in black only — a picture becomes black and white',
}

/**
 * The colour depth a back is rendered in — a consequence of the ribbon, not a
 * choice, and the *only* thing the ribbon decides about a back. Mirror of the
 * server's `Ribbon::back_depth`, so the preview can be honest about what will be
 * printed: every back source is offered on either ribbon.
 */
export function backDepth(ribbon: Ribbon): 'color' | 'black' {
  return ribbon === 'ymcko' ? 'color' : 'black'
}

/**
 * Billing units one card costs on a ribbon — the multiplier applied to that
 * ribbon's own price.
 *
 * YMCKO bills per printed side: a single-sided card is one, a double-sided one
 * two. YMCKOKO bills per card, whatever its back is: its panel set is spent
 * whether or not the second black panel is used, so a blank back saves nothing.
 * Mirror of the server's `Ribbon::billed_units`, which is what actually charges.
 */
export function billedUnits(ribbon: Ribbon, back: BackSource): number {
  if (ribbon === 'ymckoko') return 1
  return back === 'none' ? 1 : 2
}

/** A Stripe Checkout session, as far as the UI needs to know about it. */
export interface CheckoutSession {
  id: string
  url: string | null
  amount_total: number | null
  currency: string | null
  payment_status: string | null
}

export type NeedsAttentionKind =
  | 'payment_for_cancelled'
  | 'payment_for_draft'
  | 'payment_for_deleted'
  | 'payment_after_printing'
  | 'abandoned_checkout'

/** One row in the admin "needs attention" list: money and the order disagree. */
export interface NeedsAttentionItem {
  prefix: string
  kind: NeedsAttentionKind
  /** Imperative: what to do about it (refund_or_honour, refund_or_rebuild_by_hand, …). */
  action: string
  message: string
  at: string
  session_id: string | null
  amount_cents: number | null
  currency: string | null
}

/** Required on cookie-authenticated mutations; the server calls this CSRF. */
const CSRF_HEADER = 'x-esplp-csrf'
const MUTATING = new Set(['POST', 'PUT', 'PATCH', 'DELETE'])

async function req<T>(path: string, init: RequestInit = {}): Promise<T> {
  const method = (init.method ?? 'GET').toUpperCase()
  const headers = new Headers(init.headers)
  // Sent on every mutation. Harmless on customer calls — they carry no cookie,
  // so the server does not check — and required once the admin session is set.
  if (MUTATING.has(method)) headers.set(CSRF_HEADER, '1')
  const res = await fetch(path, { ...init, headers })
  const text = await res.text()
  if (!res.ok) {
    let msg = text || res.statusText
    try {
      msg = JSON.parse(text).error ?? msg
    } catch {
      /* not json */
    }
    throw new Error(`HTTP ${res.status}: ${msg}`)
  }
  return (text ? JSON.parse(text) : undefined) as T
}

const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i

export function isUuid(s: string): boolean {
  return UUID_RE.test(s.trim())
}

export function stem(filename: string): string {
  return filename.replace(/\.[^.]+$/, '')
}

/** Render an amount in minor units (cents) as a currency string. */
export function formatMoney(cents: number, currency: string): string {
  const code = currency.toUpperCase()
  try {
    return new Intl.NumberFormat(undefined, { style: 'currency', currency: code }).format(
      cents / 100,
    )
  } catch {
    // Unknown currency code: still better to show a number than to throw.
    return `${(cents / 100).toFixed(2)} ${code}`
  }
}

/**
 * Parse a typed amount ("5", "5,50", "€5.50") into minor units, or `null` if it
 * is not a number. One decimal separator, either the dot or the comma; a
 * thousands separator is not understood, which is the honest limitation of
 * letting someone type money into a text box.
 */
export function parseMoney(input: string): number | null {
  const cleaned = input.trim().replace(/[^\d.,-]/g, '').replace(',', '.')
  if (!cleaned || !/^-?\d*\.?\d*$/.test(cleaned)) return null
  const n = Number(cleaned)
  if (!Number.isFinite(n) || n < 0) return null
  return Math.round(n * 100)
}

/** The plain figure for a money input box: `500` → `"5.00"`. */
export function moneyInput(cents: number): string {
  return (cents / 100).toFixed(2)
}

export const api = {
  createPrefix: (uuid?: string, ribbon?: Ribbon): Promise<Prefix> =>
    req<Prefix>('/api/prefixes', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ ...(uuid ? { uuid } : {}), ...(ribbon ? { ribbon } : {}) }),
    }),

  getPrefix: (p: string): Promise<Prefix> => req<Prefix>(`/api/prefixes/${p}`),

  /**
   * Choose the prefix's ribbon. Only allowed while the prefix is still a draft
   * for the customer — the server answers 409 once it is sealed.
   *
   * The change is lossless: nothing about a card is tied to the ribbon except
   * how it is rendered, so the server re-renders the derived artifacts (back
   * previews and print rasters) from the stored originals and returns the
   * prefix. Re-read the cards afterwards, because those previews changed.
   */
  setRibbon: (p: string, ribbon: Ribbon): Promise<Prefix> =>
    req<Prefix>(`/api/prefixes/${p}`, {
      method: 'PATCH',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ ribbon }),
    }),

  listCards: (p: string): Promise<CardIndex> =>
    req<CardIndex>(`/api/prefixes/${p}/cards`),

  patchCard: (p: string, id: number, patch: CardPatch): Promise<Card> =>
    req<Card>(`/api/prefixes/${p}/cards/${id}`, {
      method: 'PATCH',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(patch),
    }),

  deleteCard: (p: string, id: number): Promise<void> =>
    req<void>(`/api/prefixes/${p}/cards/${id}`, { method: 'DELETE' }),

  /** Upload one image; the server assigns the next card id (POST). */
  uploadCard: (p: string, file: File, name?: string): Promise<Card> => {
    const fd = new FormData()
    fd.append('name', name?.trim() || stem(file.name))
    fd.append('image', file, file.name)
    return req<Card>(`/api/prefixes/${p}/cards`, { method: 'POST', body: fd })
  },

  cardImageUrl: (p: string, id: number): string =>
      `/api/prefixes/${p}/cards/${id}/image`,

    /** The processed front display image (normalized PNG) the UI shows. */
    cardDisplayUrl: (p: string, id: number): string =>
      `/api/prefixes/${p}/cards/${id}/display-image`,

    /** The processed back display image (colour or rendered black back). */
    cardDisplayBackUrl: (p: string, id: number): string =>
      `/api/prefixes/${p}/cards/${id}/display-back`,

  /**
   * The card's uploaded back image (an `image` back only).
   *
   * The URL is stable across a re-upload, so pass something that changes —
   * the card's `updated_at` is the honest answer — or the browser will keep
   * showing the back the customer just replaced.
   */
  cardBackImageUrl: (p: string, id: number, version?: string): string =>
    `/api/prefixes/${p}/cards/${id}/back-image${version ? `?v=${encodeURIComponent(version)}` : ''}`,

  /**
   * Attach or replace one card's back image, leaving the front alone. This also
   * makes the card's back an `image` back. Either ribbon accepts it: on a
   * black-panel ribbon the picture is rendered in black and white.
   */
  uploadBackImage: (p: string, id: number, file: File): Promise<Card> => {
    const fd = new FormData()
    fd.append('back_image', file, file.name)
    return req<Card>(`/api/prefixes/${p}/cards/${id}/back-image`, { method: 'PUT', body: fd })
  },

  /** Price and currency, so the UI can show a total before sending anyone to Stripe. */
  shopConfig: (): Promise<ShopConfig> => req<ShopConfig>('/api/shop/config'),

  /**
   * Open a Stripe Checkout session. Nothing is paid by this call: the caller
   * redirects the browser to `url`, and the prefix only becomes `paid` once
   * Stripe's webhook reaches the server.
   */
  startCheckout: (p: string): Promise<CheckoutSession> =>
    req<CheckoutSession>(`/api/prefixes/${p}/checkout`, { method: 'POST' }),
}

export const STATUSES: PrefixStatus[] = [
  'draft',
  'payment_pending',
  'paid',
  'queued',
  'printing',
  'printed',
  'cancelled',
]

/** Operator-only calls. These need the admin session cookie (or a bearer token). */
export const adminApi = {
  /** Exchange the operator token for a session cookie. */
  login: (token: string): Promise<void> =>
    req<void>('/api/admin/session', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ token }),
    }),

  logout: (): Promise<void> => req<void>('/api/admin/session', { method: 'DELETE' }),

  /**
   * Session state on page load. Raw fetch rather than req(), because "not
   * logged in" (401) and "admin API switched off" (503) are both expected
   * answers here, not errors.
   */
  session: async (): Promise<'authed' | 'anon' | 'disabled'> => {
    const res = await fetch('/api/admin/session')
    if (res.ok) return 'authed'
    return res.status === 503 ? 'disabled' : 'anon'
  },

  listPrefixes: (status?: PrefixStatus | ''): Promise<Prefix[]> =>
    req<Prefix[]>(`/api/prefixes${status ? `?status=${status}` : ''}`),

  /**
   * The "needs attention" list: prefixes where money and the order disagree
   * (payment settled for a cancelled/draft/deleted/past-printing prefix, or a
   * checkout abandoned in payment_pending), each saying what to do.
   */
  needsAttention: (): Promise<NeedsAttentionItem[]> =>
    req<NeedsAttentionItem[]>('/api/admin/needs-attention'),

  /** The operator may set any status, including backwards out of `printing`. */
  setStatus: (p: string, status: PrefixStatus, orderRef?: string): Promise<Prefix> =>
    req<Prefix>(`/api/prefixes/${p}/status`, {
      method: 'PATCH',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(
        orderRef === undefined ? { status } : { status, order_ref: orderRef },
      ),
    }),

  /** Deletes the prefix, every card and every blob under it. */
  deletePrefix: (p: string): Promise<void> =>
    req<void>(`/api/prefixes/${p}`, { method: 'DELETE' }),

  /**
   * The shop's prices and stock, so the operator can edit them. The same public
   * endpoint the customer page reads — nothing in it is secret, and the writes
   * are what need the operator.
   */
  shopConfig: (): Promise<ShopConfig> => req<ShopConfig>('/api/shop/config'),

  /**
   * Set one ribbon's price, in minor units, for the ribbon's own billing unit
   * (one printed side on YMCKO, one whole card on YMCKOKO — `RibbonPrice.unit`).
   * Returns what was stored, so the caller need not re-derive it.
   */
  setRibbonPrice: (ribbon: Ribbon, priceCents: number): Promise<RibbonPrice> =>
    req<RibbonPrice>(`/api/shop/ribbons/${ribbon}/price`, {
      method: 'PUT',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ price_cents: priceCents }),
    }),

  /**
   * Take a ribbon out of the shop, or put it back. This is the "the ribbon ran
   * out" switch: every mode that prints with it goes unavailable immediately,
   * and the checkout refuses it.
   */
  setRibbonStock: (ribbon: Ribbon, inStock: boolean): Promise<RibbonStock> =>
    req<RibbonStock>(`/api/shop/ribbons/${ribbon}`, {
      method: 'PUT',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ in_stock: inStock }),
    }),
}

/** Remembered projects, so the auto-created landing page isn't a dead end. */
export interface Recent {
  uuid: string
  at: number
}

const RECENT_KEY = 'esplp.recent'

export function rememberProject(uuid: string): void {
  try {
    const list = recentProjects().filter((r) => r.uuid !== uuid)
    list.unshift({ uuid, at: Date.now() })
    localStorage.setItem(RECENT_KEY, JSON.stringify(list.slice(0, 12)))
  } catch {
    /* storage unavailable */
  }
}

export function recentProjects(): Recent[] {
  try {
    const raw = localStorage.getItem(RECENT_KEY)
    return raw ? (JSON.parse(raw) as Recent[]) : []
  } catch {
    return []
  }
}