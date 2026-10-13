<script lang="ts">
  import { api, type Card } from '../api'
  import CardFields from './CardFields.svelte'

  // `single_sided` — a full-colour front on a blank card.
  //
  // There is no back side at all: no back field, no back pane, and nothing to
  // render. Showing an empty back panel here would suggest a side that does not
  // exist (and that the customer is not paying for), so the row says outright
  // that the option has none.
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
</script>

<article class="card" class:preview-only={readonly}>
  <div>
    <div class="preview">
      <img src={api.cardImageUrl(prefix, card.id)} alt={`front of ${card.name}`} />
    </div>
    <div class="caption muted">front · full colour</div>

    <div class="no-back-side muted" style="margin-top:10px">
      Front only — this option has no back side.
    </div>
  </div>

  <CardFields {prefix} {card} {writable} {readonly} {onDeleted} />
</article>
