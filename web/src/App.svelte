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

<!-- The Impressum belongs to every page, so it lives in the shell rather than in
     each route: no route can be served without it. -->
<footer class="site-footer">
  <div class="site-footer-inner">
    <div class="field-label">Impressum</div>
    <p>Tilman Johannes Baumann</p>
    <p>Grindstraße 7, 35066 Frankenberg (Eder), DE</p>
    <p>Telefon: +49 151 55837069</p>
    <p>
      E-Mail: <a href="mailto:info@kartenschmiede.tilman.baumann.name"
        >info@kartenschmiede.tilman.baumann.name</a
      >
    </p>
  </div>
</footer>
