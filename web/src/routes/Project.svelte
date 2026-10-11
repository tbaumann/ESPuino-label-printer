<script lang="ts">
  import { onMount } from 'svelte'
  import { api, rememberProject, type Card, type Prefix } from '../lib/api'
  import { navigate } from '../lib/router'
  import Uploader from '../lib/Uploader.svelte'
  import CardRow from '../lib/CardRow.svelte'

  let { uuid }: { uuid: string } = $props()

  let prefix = $state<Prefix | null>(null)
  let cards = $state<Card[]>([])
  let error = $state<string | null>(null)
  let loading = $state(true)
  let submitting = $state(false)
  let copied = $state(false)

  const WRITABLE: string[] = ['draft', 'submitted', 'paid', 'queued']
  const writable = $derived(prefix ? WRITABLE.includes(prefix.status) : false)

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
  })

  async function submit() {
    submitting = true
    error = null
    try {
      prefix = await api.submitPrefix(uuid)
    } catch (e) {
      error = (e as Error).message
    } finally {
      submitting = false
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
        {#if prefix.status === 'draft'}
          <button class="primary" onclick={submit} disabled={submitting}>
            {submitting ? 'Submitting…' : 'Submit for printing'}
          </button>
        {/if}
      </div>
      <div class="row" style="margin-top:12px">
        <span class="mono muted">{uuid}</span>
        <button onclick={copyLink}>{copied ? 'copied' : 'copy link'}</button>
        <span class="muted">{prefix.card_count} card{prefix.card_count === 1 ? '' : 's'}</span>
      </div>
    </section>

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
