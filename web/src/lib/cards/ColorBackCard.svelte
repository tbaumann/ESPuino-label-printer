<script lang="ts">
  import { api, needsBackImage, type Card } from '../api'
  import CardFields from './CardFields.svelte'

  // `double_sided_color` — a full-colour front and a full-colour back.
  //
  // The back side is an *uploaded image*, not text: that is the whole
  // difference from the black-back mode, and the reason this is its own
  // component. A card exists from its front image alone (the dropzone uploads a
  // folder at a time), so the back starts out missing and is added here, per
  // card; the pane says so plainly rather than showing an empty rectangle that
  // looks like artwork.
  let {
    prefix,
    card,
    writable,
    readonly = false,
    onDeleted,
    onUpdated,
  }: {
    prefix: string
    card: Card
    writable: boolean
    readonly?: boolean
    onDeleted?: (id: number) => void
    /** Told about a new back image, so the page can re-count incomplete cards. */
    onUpdated?: (card: Card) => void
  } = $props()

  // Initial values on purpose: this row owns its copy from here on, and the
  // upload below is the one thing that changes them.
  // svelte-ignore state_referenced_locally
  let hasBack = $state(!needsBackImage(card))
  // The back-image URL is stable across a re-upload, so it is versioned with
  // the card's `updated_at`: without it the browser keeps serving the previous
  // back from its cache after a replacement.
  // svelte-ignore state_referenced_locally
  let version = $state(card.updated_at)

  let busy = $state(false)
  let errMsg = $state('')
  let backInput: HTMLInputElement

  async function choose(e: Event) {
    const input = e.currentTarget as HTMLInputElement
    const file = input.files?.[0]
    // Reset first: re-picking the same file must fire `change` again.
    input.value = ''
    if (!file) return
    busy = true
    errMsg = ''
    try {
      const updated = await api.uploadBackImage(prefix, card.id, file)
      hasBack = !needsBackImage(updated)
      version = updated.updated_at
      onUpdated?.(updated)
    } catch (e) {
      errMsg = (e as Error).message
    } finally {
      busy = false
    }
  }
</script>

<article class="card" class:preview-only={readonly}>
  <div>
    <div class="preview">
      <img src={api.cardImageUrl(prefix, card.id)} alt={`front of ${card.name}`} />
    </div>
    <div class="caption muted">front · full colour</div>

    <div class="preview back color-back" style="margin-top:10px">
      {#if hasBack}
        <img
          class="backart"
          src={api.cardBackImageUrl(prefix, card.id, version)}
          alt={`colour back of ${card.name}`}
        />
      {:else}
        <span class="no-back-image">no back image yet</span>
      {/if}
    </div>
    <div class="caption muted">back · full colour</div>
  </div>

  <CardFields {prefix} {card} {writable} {readonly} {onDeleted}>
    {#snippet backField()}
      {#if readonly}
        <div>
          <div class="field-label">Back image</div>
          <div class="value">{hasBack ? 'uploaded' : 'none'}</div>
        </div>
      {:else}
        <div>
          <label for={`back-image-${card.id}`}>Back image</label>
          <input
            id={`back-image-${card.id}`}
            type="file"
            accept="image/*"
            onchange={choose}
            disabled={!writable || busy}
          />
          <div class="row" style="margin-top:6px">
            {#if busy}
              <span class="save-state"><span class="spinner"></span> uploading…</span>
            {/if}
            {#if errMsg}
              <span class="save-state" style="color:var(--danger)">{errMsg}</span>
            {/if}
          </div>
          <div class="hint muted">
            Printed on the back in full colour. Every card needs one before the
            project can be paid for.
          </div>
        </div>
      {/if}
    {/snippet}
  </CardFields>
</article>
