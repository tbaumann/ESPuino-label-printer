<script lang="ts">
  import { path, projectFromPath, adminProjectFromPath, isAdminPath } from './lib/router'
  import Landing from './routes/Landing.svelte'
  import Project from './routes/Project.svelte'
  import Admin from './routes/Admin.svelte'
  import AdminProject from './routes/AdminProject.svelte'

  const uuid = $derived(projectFromPath($path))
  const adminUuid = $derived(adminProjectFromPath($path))
  const admin = $derived(isAdminPath($path))
</script>

{#if adminUuid}
  {#key adminUuid}
    <AdminProject uuid={adminUuid} />
  {/key}
{:else if admin}
  <Admin />
{:else if uuid}
  {#key uuid}
    <Project {uuid} />
  {/key}
{:else}
  <Landing />
{/if}
