<script lang="ts">
  import { onMount } from 'svelte'
  import {
    adminApi,
    RIBBONS,
    STATUSES,
    formatMoney,
    moneyInput,
    parseMoney,
    type NeedsAttentionItem,
    type Prefix,
    type PrefixStatus,
    type Ribbon,
    type ShopConfig,
  } from '../lib/api'
  import { navigate } from '../lib/router'

  /** 'checking' until we have asked the server whether a session exists. */
  let auth = $state<'checking' | 'authed' | 'anon' | 'disabled'>('checking')
  let token = $state('')
  let busy = $state(false)
  let error = $state<string | null>(null)

  let prefixes = $state<Prefix[]>([])
  let filter = $state<PrefixStatus | ''>('')
  let attention = $state<NeedsAttentionItem[]>([])
  let shop = $state<ShopConfig | null>(null)

  // The prices as typed, one string per ribbon. A text box has to survive an
  // unparseable entry ("5,5" is fine, "five" is not), so the server's figure is
  // re-read from `shop` on save and the box keeps whatever is in it until then.
  let typed = $state<Record<string, string>>({})
  let saving = $state<Ribbon | null>(null)
  // Which row was just written, so its Save button can say so and stop asking.
  let saved = $state<Ribbon | null>(null)

  onMount(async () => {
    auth = await adminApi.session()
    if (auth === 'authed') await load()
  })

  async function load() {
    error = null
    try {
      const [list, flag, config] = await Promise.all([
        adminApi.listPrefixes(filter),
        adminApi.needsAttention(),
        adminApi.shopConfig(),
      ])
      prefixes = list
      attention = flag
      shop = config
      // Seed the boxes from the server, but leave alone a box the operator has
      // typed in and not saved — a refresh must not eat their edit.
      const next: Record<string, string> = {}
      for (const p of config.prices) {
        next[p.ribbon] = typed[p.ribbon] ?? moneyInput(p.price_cents)
      }
      typed = next
    } catch (e) {
      error = (e as Error).message
    }
  }

  /** One editable row per ribbon: the customer-facing label, its unit, its price. */
  const ribbonRows = $derived(
    (shop?.prices ?? []).map((price) => {
      const known = RIBBONS.find((r) => r.ribbon === price.ribbon)
      return {
        ribbon: price.ribbon,
        label: known?.label ?? price.ribbon,
        unit: price.unit,
        inStock: price.available,
        // What is stored, as the box would spell it — this is what a Save that
        // changes nothing should be judged against.
        stored: moneyInput(price.price_cents),
      }
    }),
  )

  function unitNote(unit: string): string {
    return unit === 'card' ? 'per card, however many sides it prints' : 'per printed side'
  }

  /** Write one ribbon's price, but only if the box holds a real amount. */
  async function savePrice(ribbon: Ribbon) {
    const cents = parseMoney(typed[ribbon] ?? '')
    if (cents === null) {
      error = `That is not a price: "${typed[ribbon]}" — type it as 5 or 5.50, in the shop's currency.`
      return
    }
    saving = ribbon
    saved = null
    error = null
    try {
      const updated = await adminApi.setRibbonPrice(ribbon, cents)
      if (shop) {
        shop = {
          ...shop,
          prices: shop.prices.map((p) => (p.ribbon === ribbon ? updated : p)),
        }
      }
      typed = { ...typed, [ribbon]: moneyInput(updated.price_cents) }
      saved = ribbon
    } catch (e) {
      error = (e as Error).message
    } finally {
      saving = null
    }
  }

  /** Flip one ribbon's stock flag. Felt by the shop immediately. */
  async function setStock(ribbon: Ribbon, inStock: boolean) {
    error = null
    saved = null
    try {
      await adminApi.setRibbonStock(ribbon, inStock)
      await load()
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
  <span class="brand">Kartenschmiede</span>
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

    {#if attention.length > 0}
      <section class="panel">
        <div class="row">
          <h2>Needs attention</h2>
          <span class="badge">{attention.length}</span>
          <span class="muted" style="margin-left:8px">
            money and the order disagree — deal with each one
          </span>
        </div>
        <div class="alist">
          {#each attention as a (a.prefix + a.at)}
            <div class="ablock">
              <div class="row">
                <a
                  class="mono"
                  href={`/admin/p/${a.prefix}`}
                  onclick={(e) => { e.preventDefault(); open(a.prefix) }}
                >
                  {a.prefix}
                </a>
                <span class="badge kind-{a.kind}">{a.kind.replace(/_/g, ' ')}</span>
                <span class="action mono">{a.action}</span>
                {#if a.currency !== null && a.amount_cents !== null}
                  <span class="muted">{formatMoney(a.amount_cents as number, a.currency as string)}</span>
                {/if}
                <span style="flex:1"></span>
                <span class="muted">{a.at}</span>
              </div>
              <p class="amessage">{a.message}</p>
            </div>
          {/each}
        </div>
      </section>
    {/if}

    <section class="panel">
      <div class="row">
        <h2>Ribbons</h2>
        <span style="flex:1"></span>
        <span class="muted">
          prices apply to checkouts opened from now on, not to ones already open
        </span>
      </div>
      {#if !shop}
        <p class="muted">loading the shop config…</p>
      {:else}
        <div class="plist">
          {#each ribbonRows as r (r.ribbon)}
            <div class="srow">
              <strong style="min-width:92px">{r.label}</strong>
              <input
                id={`price-${r.ribbon}`}
                class="money mono"
                type="text"
                inputmode="decimal"
                aria-label={`price of one unit on ${r.ribbon}`}
                bind:value={typed[r.ribbon]}
                disabled={saving === r.ribbon}
                onkeydown={(e) => {
                  if (e.key === 'Enter') void savePrice(r.ribbon)
                }}
              />
              <span class="muted">
                {shop.currency.toUpperCase()} — {unitNote(r.unit)}
              </span>
              <button
                onclick={() => savePrice(r.ribbon)}
                disabled={saving !== null || typed[r.ribbon] === r.stored}
              >
                {saving === r.ribbon ? 'Saving…' : saved === r.ribbon ? 'Saved' : 'Save'}
              </button>
              <span style="flex:1"></span>
              <label class="check" for={`stock-${r.ribbon}`}>
                <input
                  id={`stock-${r.ribbon}`}
                  type="checkbox"
                  checked={r.inStock}
                  onchange={(e) => setStock(r.ribbon, e.currentTarget.checked)}
                />
                in stock
              </label>
              {#if !r.inStock}
                <span class="badge status-cancelled">out of stock</span>
              {/if}
            </div>
          {/each}
        </div>
        <p class="muted" style="margin-top:10px">
          A unit is what the price is for: a printed side on the colour-back ribbon
          (so a two-sided card costs two of them), a whole card on the black-back
          one, whatever its back is. Unchecking <em>in stock</em> takes the ribbon
          out of the shop at once — a project already priced for it keeps its own
          price and can be printed, it just cannot open a new checkout.
        </p>
      {/if}
    </section>

    <section class="panel">
      <div class="row">
        <h2>Prefixes</h2>
        <span class="badge">{prefixes.length}</span>
        <span style="flex:1"></span>
        <span class="muted">setting a status here is immediate — <em>queued</em> reprints the order</span>
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
