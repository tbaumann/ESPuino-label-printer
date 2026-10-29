<script lang="ts">
  import {
    api,
    BACK_SOURCES,
    type BackSource,
    type Card,
    type CardPatch,
    type Ribbon,
  } from './api'
  import CardFields from './cards/CardFields.svelte'
  import MarkdownEditor from './cards/MarkdownEditor.svelte'

  let {
    prefix,
    card,
    ribbon,
    writable,
    readonly = false,
    onDeleted,
    onCardUpdated,
  }: {
    prefix: string
    card: Card
    /** The prefix's ribbon, which decides what a back may be. */
    ribbon: Ribbon
    writable: boolean
    /** Preview only: values as text, no editing and no delete. The admin UI
     * never modifies cards, it only wants the print preview. */
    readonly?: boolean
    onDeleted?: (id: number) => void
    /** A card the row changed by itself, so the page can re-count what is still
     * incomplete. */
    onCardUpdated?: (card: Card) => void
  } = $props()

  // Every back source, in the order the shop lists them — the same four
  // whatever the ribbon is.
  const choices = BACK_SOURCES

  // This row owns its editable copy of the back, resyncing from the PATCH
  // response. Rows are keyed by card id and the parent only re-fetches after an
  // upload or delete, so these initial values stay in step.
  // svelte-ignore state_referenced_locally
  let back = $state<BackSource>(card.back)
  // svelte-ignore state_referenced_locally
  let backText = $state(card.back_text ?? '')
  // svelte-ignore state_referenced_locally
  let hasBackImage = $state(!!card.back_image_content_type)

  let busy = $state(false)
  let over = $state(false)
  let errMsg = $state('')
  let saveState = $state<'idle' | 'saving' | 'saved' | 'error'>('idle')
  // True while the back *source* itself is still being written. The text box
  // exists only for a markdown back, and a text write waits until the server
  // has that source, so the two never race.
  let changingSource = $state(false)
  let timer: ReturnType<typeof setTimeout> | undefined
  // Text the server has not been told about yet. A write goes out when the
  // customer stops typing or leaves the box — never mid-sentence, because every
  // write of a card re-renders its rasters.
  let textPending = false
  const TEXT_IDLE_MS = 1200
  // $state because the input is bound under a condition, so the reference is
  // re-assigned when the pane becomes (or stops being) an image back.
  let fileInput = $state<HTMLInputElement | undefined>()

  // What the pane has to show for the back this row *has*. The server owns every
  // back artifact: the picture, a copy of the front and the rendered text all
  // come from the one `display/back` route, re-rendered at the ribbon's depth —
  // so the row shows that artifact's status rather than drawing a back itself.
  // While a source write is in flight the server still holds the previous back,
  // so its artifact is suppressed rather than standing in for the new one.
  const rendered = $derived(changingSource ? undefined : card.display_back)

  // An `image` back is the only one with a picture to choose or replace, so it
  // is the only one whose pane takes a file. Dropping onto artwork already there
  // is a deliberate "use this instead".
  const dropTarget = $derived(!readonly && writable && back === 'image')

  // Clicking the pane opens the picker only while it holds no picture: an empty
  // area has no other affordance, whereas a pane showing artwork would open a
  // file dialog on a stray click. A filled pane is replaced through the chooser
  // in it, which is right there.
  const pickOnClick = $derived(dropTarget && !hasBackImage)

  /**
   * Write a patch to this card, and hand the response to the page.
   *
   * Deliberately does NOT copy the response back into the fields being edited:
   * the server stores what it was sent, so an echo can only overwrite what the
   * customer typed while the request was in flight — which is exactly how the
   * text editor used to eat keystrokes.
   */
  async function patch(p: CardPatch): Promise<Card | null> {
    errMsg = ''
    try {
      const updated = await api.patchCard(prefix, card.id, p)
      onCardUpdated?.(updated)
      return updated
    } catch (e) {
      errMsg = (e as Error).message
      saveState = 'error'
      return null
    }
  }

  /** Choose what this card's back is. One write, one source. */
  async function choose(next: BackSource) {
    if (!writable || next === back || changingSource) return
    // Text typed but not yet written goes out first: a card keeps its text
    // across a source change, so leaving it behind would lose it.
    await flushText()
    const previous = back
    back = next // optimistic, so the editor for it appears at once
    changingSource = true
    const ok = (await patch({ back: next })) !== null
    changingSource = false
    if (!ok) back = previous
  }

  /** Whether this row may write text at all: a draft, whose back is markdown. */
  const writingText = () => writable && back === 'markdown' && !changingSource

  /** The editor's text changed: note it, and arm a write for when typing stops. */
  function onTextChange(markdown: string) {
    backText = markdown
    if (!writingText()) return
    textPending = true
    clearTimeout(timer)
    saveState = 'idle'
    timer = setTimeout(() => void saveText(), TEXT_IDLE_MS)
  }

  /** Write the text now — leaving the box, or moving to another back source. */
  async function flushText() {
    clearTimeout(timer)
    await saveText()
  }

  /** The one writer of `back_text`: whatever is in the box right now. */
  async function saveText() {
    if (!textPending || !writingText()) return
    const sent = backText
    textPending = false
    saveState = 'saving'
    const updated = await patch({ back_text: sent })
    if (updated) saveState = 'saved'
    // Still unwritten: the next keystroke, or leaving the box, retries it.
    else textPending = true
  }

  /**
   * Upload a picture as this card's back.
   *
   * Attaching one *is* saying the back is that picture, so the server sets the
   * source along with it.
   */
  async function uploadImage(file: File) {
    busy = true
    errMsg = ''
    try {
      const updated = await api.uploadBackImage(prefix, card.id, file)
      hasBackImage = !!updated.back_image_content_type
      back = updated.back
      onCardUpdated?.(updated)
    } catch (e) {
      errMsg = (e as Error).message
    } finally {
      busy = false
    }
  }

  function chooseImage(e: Event) {
    const input = e.currentTarget as HTMLInputElement
    const file = input.files?.[0]
    // Reset first: re-picking the same file must fire `change` again.
    input.value = ''
    if (file) void uploadImage(file)
  }

  const IMAGE_RE = /\.(png|jpe?g|webp|gif)$/i
  const isImage = (f: File) => f.type.startsWith('image/') || IMAGE_RE.test(f.name)

  function onDragOver(e: DragEvent) {
    if (!dropTarget || busy) return
    e.preventDefault() // without this the drop event never fires
    over = true
  }

  async function onDrop(e: DragEvent) {
    if (!dropTarget || busy) return
    e.preventDefault()
    over = false
    const file = Array.from(e.dataTransfer?.files ?? []).find(isImage)
    if (file) await uploadImage(file)
    else errMsg = 'That is not an image (png, jpg, webp or gif).'
  }
</script>

<article class="card" class:preview-only={readonly}>
  <CardFields {prefix} {card} {writable} {readonly} {onDeleted} />

  <div class="image-row">
    <div class="side">
      <div class="preview">
        <img src={api.cardFrontDisplayUrl(prefix, card.id, card.updated_at)} alt={`front of ${card.name}`} />
      </div>
    </div>

    <!-- A single-sided card has no back to show, and no column reserved for one:
         the chooser takes the place the back pane would have had rather than
         leaving a pane-wide gap between the two. -->
    {#if back !== 'none'}
      <div class="side">
        <div class="preview back" class:droppable={pickOnClick} class:over>
          <div class="back-body">
            {#if rendered === 'ready'}
              <img
                class="backart"
                src={api.cardBackDisplayUrl(prefix, card.id, `${card.updated_at}-${ribbon}`)}
                alt={`back of ${card.name}`}
              />
            {:else if rendered === 'failed'}
              <div class="no-back-image">
                <span>this back is not printable yet</span>
              </div>
            {:else if back === 'image' && !hasBackImage}
              <div class="no-back-image">
                <span>no back image yet</span>
                {#if pickOnClick}
                  <span class="hint">drop a picture here, or click to choose one</span>
                {/if}
              </div>
            {:else}
              <div class="no-back-image">
                <span>nothing rendered for this back yet</span>
              </div>
            {/if}
          </div>

          {#if dropTarget}
            <button
              class="back-drop"
              type="button"
              aria-label="Upload the back image"
              disabled={busy || changingSource}
              onclick={() => pickOnClick && fileInput?.click()}
              ondragover={onDragOver}
              ondragleave={() => (over = false)}
              ondrop={onDrop}
            ></button>
          {/if}
        </div>

        {#if back === 'image' && !readonly}
          <!-- The file chooser sits *under* the pane, with the caption: the pane
               is the picture and stays unobstructed, while the whole of it
               remains a drop area — the chooser is not in the pane's way. -->
          <div class="back-choose">
            <label for={`back-image-${card.id}`}>Back image</label>
            <input
              bind:this={fileInput}
              id={`back-image-${card.id}`}
              type="file"
              accept="image/*"
              onchange={chooseImage}
              disabled={!writable || busy || changingSource}
            />
          </div>
        {/if}
      </div>
    {/if}

    {#if !readonly}
      <div class="side editor">
        <div class="field-label">Back side</div>
        <div class="back-choices">
          {#each choices as c (c.source)}
            <label class="back-choice" class:selected={back === c.source}>
              <input
                type="radio"
                name={`back-${card.id}`}
                value={c.source}
                checked={back === c.source}
                disabled={!writable || changingSource}
                onchange={() => choose(c.source)}
              />
              <span>
                <strong>{c.label}</strong>
                <span class="hint muted">{c.summary}</span>
              </span>
            </label>
          {/each}
        </div>

        <div class="row" style="margin-top:6px">
          {#if busy || changingSource}
            <span class="save-state"><span class="spinner"></span> saving…</span>
          {:else if saveState === 'saving'}
            <span class="save-state"><span class="spinner"></span> saving…</span>
          {:else if saveState === 'saved'}
            <span class="save-state">saved</span>
          {/if}
          {#if errMsg}
            <span class="save-state" style="color:var(--danger)">{errMsg}</span>
          {/if}
        </div>
      </div>

      {#if back === 'markdown'}
        <!-- The text is a column of its own, beside the choice rather than under
             it: on a markdown back the box is the thing being worked in, and a
             narrow one below four option rows is unusable. -->
        <div class="side editor text-editor">
          <div class="field-label">Back text (markdown)</div>
          <MarkdownEditor
            value={backText}
            placeholder="song list"
            disabled={!writable || changingSource}
            onchange={onTextChange}
            onblur={flushText}
          />
        </div>
      {/if}
    {/if}
  </div>
</article>
