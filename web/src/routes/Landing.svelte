<script lang="ts">
  import { onMount } from 'svelte'
  import { api, isUuid, recentProjects, rememberProject } from '../lib/api'
  import { navigate } from '../lib/router'

  let error = $state<string | null>(null)
  let creating = $state(true)
  let code = $state('')
  let recent = $state(recentProjects())

  async function createNew() {
    creating = true
    error = null
    try {
      const p = await api.createPrefix()
      rememberProject(p.uuid)
      navigate(`/p/${p.uuid}`)
    } catch (e) {
      error = (e as Error).message
    } finally {
      creating = false
    }
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
  <h1>ESPuino Label Printer</h1>
  <p class="muted" style="margin:8px 0 24px">
    Print-ready RFID card labels for ESPuino, phoniebox, TONUino, SonUINO and friends.
  </p>

  {#if error}
    <div class="error">{error}</div>
  {/if}

  <section class="panel">
    <div class="row">
      <button class="primary" onclick={createNew} disabled={creating}>
        {#if creating}<span class="spinner"></span> Creating project…{:else}New project{/if}
      </button>
      <div class="row" style="gap:8px;flex:1">
        <input
          type="text"
          placeholder="…or open an existing project code"
          bind:value={code}
          style="min-width:260px"
          onkeydown={(e) => e.key === 'Enter' && openExisting()}
        />
        <button onclick={openExisting} disabled={creating}>Open</button>
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
