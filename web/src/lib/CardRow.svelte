<script lang="ts">
  import {
    api,
    backDepth,
    BACK_SOURCES,
    DEPTH_NOTE,
    type BackSource,
    type Card,
    type CardPatch,
    type Ribbon,
  } from './api'
  import { hasImageSyntax } from './backText'
  import CardFields from './cards/CardFields.svelte'

  // One card: its header, its two sides, and the back choice that decides what
  // the second one is.
  //
  // The card's back is entirely its own choice — nothing at all (a single-sided
  // card), a copy of the front, a second uploaded picture, or markdown. The
  // *ribbon* changes nothing about the choice; it decides only the colour depth
  // the chosen back is rendered in, which the preview shows and the caption
  // says.
  //
  // Two controls, deliberately apart: which back the card *has* is chosen in the
  // editor beside the pane (it is the same choice for every card, so it belongs
  // with the other fields), while the picture that an `image` back needs is
  // chosen *in* the pane — that pane is the picture, so the file chooser sits on
  // it, and the whole pane is a drop area for one.
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

  // The colour depth the server will render a back in. A property of the
  // ribbon, so the preview can be honest about it.
  const depth = $derived(backDepth(ribbon))

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
  // True while the back *source* itself is still being written. The text may
  // not be sent before the server knows the back is markdown — it refuses text
  // on any other source — so the editor waits for it.
  let changingSource = $state(false)
  let timer: ReturnType<typeof setTimeout> | undefined
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

  // Not a rendering rule, a courtesy: the markdown back renderer drops an image
  // construct whole, so the author is told instead of left wondering where their
  // artwork went. (The same courtesy as the black-and-white note on an `image`
  // back: a choice can be a poor one without being refused.)
  const imageRefused = $derived(back === 'markdown' && hasImageSyntax(backText))

  /** Apply a card patch, keeping the local state in step with the response. */
  async function apply(patch: CardPatch) {
    errMsg = ''
    try {
      const updated = await api.patchCard(prefix, card.id, patch)
      back = updated.back
      backText = updated.back_text ?? ''
      hasBackImage = !!updated.back_image_content_type
      onCardUpdated?.(updated)
      return true
    } catch (e) {
      errMsg = (e as Error).message
      saveState = 'error'
      return false
    }
  }

  /** Choose what this card's back is. One write, one source. */
  async function choose(next: BackSource) {
    if (!writable || next === back || changingSource) return
    clearTimeout(timer)
    const previous = back
    back = next // optimistic, so the editor for it appears at once
    changingSource = true
    const ok = await apply({ back: next })
    changingSource = false
    if (!ok) back = previous
  }

  function scheduleText() {
    if (!writable || back !== 'markdown' || changingSource) return
    clearTimeout(timer)
    saveState = 'idle'
    timer = setTimeout(saveText, 600)
  }

  async function saveText() {
    saveState = 'saving'
    const ok = await apply({ back_text: backText })
    saveState = ok ? 'saved' : 'error'
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
      backText = updated.back_text ?? ''
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

  /** The caption under the back pane: what will actually be printed there. */
  const backCaption = $derived(
    back === 'front'
      ? `back · the front again, ${DEPTH_NOTE[depth]}`
      : back === 'markdown'
        ? `back · text, ${DEPTH_NOTE[depth]}`
        : `back · the picture, ${DEPTH_NOTE[depth]}`,
  )
</script>

<article class="card" class:preview-only={readonly}>
  <CardFields {prefix} {card} {writable} {readonly} {onDeleted} />

  <div class="image-row">
    <div class="side">
      <div class="preview">
        <img src={api.cardFrontDisplayUrl(prefix, card.id, card.updated_at)} alt={`front of ${card.name}`} />
      </div>
      <div class="caption muted">front · full colour</div>
    </div>

    {#if back === 'none'}
      <!-- Nothing is printed on the back of a single-sided card, so there is no
           back pane at all: one would imply a side that does not exist. -->
      <div class="side no-back-side muted"></div>
    {:else}
      <div class="side">
        <div class="preview back" class:droppable={pickOnClick} class:over>
          <div class="back-body">
            {#if rendered === 'ready'}
              <!-- The one artifact a back has, whatever its source is: the
                   server's `display/back`, at the ribbon's depth. The row never
                   draws a back itself, so the pane cannot promise a card the
                   printer would not produce. -->
              <img
                class="backart"
                src={api.cardBackDisplayUrl(prefix, card.id, `${card.updated_at}-${ribbon}`)}
                alt={`back of ${card.name}`}
              />
            {:else if rendered === 'failed'}
              <!-- The server could not render this back (the text renderer is
                   still a stub), so the card is visibly not printable rather
                   than showing a picture that would never print. -->
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
            <!-- The whole pane is a drop area: a button laid over it, so a
                 picture dropped anywhere on it lands. While the pane is empty it
                 is the click-to-choose affordance as well. -->
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
        <div class="caption muted">{backCaption}</div>

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

        {#if back === 'markdown'}
          <label for={`back-${card.id}`}>Back text (markdown)</label>
          <textarea
            id={`back-${card.id}`}
            rows="5"
            bind:value={backText}
            oninput={scheduleText}
            disabled={!writable || changingSource}
            placeholder="song list"
          ></textarea>
          <div class="hint muted">
            {#if depth === 'color'}
              Printed on the back in colour: headings, bold, italic and lists.
            {:else}
              Printed on the back in black only: headings, bold, italic and lists.
              No colour, no images, no greys.
            {/if}
          </div>
          {#if imageRefused}
            <div class="hint warn">
              This text contains image markup, which the text back never prints —
              the whole <code>![…]</code> construct is dropped, alt text included.
              Choose “Second image” if the card should carry a picture.
            </div>
          {/if}
        {:else if back === 'image'}
          <div class="hint muted">
            {#if depth === 'color'}
              Printed on the back in full colour.
            {:else}
              Printed on the back in black and white — this project's ribbon has no
              colour panel for the back, so the picture is reduced to pure black
              and white. A colour-back project keeps it in colour.
            {/if}
            Every double-sided card needs one before the project can be paid for —
            choose it in the pane, or drop a picture on the pane.
          </div>
        {:else if back === 'front'}
          <div class="hint muted">
            Nothing to supply: the back is the front picture again.
          </div>
        {:else}
          <div class="hint muted">Nothing to supply: a single-sided card has no back.</div>
        {/if}

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
    {/if}
  </div>
</article>
