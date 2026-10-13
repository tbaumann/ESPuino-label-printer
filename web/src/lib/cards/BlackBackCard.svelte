<script lang="ts">
  import { api, type Card } from '../api'
  import { hasImageSyntax, renderBackText } from '../backText'
  import CardFields from './CardFields.svelte'

  // `color_front_black_back` — a full-colour front, and a back printed by the
  // black panel alone.
  //
  // The back side is `back_text`, markdown, and the preview has to be honest
  // about the one thing this mode is: 1-bit black. So the pane renders on paper
  // white with pure black ink (never the theme's colours, never a grey the
  // printer cannot make), and says outright what the medium refuses — an image
  // is dropped by the renderer, and a customer who typed one should be told
  // rather than shown a blank where they expected artwork. That difference from
  // the colour mode is the whole reason the two have separate components.
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
    readonly?: boolean
    onDeleted?: (id: number) => void
  } = $props()

  // This row owns an editable copy of the back text; it resyncs from the PATCH
  // response, and the parent never re-fetches after an edit (rows are keyed by
  // card id).
  // svelte-ignore state_referenced_locally
  let back = $state(card.back_text ?? '')

  // The preview renders `back` as markdown. The output has been through the
  // sanitiser (renderBackText), so `{@html}` on it is safe.
  const backHtml = $derived(back ? renderBackText(back) : '')
  // Not a rendering rule, a courtesy: the renderer drops an image whole, so the
  // author is told instead of left wondering where their artwork went.
  const imageRefused = $derived(hasImageSyntax(back))

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
      const updated = await api.patchCard(prefix, card.id, { back_text: back })
      back = updated.back_text ?? ''
      saveState = 'saved'
    } catch (e) {
      errMsg = (e as Error).message
      saveState = 'error'
    }
  }
</script>

<article class="card" class:preview-only={readonly}>
  <CardFields {prefix} {card} {writable} {readonly} {onDeleted} />

  <div class="image-row">
    <div class="side">
      <div class="preview">
        <img src={api.cardImageUrl(prefix, card.id)} alt={`front of ${card.name}`} />
      </div>
      <div class="caption muted">front · full colour</div>
    </div>

    <div class="side">
      <div class="preview back black-only">
        <div class="panel">{@html backHtml || '—'}</div>
      </div>
      <div class="caption muted">back · black only (1 bit)</div>
    </div>

    {#if !readonly}
      <div class="side editor">
        <label for={`back-${card.id}`}>Back text (markdown)</label>
        <textarea
          id={`back-${card.id}`}
          rows="5"
          bind:value={back}
          oninput={schedule}
          disabled={!writable}
          placeholder="song list"
        ></textarea>
        <div class="hint muted">
          Printed on the back in black only: headings, bold, italic and lists.
          No colour, no images, no greys.
        </div>
        {#if imageRefused}
          <div class="hint warn">
            This text contains image markup, which the black-only back cannot
            print — the whole <code>![…]</code> construct is dropped, alt text
            included. Use the double-sided colour option for a picture.
          </div>
        {/if}
        {#if saveState === 'saving'}
          <span class="save-state"><span class="spinner"></span> saving…</span>
        {:else if saveState === 'error'}
          <span class="save-state" style="color:var(--danger)">{errMsg}</span>
        {/if}
      </div>
    {/if}
  </div>
</article>
