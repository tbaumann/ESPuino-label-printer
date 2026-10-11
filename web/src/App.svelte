<script lang="ts">
  import { path, projectFromPath } from './lib/router'
  import Landing from './routes/Landing.svelte'
  import Project from './routes/Project.svelte'

  const uuid = $derived(projectFromPath($path))
</script>

{#if uuid}
  {#key uuid}
    <Project {uuid} />
  {/key}
{:else}
  <Landing />
{/if}
