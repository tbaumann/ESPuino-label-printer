<script lang="ts">
  import type { Snippet } from 'svelte'
  import { api, type Card } from '../api'

  // The field column every print mode shares: name, serial number, the save
  // state and the delete button.
  //
  // What differs per mode is the *back side* — its preview and its editor — and
  // each mode's component hands that in as `backField`, so the shared fields
  // and their debounced save live in one place while the mode-specific inputs
  // stay in the mode's own file.
  let {
    prefix,
    card,
    writable,
    readonly = false,
    onDeleted,
    backField,
  }: {
    prefix: string
    card: Card
    writable: boolean
    /** Preview only: values as text, no editing and no delete. The admin UI
     * never modifies cards, it only wants the print preview. */
    readonly?: boolean
    onDeleted?: (id: number) => void
    /** The mode's own back-side field(s), rendered under the shared ones. */
    backField?: Snippet
  } = $props()

  // This row owns an editable copy of the shared fields. The parent only
  // re-fetches the list after an upload/delete (never after an edit), and rows
  // are keyed by card id, so these initial values stay in step; edits resync
  // from the PATCH response below.
  // svelte-ignore state_referenced_locally
  let name = $state(card.name)
  // svelte-ignore state_referenced_locally
  let serial = $state(card.serial_number ?? '')

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

  // Only the shared fields: the back side is the mode component's to write, and
  // a PATCH carrying every field would fight it over the same card.
  async function save() {
    saveState = 'saving'
    try {
      const updated = await api.patchCard(prefix, card.id, {
        name,
        serial_number: serial,
      })
      name = updated.name
      serial = updated.serial_number ?? ''
      saveState = 'saved'
    } catch (e) {
      errMsg = (e as Error).message
      saveState = 'error'
    }
  }
</script>

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

    {@render backField?.()}
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

    {@render backField?.()}
  </div>

  <div class="fields">
    <button class="danger" onclick={() => onDeleted?.(card.id)} disabled={!writable}>Delete</button>
  </div>
{/if}
