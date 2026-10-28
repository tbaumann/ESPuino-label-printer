<script lang="ts">
  import { api, type Card } from '../api'

  // The top row every card shares: number, name, the save state and the delete
  // button. The mode-specific *back side* (its preview and its editor) is
  // rendered in the image row by the mode's own component — this row only owns
  // the shared header field (name) and its debounced save.
  //
  // The serial number is deliberately not surfaced here: it is an API-level
  // data field, not something the customer needs to edit or read in the UI.
  let {
    prefix,
    card,
    writable,
    readonly = false,
    onDeleted,
  }: {
    prefix: string
    card: Card
    writable: boolean
    /** Preview only: values as text, no editing and no delete. The admin UI
     * never modifies cards, it only wants the print preview. */
    readonly?: boolean
    onDeleted?: (id: number) => void
  } = $props()

  // This row owns an editable copy of the name. The parent only re-fetches the
  // list after an upload/delete (never after an edit), and rows are keyed by
  // card id, so these initial values stay in step; edits resync from the PATCH
  // response below.
  // svelte-ignore state_referenced_locally
  let name = $state(card.name)

  // NB: do not name this `state` — a bare `state` variable collides with the
  // `$state` rune, which stops the component being compiled in runes mode.
  let saveState = $state<'idle' | 'saving' | 'saved' | 'error'>('idle')
  let errMsg = $state('')
  let timer: ReturnType<typeof setTimeout> | undefined
  // Typed, not yet written. A write goes out when the customer stops typing or
  // leaves the field, never mid-word.
  let pending = false
  const NAME_IDLE_MS = 1200

  function schedule() {
    if (!writable) return
    pending = true
    clearTimeout(timer)
    saveState = 'idle'
    timer = setTimeout(() => void save(), NAME_IDLE_MS)
  }

  /** Write the name now — leaving the field, or when typing has stopped. */
  async function flush() {
    clearTimeout(timer)
    await save()
  }

  // Only the shared name field: the back side is the mode component's to write,
  // and a PATCH carrying every field would fight it over the same card. The
  // serial number is not edited from the UI, so it is not part of this PATCH
  // either (it lives on in the model and stays settable via the API).
  async function save() {
    if (!pending || !writable) return
    const sent = name
    pending = false
    saveState = 'saving'
    try {
      // The response is deliberately not copied back into the field — it is the
      // name that was sent, and the customer may have typed on since; echoing it
      // is what used to eat those keystrokes.
      await api.patchCard(prefix, card.id, { name: sent })
      saveState = 'saved'
    } catch (e) {
      errMsg = (e as Error).message
      saveState = 'error'
      // Still unwritten: the next keystroke, or leaving the field, retries it.
      pending = true
    }
  }
</script>

<div class="card-head">
  <span class="card-id">#{card.id}</span>

  {#if readonly}
    <div class="head-field">
      <div class="field-label">Name</div>
      <div class="value">{name}</div>
    </div>
  {:else}
    <div class="head-field">
      <label for={`name-${card.id}`}>Name</label>
      <input
        id={`name-${card.id}`}
        type="text"
        bind:value={name}
        oninput={schedule}
        onblur={flush}
        disabled={!writable}
      />
    </div>
  {/if}

  <span style="flex:1"></span>

  {#if !readonly}
    {#if saveState === 'saving'}
      <span class="save-state"><span class="spinner"></span> saving…</span>
    {:else if saveState === 'saved'}
      <span class="save-state">saved</span>
    {:else if saveState === 'error'}
      <span class="save-state" style="color:var(--danger)">{errMsg}</span>
    {/if}
    <button class="danger" onclick={() => onDeleted?.(card.id)} disabled={!writable}>Delete</button>
  {/if}
</div>
