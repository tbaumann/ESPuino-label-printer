<script lang="ts">
  import type { Card, PrintMode } from './api'
  import SingleSidedCard from './cards/SingleSidedCard.svelte'
  import ColorBackCard from './cards/ColorBackCard.svelte'
  import BlackBackCard from './cards/BlackBackCard.svelte'

  // One card row, rendered by the component of the prefix's print mode.
  //
  // The mode decides what a card *is*, so it decides what the row shows and
  // which inputs it offers: `single_sided` has no back at all, `double_sided_color`
  // takes a colour back image, `color_front_black_back` takes markdown for a
  // black-only back. Each of those is one component next door, and this one only
  // picks between them — the shared front preview, name and delete live
  // in `cards/CardFields.svelte`, which all three use.
  let {
    prefix,
    card,
    mode,
    writable,
    readonly = false,
    onDeleted,
    onCardUpdated,
  }: {
    prefix: string
    card: Card
    /** The prefix's print mode; a card cannot be rendered without it. */
    mode: PrintMode
    writable: boolean
    readonly?: boolean
    onDeleted?: (id: number) => void
    /** A card the row changed by itself (a colour back upload), so the page can
     * re-count what is still incomplete. */
    onCardUpdated?: (card: Card) => void
  } = $props()
</script>

{#if mode === 'single_sided'}
  <SingleSidedCard {prefix} {card} {writable} {readonly} {onDeleted} />
{:else if mode === 'double_sided_color'}
  <ColorBackCard {prefix} {card} {writable} {readonly} {onDeleted} onUpdated={onCardUpdated} />
{:else if mode === 'color_front_black_back'}
  <BlackBackCard {prefix} {card} {writable} {readonly} {onDeleted} />
{:else}
  <!-- A mode this build does not know would need its own component; saying so
       beats rendering one of the others and passing the wrong card off as it. -->
  <div class="empty">Unknown print mode "{mode}" — this card cannot be shown.</div>
{/if}
