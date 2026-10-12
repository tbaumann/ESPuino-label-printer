<script lang="ts">
  import { api, type Card } from './api'
  import { renderBackText } from './backText'

  let {
    prefix,
    card,
    writable,
    onDeleted,
    readonly = false,
  }: {
    prefix: string
    card: Card
    writable: boolean
    onDeleted?: (id: number) => void
    /** Preview only: values as text, no editing and no delete. The admin UI
     * never modifies cards, it only wants the print preview. */
    readonly?: boolean
  } = $props()

  // This row owns an editable copy of the fields. The parent only re-fetches
  // the list after an upload/delete (never after an edit), and rows are keyed
  // by card id, so these initial values stay in step; edits resync from the
  // PATCH response below.
  // svelte-ignore state_referenced_locally
  let name = $state(card.name)
  // svelte-ignore state_referenced_locally
  let serial = $state(card.serial_number ?? '')
  // svelte-ignore state_referenced_locally
  let back = $state(card.back_text ?? '')

  // The card-back preview renders `back` as markdown. The output has been
  // through the sanitiser (renderBackText), so `{@html}` on it is safe.
  const backHtml = $derived(back ? renderBackText(back) : '')

  // NB: do not name this `state` — a bare `state` variable collides with the
  // `$state` rune, which stops the component being compiled in runes mode.
  let saveState = $state<'idle' | 'saving' | 'saved' | 'error'>('idle')
  let errMsg = $state('')
  let timer: ReturnType<typeof setTimeout> | undefined

  function schedule() {
    if (!writable) return
    clearTimeout(timer)
    saveState = 'idle'
    timer = setTimeout(save, 600)
  }

  async function save() {
    saveState = 'saving'
    try {
      const updated = await api.patchCard(prefix, card.id, {
        name,
        serial_number: serial,
        back_text: back,
      })
      name = updated.name
      serial = updated.serial_number ?? ''
      back = updated.back_text ?? ''
      saveState = 'saved'
    } catch (e) {
      errMsg = (e as Error).message
      saveState = 'error'
    }
  }
</script>

<article class="card" class:preview-only={readonly}>
  <div>
    <div class="preview">
      <img src={api.cardImageUrl(prefix, card.id)} alt={`front of ${name}`} />
    </div>
    <div class="caption muted">front</div>

    <div class="preview back" style="margin-top:10px">
      <div class="halfpanel">{@html backHtml || '—'}</div>
    </div>
    <div class="caption muted">back · B&amp;W half panel</div>
  </div>

  {#if readonly}
    <div class="fields">
      <div class="card-head"><span class="card-id">#{card.id}</span></div>

      <div>
        <div class="field-label">Name</div>
        <div class="value">{name}</div>
      </div>

      <div>
        <div class="field-label">Serial number</div>
        <div class="value mono">{serial || '—'}</div>
      </div>

      <div>
        <div class="field-label">Back text</div>
        <pre class="value backtext">{back || '—'}</pre>
      </div>
    </div>
  {:else}
    <div class="fields">
      <div class="card-head">
        <span class="card-id">#{card.id}</span>
        {#if saveState === 'saving'}
          <span class="save-state"><span class="spinner"></span> saving…</span>
        {:else if saveState === 'saved'}
          <span class="save-state">saved</span>
        {:else if saveState === 'error'}
          <span class="save-state" style="color:var(--danger)">{errMsg}</span>
        {/if}
      </div>

      <div>
        <label for={`name-${card.id}`}>Name</label>
        <input id={`name-${card.id}`} type="text" bind:value={name} oninput={schedule} disabled={!writable} />
      </div>

      <div>
        <label for={`serial-${card.id}`}>Serial number</label>
        <input
          id={`serial-${card.id}`}
          type="text"
          bind:value={serial}
          oninput={schedule}
          disabled={!writable}
          placeholder="optional"
        />
      </div>

      <div>
        <label for={`back-${card.id}`}>Back text </label>
        <textarea
          id={`back-${card.id}`}
          rows="5"
          bind:value={back}
          oninput={schedule}
          disabled={!writable}
          placeholder="song list"
        ></textarea>
      </div>
    </div>

    <div class="fields">
      <button class="danger" onclick={() => onDeleted?.(card.id)} disabled={!writable}>Delete</button>
    </div>
  {/if}
</article>
