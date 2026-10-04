# Editor setup and everyday use

## Choose an editor

Open **Settings → General → Editor** from Home or a project. Choose **Devcroft editor** for the built-in editor, or **Neovim** for your terminal editor and its existing configuration. Neovim requires `nvim` on your environment's executable path; the built-in editor does not.

New installations start with the built-in editor. The choice is saved on this device. Existing installations with no saved editor choice keep Neovim. First-run detection is conservative: any existing device settings or data counts as an existing installation, including portable data restored from another machine. Changing the choice switches the current project's editor while keeping its Neovim session and built-in drafts alive; returning to either restores that instance. Other open projects keep their existing editor instances. New projects use the saved choice.

## Find and edit files

- Open the project file finder with `Cmd+J`, then `o` on macOS (`Ctrl+J`, then `o` elsewhere), or the sidebar search icon. Type a partial path, select with arrow keys, and press Enter.
- Use the project tree for folders and files. **… → Refresh files** picks up newly added files; **Search project** searches indexed text while respecting ignore rules.
- Tabs retain separate drafts and undo histories. The tab indicator shows unsaved changes. Save with `Cmd+S` / `Ctrl+S`; closing a dirty tab offers Save, Discard, or Cancel.
- Use **… → Go to line**, **Find in file**, and **Replace in file**. **… → Go back / Go forward** restores file/line navigation. Normal selection, clipboard, undo, and redo shortcuts work in the focused editor.
- In Review, **Open ↗** or a new-file line number opens the current checkout file. Historical/deleted diff content stays in Review.

`Cmd+P` / `Ctrl+P` opens projects, not files. See the [keyboard reference](keyboard-reference.md) for navigation and focus controls.

## Language features

Syntax highlighting covers Rust, TOML (including Cargo.lock), Python, JavaScript/JSX, TypeScript/TSX, Go, shell, Markdown, HTML, CSS, JSON, YAML, C/C++, Java, and Ruby. Other text files remain editable without highlighting.

Rust completion, hover, definitions, and diagnostics require an installed rust-analyzer and the explicit **… → Trust checkout and enable Rust tooling (may run project tools)** action. Trust lasts for the editor workspace's lifetime. The [language support guide](language-support.md) describes executable discovery, prerequisites, the tested server matrix, restart, and limitations. Other languages currently have no semantic server integration.

## Open in Zed or VS Code

Use the built-in editor's **… → Open in Zed / VS Code** action to launch the checkout externally. These launchers are separate from your editor preference.

Install the editor's command-line launcher (`zed`/`zeditor` or `code`). If the desktop environment cannot find it, select the launcher in **Settings → General** and set its executable path. Enter a program path, not a shell command or arguments; paths containing spaces do not need shell quotes. A launch failure leaves your current editor and drafts available.

## File safety and limits

Clean buffers reload after external edits. Dirty buffers keep your draft and offer comparison, reload, or save-as when disk contents change. Saves recheck disk contents and use atomic replacement; a detected conflict or write failure retains your draft. Existing consistent LF/CRLF line endings and file permissions are preserved. Recoverable drafts are stored per checkout on this device.

Files must be regular UTF-8 text, contain no NUL bytes, and be at most 2 MiB. Mixed line endings remain editable and are not normalized automatically. Checkout-escaping paths are rejected. Deleted files are not silently recreated; review a rename or deletion before saving. Project indexing and search are bounded. For unsupported files, use an external editor.

This editor is intended for code exploration and small edits. It has no debugger, extensions, broad refactoring, or automatic language-server installation. Native UI verification for the latest editor changes and Linux/Windows runtime validation remain open; the macOS headless and live-server checks are recorded in the language support guide.
