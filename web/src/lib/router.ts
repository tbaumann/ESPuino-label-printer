import { writable } from 'svelte/store'

/** Current client-side path, kept in sync with the History API. */
export const path = writable<string>(window.location.pathname)

export function navigate(to: string): void {
  if (to !== window.location.pathname) {
    history.pushState({}, '', to)
  }
  path.set(window.location.pathname)
}

window.addEventListener('popstate', () => path.set(window.location.pathname))

/** Extract the prefix uuid from a `/p/<uuid>` path, else null. */
export function projectFromPath(p: string): string | null {
  const m = p.match(/^\/p\/([^/]+)\/?$/)
  return m ? decodeURIComponent(m[1]) : null
}

/** Whether the path belongs to the admin UI (`/admin`, `/admin/p/<uuid>`). */
export function isAdminPath(p: string): boolean {
  const clean = p.replace(/\/+$/, '')
  return clean === '/admin' || clean.startsWith('/admin/')
}

/** Extract the prefix uuid from an `/admin/p/<uuid>` path, else null. */
export function adminProjectFromPath(p: string): string | null {
  const m = p.match(/^\/admin\/p\/([^/]+)\/?$/)
  return m ? decodeURIComponent(m[1]) : null
}
