# Icon assets — attribution

## Agent harness logos

The top-level SVGs `claude.svg`, `codex.svg`, `opencode.svg`, and `omp.svg`
are the respective agent CLIs' brand marks, used as full-color logos in the
Agent surfaces (new-session picker, sessions sidebar,
Home cards — see `src/agent_icons.rs`). The first three belong to their
respective owners (Anthropic, OpenAI, SST) and are redistributed here for
product identification only; `omp.svg` is oh-my-pi's pi-and-connector mark.
Sessions from unknown providers fall back to the neutral `Bot` glyph. To add
a harness logo, drop `<key>.svg` next to these and extend `icon_svg`.

## Editor brand icons

The top-level PNGs `neovim.png`, `zed.png`, and `vscode.png` are the
respective editors' brand marks, used as full-color logos in the Settings
Editor section (see `src/editor_icons.rs`). They belong to their respective
owners (the Neovim project, Zed Industries, Microsoft) and are redistributed
here for product identification only. To add an editor logo, drop `<key>.png`
next to these and extend `EditorIcon`.

## File icons

The SVGs in `files/` are a vendored subset of the
[Material Icon Theme](https://github.com/material-extensions/vscode-material-icon-theme)
(`icons/` directory), used as file-type glyphs in the Review sidebar.

- Source: https://github.com/material-extensions/vscode-material-icon-theme
- License: MIT License, Copyright (c) 2025 Material Extensions
- The full license text is reproduced below, as required by the MIT license
  for redistributed copies.

Only the icons mapped in `src/review/icons.rs` are vendored here
(30 of ~900 upstream). To add a file type: copy its `<name>.svg` from the
upstream `icons/` directory into `files/` and extend the resolver tables —
no other onboarding needed. Prefer the dark (non-`_light`) variants: the
app is dark-only, and GPUI renders SVGs as monochrome silhouettes tinted at
draw time, so light-variant alternates have no effect.

---

The MIT License (MIT)
Copyright (c) 2025 Material Extensions

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
THE SOFTWARE.
