<script lang="ts">
  import { onMount } from 'svelte'
  import { api, adminApi, STATUSES, type Card, type Prefix, type PrefixStatus } from '../lib/api'
  import { navigate } from '../lib/router'
  import CardRow from '../lib/CardRow.svelte'

  let { uuid }: { uuid: string } = $props()

  let prefix = $state<Prefix | null>(null)
  let cards = $state<Card[]>([])
  let loading = $state(true)
  let error = $state<string | null>(null)
  let busy = $state(false)

  // Editable copies for the status form; refreshed from the server after a write.
  let status = $state<PrefixStatus>('draft')
  let orderRef = $state('')

  async function refresh() {
    const [p, idx] = await Promise.all([api.getPrefix(uuid), api.listCards(uuid)])
    prefix = p
    cards = idx.cards
    status = p.status
    orderRef = p.order_ref ?? ''
  }

  onMount(async () => {
    try {
      await refresh()
    } catch (e) {
      error = (e as Error).message
    } finally {
      loading = false
    }
  })

  async function apply() {
    busy = true
    error = null
    try {
      prefix = await adminApi.setStatus(uuid, status, orderRef)
      await refresh()
    } catch (e) {
      error = (e as Error).message
    } finally {
      busy = false
    }
  }

  async function remove() {
    const n = prefix?.card_count ?? cards.length
    if (!confirm(`Delete ${uuid} and its ${n} card${n === 1 ? '' : 's'}? This cannot be undone.`)) {
      return
    }
    busy = true
    error = null
    try {
      await adminApi.deletePrefix(uuid)
      navigate('/admin')
    } catch (e) {
      error = (e as Error).message
      busy = false
    }
  }
</script>

<div class="topbar">
  <span class="brand">ESPuino Label Printer</span>
  <span class="badge">admin</span>
  <span class="spacer"></span>
  <a href="/admin" onclick={(e) => { e.preventDefault(); navigate('/admin') }} style="color:var(--accent);text-decoration:none">
    ← All prefixes
  </a>
  <a href="/swagger-ui/" target="_blank" rel="noreferrer" class="muted" style="text-decoration:none">API</a>
</div>

<div class="app">
  {#if error}
    <div class="error">{error}</div>
  {/if}

  {#if loading}
    <p class="muted"><span class="spinner"></span> loading…</p>
  {:else if prefix}
    <section class="panel">
      <div class="row">
        <h2>Prefix</h2>
        <span class="badge status-{prefix.status}">{prefix.status}</span>
        <span style="flex:1"></span>
        <button class="danger" onclick={remove} disabled={busy}>Delete prefix</button>
      </div>

      <div class="row" style="margin-top:12px">
        <span class="mono muted">{uuid}</span>
        <span class="muted">{prefix.card_count} card{prefix.card_count === 1 ? '' : 's'}</span>
        <span class="muted">created {prefix.created_at}</span>
      </div>

      <div class="row" style="margin-top:14px">
        <div>
          <label for="admin-status">Status</label>
          <select id="admin-status" bind:value={status}>
            {#each STATUSES as s (s)}
              <option value={s}>{s}</option>
            {/each}
          </select>
        </div>
        <div style="min-width:200px">
          <label for="admin-order-ref">Order reference</label>
          <input id="admin-order-ref" type="text" bind:value={orderRef} placeholder="optional" />
        </div>
        <button class="primary" onclick={apply} disabled={busy} style="align-self:flex-end">
          {busy ? 'Applying…' : 'Apply'}
        </button>
      </div>
      <p class="muted" style="margin-top:8px;font-size:13px">
        Any status, in any direction — including back out of printing.
      </p>
    </section>

    <section>
      <div class="row" style="margin-bottom:12px">
        <h2>Print preview</h2>
        <span class="muted">read-only</span>
      </div>
      {#if cards.length === 0}
        <div class="empty">This prefix has no cards.</div>
      {:else}
        <div class="cards">
          {#each cards as card (card.id)}
            <CardRow prefix={uuid} {card} ribbon={prefix.ribbon} writable={false} readonly />
          {/each}
        </div>
      {/if}
    </section>
  {:else}
    <div class="empty">Prefix not found.</div>
  {/if}
</div>
