<script lang="ts">
  import { api, type Ribbon } from './api'

  let {
    prefix,
    ribbon,
    onUploaded,
    disabled = false,
  }: {
    prefix: string
    /** The prefix's ribbon, which decides how a card's back is printed. */
    ribbon?: Ribbon
    onUploaded: () => void | Promise<void>
    disabled?: boolean
  } = $props()

  let over = $state(false)
  let busy = $state(false)
  let done = $state(0)
  let total = $state(0)
  let errors = $state<string[]>([])

  let fileInput: HTMLInputElement
  let dirInput: HTMLInputElement

  // Directory selection is fragile: a `display:none` input can be ignored for
  // it, and some engines only honour the IDL property, not the attribute.
  // Set the property directly as well, and keep the input rendered (see the
  // `visually-hidden` class below).
  $effect(() => {
    if (dirInput) {
      ;(dirInput as HTMLInputElement & { webkitdirectory: boolean }).webkitdirectory = true
    }
  })

  const IMAGE_RE = /\.(png|jpe?g|webp|gif)$/i

  function isImage(f: File): boolean {
    return f.type.startsWith('image/') || IMAGE_RE.test(f.name)
  }

  function walkEntry(entry: FileSystemEntry, out: File[]): Promise<void> {
    return new Promise((resolve) => {
      if (entry.isFile) {
        ;(entry as FileSystemFileEntry).file(
          (f) => {
            out.push(f)
            resolve()
          },
          () => resolve(),
        )
      } else if (entry.isDirectory) {
        const reader = (entry as FileSystemDirectoryEntry).createReader()
        const all: FileSystemEntry[] = []
        const readBatch = () =>
          reader.readEntries(
            async (batch) => {
              if (!batch.length) {
                await Promise.all(all.map((e) => walkEntry(e, out)))
                resolve()
              } else {
                all.push(...batch)
                readBatch()
              }
            },
            () => resolve(),
          )
        readBatch()
      } else {
        resolve()
      }
    })
  }

  /** Collect dropped files, recursing into dropped directories. */
  async function filesFromDrop(dt: DataTransfer): Promise<File[]> {
    const entries = Array.from(dt.items ?? [])
      .map((i) => (typeof i.webkitGetAsEntry === 'function' ? i.webkitGetAsEntry() : null))
      .filter((e): e is FileSystemEntry => e !== null)
    if (!entries.length) return Array.from(dt.files)
    const out: File[] = []
    await Promise.all(entries.map((e) => walkEntry(e, out)))
    return out
  }

  async function upload(files: File[]) {
    const images = files.filter(isImage)
    errors = []
    if (!images.length) {
      errors = ['No image files found (png, jpg, webp, gif).']
      return
    }
    busy = true
    done = 0
    total = images.length
    for (const f of images) {
      try {
        await api.uploadCard(prefix, f)
      } catch (e) {
        errors = [...errors, `${f.name}: ${(e as Error).message}`]
      }
      done += 1
    }
    busy = false
    await onUploaded()
  }

  async function onDrop(e: DragEvent) {
    e.preventDefault()
    over = false
    if (disabled || busy || !e.dataTransfer) return
    await upload(await filesFromDrop(e.dataTransfer))
  }

  async function onPick(e: Event) {
    const input = e.currentTarget as HTMLInputElement
    const files = Array.from(input.files ?? [])
    input.value = ''
    if (files.length) await upload(files)
  }
</script>

<div
  class="dropzone"
  class:over
  role="button"
  tabindex="0"
  aria-label="Upload card images"
  onclick={() => !disabled && !busy && fileInput.click()}
  onkeydown={(e) => {
    if ((e.key === 'Enter' || e.key === ' ') && !disabled && !busy) {
      e.preventDefault()
      fileInput.click()
    }
  }}
  ondragover={(e) => {
    e.preventDefault()
    if (!disabled) over = true
  }}
  ondragleave={() => (over = false)}
  ondrop={onDrop}
>
  <div style="font-weight:600">Drag images or a folder here</div>
  <div class="row" style="justify-content:center;margin-top:12px">
    <button
      onclick={(e) => {
        e.stopPropagation()
        fileInput.click()
      }}
      disabled={disabled || busy}>Choose files…</button
    >
    <button
      onclick={(e) => {
        e.stopPropagation()
        dirInput.click()
      }}
      disabled={disabled || busy}>Choose folder…</button
    >
  </div>
  {#if busy}
    <div class="progress">
      <span style="width:{(done / Math.max(total, 1)) * 100}%"></span>
    </div>
    <div class="hint">uploading {done}/{total}…</div>
  {/if}
</div>

<!-- Deliberately OUTSIDE the dropzone: a programmatic .click() on these
     bubbles to the dropzone's own click handler, which would open the file
     picker instead of the folder picker. -->
<input
  bind:this={fileInput}
  class="visually-hidden"
  type="file"
  accept="image/*"
  multiple
  onchange={onPick}
/>
<input
  bind:this={dirInput}
  class="visually-hidden"
  type="file"
  multiple
  webkitdirectory
  onchange={onPick}
/>

{#each errors as msg}
  <div class="error" style="margin-top:10px">{msg}</div>
{/each}
