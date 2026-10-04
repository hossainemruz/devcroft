# Editor setup and everyday use

## Choose an editor

Open **Settings → Editor** from Home or a project. Choose **Devcroft editor** for the built-in editor, or **Neovim** for your terminal editor and its existing configuration. Select either editor using its radio card. Neovim requires an installed `nvim` executable; its card is disabled with an error if Devcroft cannot find it. Use **Check again** after installation. Detection asks your interactive login shell whether `nvim` is available, without running Neovim; a stalled shell check times out after three seconds. The built-in editor is always available.

New installations start with the built-in editor. The choice is saved on this device. Existing installations with no saved editor choice keep Neovim. First-run detection is conservative: any existing device settings or data counts as an existing installation, including portable data restored from another machine. Changing the choice switches the current project's editor while keeping its Neovim session and built-in drafts alive; returning to either restores that instance. Other open projects keep their existing editor instances. New projects use the saved choice.

## Find and edit files

- Open the project file finder with `Cmd+J`, then `f` on macOS (`Ctrl+J`, then `f` elsewhere), or the sidebar search icon. The floating finder uses fff to rank partial paths and typos, with results on the left and a syntax-highlighted preview on the right. Select with arrow keys or `Ctrl+N`/`Ctrl+P`, press Enter to open, and Escape to dismiss.
- Use the project tree for folders and files. **… → Refresh files** picks up newly added files; **Search project** searches indexed text while respecting ignore rules.
- Tabs retain separate drafts and undo histories. The tab indicator shows unsaved changes. Save with `Cmd+S` / `Ctrl+S`; closing a dirty tab offers Save, Discard, or Cancel.
- Use **… → Go to line**, **Find in file**, and **Replace in file**. **… → Go back / Go forward** restores file/line navigation. Normal selection, clipboard, undo, and redo shortcuts work in the focused editor.
- In Review, **Open ↗** or a new-file line number opens the current checkout file. Historical/deleted diff content stays in Review.

`Cmd+P` / `Ctrl+P` opens projects, not files. See the [keyboard reference](keyboard-reference.md) for navigation and focus controls.

## Language features

Syntax highlighting covers Rust, TOML (including Cargo.lock), Python, JavaScript/JSX, TypeScript/TSX, Go, shell, Markdown, HTML, CSS, JSON, YAML, C/C++, Java, and Ruby. Other text files remain editable without highlighting.

Rust completion, hover, definitions, and diagnostics require an installed rust-analyzer and trusting the checkout: click the disabled Rust indicator in the editor status bar and confirm **Trust checkout**. Trust lasts for the editor workspace's lifetime. The [language support guide](language-support.md) describes executable discovery, prerequisites, the tested server matrix, restart, and limitations. Other languages currently have no semantic server integration.

## Open in Zed or VS Code

Use the built-in editor's **… → Open in Zed / VS Code** action to launch the checkout externally. These launchers are separate from your editor preference.

Enable **Zed** or **VS Code** with its switch in **Settings → Editor → External editors**. Disabled editors are removed from the built-in editor’s Open in menu. If an editor is not found, its switch is disabled; install the application or command-line launcher, then choose **Check again**.

Devcroft uses the same executable discovery for installation checks and launches: the desktop PATH and common binary directories, plus `/Applications` and `~/Applications` app bundles on macOS. No custom executable configuration is needed or supported. Existing custom-path settings are ignored. A launch failure leaves your editor and drafts available.

## File safety and limits

Clean buffers reload after external edits. Dirty buffers keep your draft and offer comparison, reload, or save-as when disk contents change. Saves recheck disk contents and use atomic replacement; a detected conflict or write failure retains your draft. Existing consistent LF/CRLF line endings and file permissions are preserved. Recoverable drafts are stored per checkout on this device.

Files must be regular UTF-8 text, contain no NUL bytes, and be at most 2 MiB. Mixed line endings remain editable and are not normalized automatically. Checkout-escaping paths are rejected. Deleted files are not silently recreated; review a rename or deletion before saving. Project indexing and search are bounded. For unsupported files, use an external editor.

This editor is intended for code exploration and small edits. It has no debugger, extensions, broad refactoring, or automatic language-server installation. Native UI verification for the latest editor changes and Linux/Windows runtime validation remain open; the macOS headless and live-server checks are recorded in the language support guide.
