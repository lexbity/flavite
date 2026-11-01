## Workspace reading and structure tools

The development workspace supports Edit and rendered Read modes on the same
note tab. Use **Ctrl+E** (**Cmd+E** on macOS) to switch and **Ctrl/Cmd+F** to find
text in the current buffer, including unsaved changes. A mode change keeps the
note, shared Undo history, and content anchor; it does not save the note.

Open **Outline**, **Properties**, or **Preview** from the command palette
(Ctrl/Cmd+Shift+P). Each tool has an explicit source binding and can move to any
tab group. Preview also offers a source-text mode. Tasks and supported scalar
frontmatter values use revision-guarded edits and standalone Undo steps.
Complex or ambiguous YAML offers **Edit source**.

Read mode renders CommonMark and the existing Flavite extensions. HTML remains
inert source; remote images are not fetched. Local PNG/JPEG/WebP images and note
embeds have size, recursion, and shared-memory limits, with explicit Open
actions when content cannot be embedded.

Run `just workspace-gate W09` for the additive behavior checks and
`just workspace-visual read-structure` for the Linux window workflow. The latter
requires the tools installed by `just ui-visual-setup`, including Tesseract's
English language data. It uses only a disposable fixture vault; `app-demo`
creates example content and is not a production startup path.

The [W09 verification record](devdocs/verification/workspace-w09.md) distinguishes
implemented behavior from outstanding native accessibility/IME and release
performance evidence.

## On-disk cache versions

The SQLite index cache uses schema major **3**. A cache with a different schema
major is disposable and is rebuilt from vault files. Rename journal manifests
use version **2**; an older or corrupt manifest is quarantined during open and
reported through an action-required notice. These version changes do not alter
the note files that are the vault's source of truth.
