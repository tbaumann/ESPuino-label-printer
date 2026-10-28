<script lang="ts">
  import { Editor } from '@tiptap/core'
  import StarterKit from '@tiptap/starter-kit'
  import Image from '@tiptap/extension-image'
  import { TableKit } from '@tiptap/extension-table'
  import { TaskItem, TaskList } from '@tiptap/extension-list'
  import { Placeholder } from '@tiptap/extension-placeholder'
  import { Markdown } from '@tiptap/markdown'

  // The back-text editor: the customer writes a song list and sees the
  // formatting it will have, while what leaves this component is the *markdown*
  // — the same text the backend is handed to render the back from.
  //
  // Nothing here lays out a card: the pane beside it is the server's render of
  // this text, and the editor's own formatting is an editing aid, not a promise
  // about the print. What the markdown may contain is therefore not narrowed
  // here either — the schema carries every construct a back can hold (lists,
  // headings, quotes, code, links, pictures, tables), so editing a card cannot
  // silently drop what the backend would have printed.
  let {
    value,
    placeholder = '',
    disabled = false,
    onchange,
    onblur,
  }: {
    /** The markdown to edit. Read once, when the editor is built. */
    value: string
    /** Shown while the box is empty and there is nothing to type over. */
    placeholder?: string
    /** Read-only while a write of this card is in flight. */
    disabled?: boolean
    /** The whole document as markdown, after every change. */
    onchange: (markdown: string) => void
    /** The editor lost focus — the moment to write what is in it. */
    onblur?: () => void
  } = $props()

  // A row is keyed by card id and outlives its cards' writes, so the editor is
  // built against the text the card had and the *parent* owns what is in it
  // from then on (`onchange`). Reading `value` inside the effect below would
  // rebuild the editor on every keystroke, taking the caret with it.
  // svelte-ignore state_referenced_locally
  const startText = value
  // svelte-ignore state_referenced_locally
  const startDisabled = disabled
  // svelte-ignore state_referenced_locally
  const startPlaceholder = placeholder

  let host = $state<HTMLDivElement | undefined>()
  let editor = $state<Editor>()
  // Which controls apply at the caret, for the toolbar's own state. Updated on
  // every transaction, which is what makes a button light up as the customer
  // moves through the text.
  let on = $state<Record<string, boolean>>({})

  const EXTENSIONS = [
    StarterKit,
    // Beyond the starter set, the constructs markdown is read for here: a
    // picture and a table are what would otherwise be dropped on the first
    // edit, and a task list its checkboxes.
    Image,
    TableKit,
    TaskList,
    TaskItem,
    Placeholder.configure({ placeholder: startPlaceholder }),
    Markdown,
  ]

  function readState(e: Editor) {
    on = {
      bold: e.isActive('bold'),
      italic: e.isActive('italic'),
      strike: e.isActive('strike'),
      bulletList: e.isActive('bulletList'),
      orderedList: e.isActive('orderedList'),
      blockquote: e.isActive('blockquote'),
      codeBlock: e.isActive('codeBlock'),
    }
  }

  $effect(() => {
    if (!host) return
    const e = new Editor({
      element: host,
      extensions: EXTENSIONS,
      content: startText,
      contentType: 'markdown',
      editable: !startDisabled,
      onUpdate: ({ editor: ed }) => onchange(ed.getMarkdown()),
      onTransaction: ({ editor: ed }) => readState(ed),
      onBlur: () => onblur?.(),
    })
    editor = e
    readState(e)
    return () => {
      editor = undefined
      e.destroy()
    }
  })

  // The card is writable or not; the editor is the same one either way.
  $effect(() => {
    editor?.setEditable(!disabled)
  })
</script>

<div class="md-editor" class:disabled>
  <!-- `mousedown` is prevented on the toolbar so a click on a button takes
       nothing away from the selection it is about to change — without it the
       editor would blur (and the row would write) before the command runs. -->
  <div
    class="md-toolbar"
    role="toolbar"
    aria-label="Formatting"
    tabindex="-1"
    onmousedown={(e) => e.preventDefault()}
  >
    <button
      type="button"
      title="Bold"
      aria-pressed={on.bold}
      class:on={on.bold}
      {disabled}
      onclick={() => editor?.chain().focus().toggleBold().run()}
    >
      <strong>B</strong>
    </button>
    <button
      type="button"
      title="Italic"
      aria-pressed={on.italic}
      class:on={on.italic}
      {disabled}
      onclick={() => editor?.chain().focus().toggleItalic().run()}
    >
      <em>I</em>
    </button>
    <button
      type="button"
      title="Strikethrough"
      aria-pressed={on.strike}
      class:on={on.strike}
      {disabled}
      onclick={() => editor?.chain().focus().toggleStrike().run()}
    >
      <s>S</s>
    </button>
    <button
      type="button"
      title="Heading"
      aria-pressed={false}
      {disabled}
      onclick={() => editor?.chain().focus().toggleHeading({ level: 2 }).run()}
    >
      H2
    </button>
    <button
      type="button"
      title="Bullet list"
      aria-pressed={on.bulletList}
      class:on={on.bulletList}
      {disabled}
      onclick={() => editor?.chain().focus().toggleBulletList().run()}
    >
      •
    </button>
    <button
      type="button"
      title="Numbered list"
      aria-pressed={on.orderedList}
      class:on={on.orderedList}
      {disabled}
      onclick={() => editor?.chain().focus().toggleOrderedList().run()}
    >
      1.
    </button>
    <button
      type="button"
      title="Quote"
      aria-pressed={on.blockquote}
      class:on={on.blockquote}
      {disabled}
      onclick={() => editor?.chain().focus().toggleBlockquote().run()}
    >
      ❝
    </button>
    <button
      type="button"
      title="Code"
      aria-pressed={on.codeBlock}
      class:on={on.codeBlock}
      {disabled}
      onclick={() => editor?.chain().focus().toggleCodeBlock().run()}
    >
      {'{ }'}
    </button>
  </div>

  <div class="md-surface" bind:this={host}></div>
</div>
