<script lang="ts">
  import {
    api,
    backDepth,
    ribbonAllowsBack,
    BACK_SOURCES,
    type BackSource,
    type Card,
    type CardPatch,
    type Ribbon,
  } from './api'
  import { hasImageSyntax, renderBackText } from './backText'
  import CardFields from './cards/CardFields.svelte'

  // One card: its header, its two sides, and the back choice that decides what
  // the second one is.
  //
  // The *job* fixes the ribbon, and the ribbon fixes what a back can be: YMCKO's
  // back is a colour panel set (any source), YMCKOKO's is the second black panel
  // (text only). Within that, the card's back is its own choice — nothing at all
  // (a single-sided card), a copy of the front, a second uploaded picture, or
  // markdown.
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

  // The choices this ribbon can print, in the order the shop lists them. An
  // ineligible one is not shown at all: unlike an out-of-stock ribbon (which is
  // shown greyed so the customer knows it exists), a back this ribbon physically
  // cannot make is not an offer that is temporarily out of stock.
  const choices = $derived(BACK_SOURCES.filter((c) => ribbonAllowsBack(ribbon, c.source)))

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

  // An `image` back is the only one with a picture to choose or replace, so it
  // is the only one whose pane takes a file. Dropping onto artwork already there
  // is a deliberate "use this instead".
  const dropTarget = $derived(!readonly && writable && back === 'image')

  // Clicking the pane opens the picker only while it holds no picture: an empty
  // area has no other affordance, whereas a pane showing artwork would open a
  // file dialog on a stray click. A filled pane is replaced through the chooser
  // in it, which is right there.
  const pickOnClick = $derived(dropTarget && !hasBackImage)

  // The preview renders the markdown through the sanitiser, so `{@html}` on it
  // is safe. What the *printer* gets is the server's raster; this is the editing
  // preview, and the two are kept in step by `backText.ts`.
  const backHtml = $derived(backText ? renderBackText(backText) : '')

  // Not a rendering rule, a courtesy: on a black-only back the renderer drops an
  // image whole, so the author is told instead of left wondering where their
  // artwork went. On a colour back there is nothing to warn about.
  const imageRefused = $derived(
    back === 'markdown' && depth === 'black' && hasImageSyntax(backText),
  )

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
      ? depth === 'color'
        ? 'back · the front, printed again in full colour'
        : 'back · the front, printed again'
      : back === 'markdown'
        ? depth === 'color'
          ? 'back · colour'
          : 'back · black only (1 bit)'
        : 'back · full colour',
  )
</script>

<article class="card" class:preview-only={readonly}>
  <CardFields {prefix} {card} {writable} {readonly} {onDeleted} />

  <div class="image-row">
    <div class="side">
      <div class="preview">
        <img src={api.cardDisplayUrl(prefix, card.id)} alt={`front of ${card.name}`} />
      </div>
      <div class="caption muted">front · full colour</div>
    </div>

    {#if back === 'none'}
      <!-- Nothing is printed on the back of a single-sided card, so there is no
           back pane at all: one would imply a side that does not exist. -->
      <div class="side no-back-side muted"></div>
    {:else}
      <div class="side">
        <div
          class="preview back"
          class:black-only={back === 'markdown'}
          class:color-ink={back === 'markdown' && depth === 'color'}
          class:color-back={back === 'image'}
          class:droppable={pickOnClick}
          class:over
        >
          <div class="back-body">
            {#if back === 'markdown'}
              <!-- The preview has to be honest about the medium: on a black-only
                   back the pane renders paper white with pure black ink (never
                   the theme's colours, never a grey the printer cannot make). -->
              <div class="panel">{@html backHtml || '—'}</div>
            {:else if back === 'image'}
              {#if hasBackImage}
                <img
                  class="backart"
                  src={api.cardDisplayBackUrl(prefix, card.id)}
                  alt={`back of ${card.name}`}
                />
              {:else}
                <div class="no-back-image">
                  <span>no back image yet</span>
                  {#if pickOnClick}
                    <span class="hint">drop a picture here, or click to choose one</span>
                  {/if}
                </div>
              {/if}
            {:else}
              <!-- A copy of the front: the same panel, so it is shown as the same
                   picture rather than a blank pane that looks like missing
                   artwork. -->
              <img
                class="backart"
                src={api.cardDisplayUrl(prefix, card.id)}
                alt={`back of ${card.name}, the same as its front`}
              />
            {/if}
          </div>

          {#if back === 'image' && !readonly}
            <!-- The file chooser lives in the pane, over the drop target so it
                 stays clickable. -->
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
              This text contains image markup, which the black-only back cannot
              print — the whole <code>![…]</code> construct is dropped, alt text
              included. Choose “Second image”, or move the card to a colour-back
              project, for a picture.
            </div>
          {/if}
        {:else if back === 'image'}
          <div class="hint muted">
            Printed on the back in full colour. Every double-sided card needs one
            before the project can be paid for — choose it in the pane, or drop a
            picture on the pane.
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
