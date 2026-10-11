export type PrefixStatus =
  | 'draft'
  | 'submitted'
  | 'paid'
  | 'queued'
  | 'printing'
  | 'printed'
  | 'cancelled'

export interface Prefix {
  uuid: string
  created_at: string
  status: PrefixStatus
  status_changed_at: string | null
  order_ref: string | null
  card_count: number
}

export interface Card {
  id: number
  name: string
  serial_number: string | null
  back_text: string | null
  image_content_type: string
  created_at: string
  updated_at: string
}

export interface CardIndex {
  prefix: string
  cards: Card[]
}

export interface CardPatch {
  name?: string
  serial_number?: string
  back_text?: string
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

export const api = {
  createPrefix: (uuid?: string): Promise<Prefix> =>
    req<Prefix>('/api/prefixes', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(uuid ? { uuid } : {}),
    }),

  getPrefix: (p: string): Promise<Prefix> => req<Prefix>(`/api/prefixes/${p}`),

  listCards: (p: string): Promise<CardIndex> =>
    req<CardIndex>(`/api/prefixes/${p}/cards`),

  submitPrefix: (p: string): Promise<Prefix> =>
    req<Prefix>(`/api/prefixes/${p}/submit`, { method: 'POST' }),

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
}

export const STATUSES: PrefixStatus[] = [
  'draft',
  'submitted',
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