<script lang="ts">
  import { onMount } from 'svelte'
  import {
    api,
    formatMoney,
    rememberProject,
    type Card,
    type Prefix,
    type ShopConfig,
  } from '../lib/api'
  import { navigate } from '../lib/router'
  import Uploader from '../lib/Uploader.svelte'
  import CardRow from '../lib/CardRow.svelte'

  let { uuid }: { uuid: string } = $props()

  let prefix = $state<Prefix | null>(null)
  let cards = $state<Card[]>([])
  let error = $state<string | null>(null)
  let loading = $state(true)
  let copied = $state(false)
  let shop = $state<ShopConfig | null>(null)
  let paying = $state(false)
  let awaitingPayment = $state(false)

  // The customer may only write while the prefix is a draft; the moment
  // they click Pay the project is sealed for them. This deliberately matches
  // the server's `is_customer_writable()` (draft only), not the operator's
  // `is_writable()`.
  const WRITABLE: string[] = ['draft']
  const writable = $derived(prefix ? WRITABLE.includes(prefix.status) : false)

  // Amounts pre-rendered, so the template never re-derives a price and `shop`
  // never has to be narrowed there.
  const shopView = $derived(
    shop && shop.enabled
      ? {
          perCard: formatMoney(shop.per_card_cents, shop.currency),
          shipping:
            shop.shipping_cents > 0
              ? formatMoney(shop.shipping_cents, shop.currency)
              : null,
          total: formatMoney(
            shop.per_card_cents * (prefix?.card_count ?? 0) + shop.shipping_cents,
            shop.currency,
          ),
        }
      : null,
  )

  // Stripe appends ?paid=1 to the success URL. It means "you were sent back
  // here", not "you paid" — see pollForPayment.
  const cameBackFromStripe =
    typeof location !== 'undefined' && new URLSearchParams(location.search).has('paid')

  const PAYMENT_POLL_MS = 1500
  const PAYMENT_POLL_TRIES = 40

  async function refresh() {
    const [p, idx] = await Promise.all([api.getPrefix(uuid), api.listCards(uuid)])
    prefix = p
    cards = idx.cards
    rememberProject(uuid)
  }

  onMount(async () => {
    try {
      await refresh()
    } catch (e) {
      error = (e as Error).message
    } finally {
      loading = false
    }

    // Best effort — without it the page just shows no way to pay, which is also
    // what an unconfigured deployment means.
    try {
      shop = await api.shopConfig()
    } catch {
      shop = null
    }

    if (cameBackFromStripe && prefix?.status === 'payment_pending') {
      awaitingPayment = true
      void pollForPayment()
    }
  })

  /**
   * Wait for the webhook to catch up.
   *
   * Stripe's redirect and its webhook are two independent events and can arrive
   * in either order, so a page that just came back can still read
   * 'payment_pending'. That is not a failed payment, so poll for a while before
   * giving up.
   */
  async function pollForPayment() {
    for (let i = 0; i < PAYMENT_POLL_TRIES; i++) {
      await new Promise((r) => setTimeout(r, PAYMENT_POLL_MS))
      try {
        const p = await api.getPrefix(uuid)
        prefix = p
        if (p.status !== 'payment_pending') break
      } catch {
        // A transient failure is not a failed payment; keep waiting.
      }
    }
    awaitingPayment = false
  }

  async function pay() {
    paying = true
    error = null
    try {
      const session = await api.startCheckout(uuid)
      if (!session.url) throw new Error('Stripe did not return a checkout URL')
      // Leaves the page; nothing here marks the order paid.
      location.href = session.url
    } catch (e) {
      error = (e as Error).message
      paying = false
    }
  }

  async function onUploaded() {
    try {
      error = null
      await refresh()
    } catch (e) {
      error = (e as Error).message
    }
  }

  async function onDeleted(id: number) {
    try {
      await api.deleteCard(uuid, id)
      await refresh()
    } catch (e) {
      error = (e as Error).message
    }
  }

  async function copyLink() {
    try {
      await navigator.clipboard.writeText(`${location.origin}/p/${uuid}`)
      copied = true
      setTimeout(() => (copied = false), 1500)
    } catch {
      /* clipboard unavailable */
    }
  }
</script>

<div class="topbar">
  <span class="brand">ESPuino Label Printer</span>
  <span class="spacer"></span>
  <a
    href="/"
    onclick={(e) => {
      e.preventDefault()
      navigate('/')
    }}
    style="color:var(--accent);text-decoration:none">New project</a
  >
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
        <h2>Project</h2>
        <span class="badge status-{prefix.status}">{prefix.status}</span>
        <span style="flex:1"></span>
      </div>
      <div class="row" style="margin-top:12px">
        <span class="mono muted">{uuid}</span>
        <button onclick={copyLink}>{copied ? 'copied' : 'copy link'}</button>
        <span class="muted">{prefix.card_count} card{prefix.card_count === 1 ? '' : 's'}</span>
        {#if prefix.order_ref}
          <span class="mono muted">· {prefix.order_ref}</span>
        {/if}
      </div>
    </section>

    {#if (prefix.status === 'draft' || prefix.status === 'payment_pending') && shopView}
      <section class="panel">
        <div class="row">
          <h2>Pay</h2>
          <span style="flex:1"></span>
          {#if !awaitingPayment}
            <span class="muted">
              {prefix.card_count} card{prefix.card_count === 1 ? '' : 's'} × {shopView.perCard}
              {#if shopView.shipping}
                + {shopView.shipping} shipping
              {/if}
              = <strong>{shopView.total}</strong>
            </span>
          {/if}
        </div>
        <div class="row" style="margin-top:12px">
          {#if awaitingPayment}
            <span class="muted">
              <span class="spinner"></span> waiting for the payment to be confirmed…
            </span>
          {:else}
            <button class="primary" onclick={pay} disabled={paying}>
              {paying ? 'Opening checkout…' : 'Pay now'}
            </button>
            <span class="muted">
              Stripe takes the shipping address and the payment; you come straight back here.
            </span>
          {/if}
        </div>
      </section>
    {/if}

    <section class="panel">
      {#if writable}
        <Uploader prefix={uuid} onUploaded={onUploaded} />
      {:else}
        <p class="muted">
          This project is <strong>{prefix.status}</strong> and can no longer be changed.
        </p>
      {/if}
    </section>

    <section>
      <h2 style="margin-bottom:12px">Cards</h2>
      {#if cards.length === 0}
        <div class="empty">No cards yet — drop some images above.</div>
      {:else}
        <div class="cards">
          {#each cards as card (card.id)}
            <CardRow prefix={uuid} {card} {writable} onDeleted={onDeleted} />
          {/each}
        </div>
      {/if}
    </section>
  {:else}
    <div class="empty">Project not found.</div>
  {/if}
</div>
