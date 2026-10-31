<script lang="ts">
  import { onMount } from 'svelte'
  import { isUuid, newUuid, recentProjects, rememberProject } from '../lib/api'
  import { navigate } from '../lib/router'

  let error = $state<string | null>(null)
  let code = $state('')
  let recent = $state(recentProjects())

  /**
   * Open a project nobody has written to yet.
   *
   * The uuid is picked here, in the browser, and going to it stores nothing: the
   * project is created by its first write (the first card uploaded). So a page
   * load — or a hundred of them — leaves no empty project behind, and a page
   * that is only looked at is not an order.
   */
  function createNew() {
    navigate(`/p/${newUuid()}`)
  }

  function openExisting() {
    const c = code.trim()
    if (isUuid(c)) {
      rememberProject(c)
      navigate(`/p/${c}`)
    } else {
      error = 'That is not a project code (expected a uuid).'
    }
  }

  // Opening the main page drops you into a fresh project.
  onMount(createNew)
</script>

<div class="app" style="max-width:640px">
  <h1>Kartenschmiede</h1>
  <p class="muted" style="margin:8px 0 24px">
    Print-ready RFID card labels for ESPuino, phoniebox, TONUino, SonUINO and friends.
  </p>

  {#if error}
    <div class="error">{error}</div>
  {/if}

  <section class="panel">
    <div class="row">
      <button class="primary" onclick={createNew}>New project</button>
      <div class="row" style="gap:8px;flex:1">
        <input
          type="text"
          placeholder="…or open an existing project code"
          bind:value={code}
          style="min-width:260px"
          onkeydown={(e) => e.key === 'Enter' && openExisting()}
        />
        <button onclick={openExisting}>Open</button>
      </div>
    </div>

    {#if recent.length}
      <div style="margin-top:18px">
        <div class="field-label">Recent projects</div>
        {#each recent as r (r.uuid)}
          <a
            class="mono"
            href={`/p/${r.uuid}`}
            onclick={(e) => {
              e.preventDefault()
              navigate(`/p/${r.uuid}`)
            }}
            style="display:block;color:var(--accent);text-decoration:none;padding:3px 0">{r.uuid}</a>
        {/each}
      </div>
    {/if}
  </section>
</div>
