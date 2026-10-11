<script lang="ts">
  import { onMount } from 'svelte'
  import { adminApi, STATUSES, type Prefix, type PrefixStatus } from '../lib/api'
  import { navigate } from '../lib/router'

  /** 'checking' until we have asked the server whether a session exists. */
  let auth = $state<'checking' | 'authed' | 'anon' | 'disabled'>('checking')
  let token = $state('')
  let busy = $state(false)
  let error = $state<string | null>(null)

  let prefixes = $state<Prefix[]>([])
  let filter = $state<PrefixStatus | ''>('')

  onMount(async () => {
    auth = await adminApi.session()
    if (auth === 'authed') await load()
  })

  async function load() {
    error = null
    try {
      prefixes = await adminApi.listPrefixes(filter)
    } catch (e) {
      error = (e as Error).message
    }
  }

  async function login(e: SubmitEvent) {
    e.preventDefault()
    busy = true
    error = null
    try {
      await adminApi.login(token.trim())
      token = ''
      auth = 'authed'
      await load()
    } catch (err) {
      error = (err as Error).message
    } finally {
      busy = false
    }
  }

  async function logout() {
    error = null
    try {
      await adminApi.logout()
    } catch (e) {
      error = (e as Error).message
    }
    prefixes = []
    auth = 'anon'
  }

  async function setStatus(p: Prefix, status: PrefixStatus) {
    error = null
    try {
      await adminApi.setStatus(p.uuid, status)
      await load()
    } catch (e) {
      error = (e as Error).message
    }
  }

  async function remove(p: Prefix) {
    const n = p.card_count
    if (!confirm(`Delete ${p.uuid} and its ${n} card${n === 1 ? '' : 's'}? This cannot be undone.`)) {
      return
    }
    error = null
    try {
      await adminApi.deletePrefix(p.uuid)
      await load()
    } catch (e) {
      error = (e as Error).message
    }
  }

  function open(uuid: string) {
    navigate(`/admin/p/${uuid}`)
  }
</script>

<div class="topbar">
  <span class="brand">ESPuino Label Printer</span>
  <span class="badge">admin</span>
  <span class="spacer"></span>
  <a href="/" onclick={(e) => { e.preventDefault(); navigate('/') }} style="color:var(--accent);text-decoration:none">
    Customer view
  </a>
  <a href="/swagger-ui/" target="_blank" rel="noreferrer" class="muted" style="text-decoration:none">API</a>
</div>

<div class="app">
  {#if auth === 'checking'}
    <p class="muted"><span class="spinner"></span> checking session…</p>
  {:else if auth === 'disabled'}
    <div class="error">
      The admin API is switched off on this server: <span class="mono">ADMIN_TOKEN</span> is not set.
    </div>
  {:else if auth === 'anon'}
    <section class="panel login">
      <h2>Admin login</h2>
      <form onsubmit={login}>
        <div>
          <label for="admin-token">Operator token</label>
          <input
            id="admin-token"
            type="password"
            bind:value={token}
            autocomplete="current-password"
            placeholder="ADMIN_TOKEN"
          />
        </div>
        <button class="primary" type="submit" disabled={busy || !token.trim()}>
          {busy ? 'Checking…' : 'Log in'}
        </button>
      </form>
      {#if error}
        <div class="error" style="margin-top:14px">{error}</div>
      {/if}
    </section>
  {:else}
    {#if error}
      <div class="error">{error}</div>
    {/if}

    <section class="panel">
      <div class="row">
        <h2>Prefixes</h2>
        <span class="badge">{prefixes.length}</span>
        <span style="flex:1"></span>
        <label class="field-label" for="status-filter" style="margin:0">Status</label>
        <select id="status-filter" bind:value={filter} onchange={load}>
          <option value="">all</option>
          {#each STATUSES as s (s)}
            <option value={s}>{s}</option>
          {/each}
        </select>
        <button onclick={load} disabled={busy}>Refresh</button>
        <button onclick={logout}>Log out</button>
      </div>
    </section>

    {#if prefixes.length === 0}
      <div class="empty">No prefixes.</div>
    {:else}
      <div class="plist">
        {#each prefixes as p (p.uuid)}
          <div class="prow">
            <a
              class="mono"
              href={`/admin/p/${p.uuid}`}
              onclick={(e) => { e.preventDefault(); open(p.uuid) }}
            >
              {p.uuid}
            </a>
            <span class="badge status-{p.status}">{p.status}</span>
            <span class="muted">{p.card_count} {p.card_count === 1 ? 'card' : 'cards'}</span>
            {#if p.order_ref}
              <span class="mono muted">{p.order_ref}</span>
            {/if}
            <span style="flex:1"></span>
            <label class="field-label" for={`status-${p.uuid}`} style="margin:0">set</label>
            <select
              id={`status-${p.uuid}`}
              value={p.status}
              onchange={(e) => setStatus(p, e.currentTarget.value as PrefixStatus)}
            >
              {#each STATUSES as s (s)}
                <option value={s}>{s}</option>
              {/each}
            </select>
            <button onclick={() => open(p.uuid)}>Preview</button>
            <button class="danger" onclick={() => remove(p)}>Delete</button>
          </div>
        {/each}
      </div>
    {/if}
  {/if}
</div>
