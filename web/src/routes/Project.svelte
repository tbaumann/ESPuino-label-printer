<script lang="ts">
  import { onMount } from 'svelte'
  import {
    api,
    backIncomplete,
    billedUnits,
    formatMoney,
    RIBBONS,
    rememberProject,
    type Card,
    type Prefix,
    type Ribbon,
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

  /**
   * A ribbon chosen before the project is stored.
   *
   * Nothing has been written to this uuid yet, so there is nowhere to write the
   * choice: the first card is what creates the project, and this is applied by
   * that upload (see `onUploaded`). Choosing a ribbon is not an order, and a page
   * that is only looked at must not leave one behind — so the choice waits here
   * until there is something real to save it with.
   */
  let heldRibbon = $state<Ribbon | null>(null)

  /**
   * The ribbon the page shows: the server's, or the choice held for the
   * project's first write. Only ever the two — a card cannot exist while a
   * choice is held, so the previews always read the server's ribbon.
   */
  const ribbon = $derived(heldRibbon ?? prefix?.ribbon)

  // Amounts pre-rendered, so the template never re-derives a price and `shop`
  // never has to be narrowed there.
  //
  // A ribbon's price is the price of one *billing unit*, and the two ribbons
  // price the same order differently: YMCKO's unit is one printed side (so the
  // count is the sum over the cards, because a batch may mix single- and
  // double-sided ones), YMCKOK's is one whole card (its panel set is spent
  // whether or not the second black panel is used). That rule is `billedUnits`,
  // mirroring the server's.
  const units = $derived(
    prefix ? cards.reduce((n, c) => n + billedUnits(prefix!.ribbon, c.back), 0) : 0,
  )

  const shopView = $derived(
    shop && shop.enabled && prefix
      ? (() => {
          const price = shop.prices.find((p) => p.ribbon === prefix!.ribbon)
          if (!price) return null
          return {
            perUnit: formatMoney(price.price_cents, shop.currency),
            // What the unit is called, straight from the server: "side" or
            // "card". The UI never guesses it from the ribbon.
            unit: price.unit,
            units,
            shipping:
              shop.shipping_cents > 0
                ? formatMoney(shop.shipping_cents, shop.currency)
                : null,
            total: formatMoney(
              price.price_cents * units + shop.shipping_cents,
              shop.currency,
            ),
          }
        })()
      : null,
  )

  // One row per ribbon, in the order the shop offers them, each with its price —
  // per *unit*, and the unit is named as the server names it — and whether it is
  // currently in stock. An out-of-stock ribbon stays in this list — the customer
  // should see that it exists and is out — and is rendered disabled rather than
  // dropped.
  //
  // Without a shop config there is nothing to say about stock, so everything
  // reads as available; the server is the one that actually refuses, and a
  // chooser that greyed itself out on a failed fetch would be a lie.
  const ribbons = $derived(
    RIBBONS.map((r) => {
      const price = shop?.prices.find((p) => p.ribbon === r.ribbon)
      return {
        ...r,
        available: price ? price.available : true,
        price:
          price && shop
            ? `${formatMoney(price.price_cents, shop.currency)} per ${price.unit}`
            : null,
      }
    }),
  )

  // The ribbon the prefix is actually set to, so the panel can mark it and warn
  // when it is the one that ran out.
  const chosen = $derived(ribbons.find((r) => r.ribbon === ribbon) ?? null)

  // A card whose back side is a picture or text that has not been supplied yet
  // is an order that cannot be printed. The server refuses such a checkout with
  // 409 (it is the layer that takes the money, so it is the layer where this has
  // to hold); the button must not pretend otherwise.
  const missingBacks = $derived(cards.filter(backIncomplete).length)

  let choosing = $state<Ribbon | null>(null)

  async function chooseRibbon(next: Ribbon) {
    if (!prefix || next === ribbon) return
    error = null
    // Nothing is stored for this project yet, so there is nothing to write the
    // choice to. Hold it, and let the upload that creates the first card carry
    // it: choosing a ribbon is not worth a project in the database.
    if (cards.length === 0) {
      // Picking back the ribbon that is already stored is simply no longer a
      // choice that differs from it.
      heldRibbon = next === prefix.ribbon ? null : next
      return
    }
    choosing = next
    try {
      prefix = await api.setRibbon(uuid, next)
      // Nothing on a card changes with the ribbon, so no card can be stranded:
      // the server re-renders the rasters and repaints the back previews in the
      // new colour depth. Re-read the cards, because those previews did change.
      await refresh()
    } catch (e) {
      error = (e as Error).message
    } finally {
      choosing = null
    }
  }

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
    // The upload that just landed is this project's first write, so a ribbon
    // chosen while there was nothing to write it to can be applied now — and
    // the choice is only ever held until a card exists. The server re-renders
    // the new card's back in that depth, which is why the cards are read back a
    // second time.
    const wanted = heldRibbon
    try {
      error = null
      await refresh()
      if (wanted && cards.length > 0) {
        if (prefix && prefix.ribbon !== wanted) {
          prefix = await api.setRibbon(uuid, wanted)
          await refresh()
        }
        heldRibbon = null
      }
    } catch (e) {
      error = (e as Error).message
    }
  }

  // A card was changed by its own row (a back image uploaded, a back side
  // chosen). Only that row knows, and only the page keeps the list, so it is
  // spliced in here — which is also what moves `missingBacks` on and opens the
  // Pay button.
  function onCardUpdated(updated: Card) {
    cards = cards.map((c) => (c.id === updated.id ? updated : c))
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

<div class="app wide">
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

    <section class="panel">
      <div class="row">
        <h2>Ribbon</h2>
        <span style="flex:1"></span>
      </div>
      <div class="modes">
        {#each ribbons as r (r.ribbon)}
          <label
            class="mode"
            class:selected={ribbon === r.ribbon}
            class:unavailable={!r.available}
          >
            <input
              type="radio"
              name="print-ribbon"
              value={r.ribbon}
              checked={ribbon === r.ribbon}
              disabled={!writable || !r.available || choosing !== null}
              onchange={() => chooseRibbon(r.ribbon)}
            />
            <span class="mode-body">
              <span class="row" style="gap:8px">
                <strong>{r.label}</strong>
                {#if r.price}
                  <span class="muted">{r.price}</span>
                {/if}
                {#if !r.available}
                  <span class="badge status-cancelled">out of stock</span>
                {/if}
              </span>
              <span class="muted summary">{r.summary}</span>
              {#if !r.available}
                <span class="muted summary">
                  This ribbon is out of stock.
                </span>
              {/if}
            </span>
          </label>
        {/each}
      </div>
      {#if chosen && !chosen.available}
        <p class="muted" style="margin-top:12px">
          This project is set to <strong>{chosen.label}</strong>, whose ribbon is out
          of stock, so it cannot be paid for right now. Pick the other one, or come
          back once it is restocked.
        </p>
      {/if}
    </section>

    {#if (prefix.status === 'draft' || prefix.status === 'payment_pending') && shopView}
      <section class="panel">
        <div class="row">
          <h2>Pay</h2>
          <span style="flex:1"></span>
          {#if !awaitingPayment}
            <span class="muted">
              {shopView.units} {shopView.unit}{shopView.units === 1 ? '' : 's'} × {shopView.perUnit}
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
          {:else if chosen && !chosen.available}
            <!-- The server refuses this too (409), so the button must not
                 pretend it would work: an unavailable ribbon cannot print. -->
            <button class="primary" disabled>Pay now</button>
            <span class="muted">
              {chosen.label} needs a ribbon we are out of. Choose an available option above.
            </span>
          {:else if missingBacks > 0}
            <!-- Same rule at both layers: the checkout answers 409 while a
                 double-sided card has no back image or back text yet. -->
            <button class="primary" disabled>Pay now</button>
            <span class="muted">
              {missingBacks} card{missingBacks === 1 ? '' : 's'} still need
              {missingBacks === 1 ? 'its' : 'their'} back finished — the picture or
              the text the back is printed from.
            </span>
          {:else if cards.length === 0}
            <!-- Nothing to print yet, and nothing the server would accept: a
                 card-less checkout is a 409. -->
            <button class="primary" disabled>Pay now</button>
            <span class="muted">Add a card first — there is nothing to print yet.</span>
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
        <Uploader prefix={uuid} ribbon={prefix.ribbon} onUploaded={onUploaded} />
      {:else}
        <p class="muted">
          This project is <strong>{prefix.status}</strong> and can no longer be changed.
        </p>
      {/if}
    </section>

    <section>
      <div class="row" style="margin-bottom:12px">
        <h2>Cards</h2>
        <span style="flex:1"></span>
        {#if missingBacks > 0}
          <span class="badge status-cancelled">
            {missingBacks} without a finished back
          </span>
        {/if}
      </div>
      {#if cards.length === 0}
        <div class="empty">No cards yet - drop some images above.</div>
      {:else}
        <div class="cards">
          {#each cards as card (card.id)}
            <CardRow
              prefix={uuid}
              {card}
              ribbon={prefix.ribbon}
              {writable}
              onDeleted={onDeleted}
              onCardUpdated={onCardUpdated}
            />
          {/each}
        </div>
      {/if}
    </section>
  {:else}
    <div class="empty">Project not found.</div>
  {/if}
</div>
