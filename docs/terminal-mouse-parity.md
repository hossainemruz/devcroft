# Terminal mouse parity

The terminal previously forwarded only wheel events. Left clicks always began local selection, releases copied selections, and no link-opening path existed. Agent controls therefore never received clicks.

Implementation: forward requested mouse press/release/motion through the existing Ghostty encoder, using pane-relative coordinates and terminal protocol modes. Keep the gesture routed to the application through release, even when Shift changes during a drag. Shift starts local selection instead. Ctrl-click (Cmd-click on macOS) opens HTTP(S) OSC 8 destinations or visible single-row URLs. Existing selection and auto-copy remain available outside application mouse tracking.

Validation: compile and lint; regression test SGR press/release coordinates, suppression of hover in button-motion mode, tracking disable, and OSC 8 destination lookup. Desktop acceptance requires expanding agent thinking, interacting with links, dragging inside an application, Shift-selecting, and releasing a drag outside the pane.

Remaining differences: image protocols, wrapped plain-text URL detection, link hover decoration, and configurable copy-on-select are not implemented. This change does not claim full Ghostty frontend parity.

## Follow-up implementation plan

1. Detect complete HTTP(S) URLs across actual soft wraps, retain grid ranges for hover underlining, and show the destination while hovering. OSC 8 destinations take precedence. Never join hard line breaks.
2. Add a device-local General setting for copy-on-select, applied live, plus explicit Ctrl+Shift+C / Cmd+C copying without consuming Ctrl+C.
3. Enable Ghostty's Kitty graphics support with bounded storage and PNG decoding. Render cached images with source cropping, viewport clipping, placement ordering, scrolling, and screen switching; exercise transmit/delete and geometry in tests. Ghostty's graphics protocol is Kitty, so unrelated protocols such as Sixel are outside this parity work.
4. Extend protocol/interaction regression coverage, run checks and build, and perform a native desktop smoke test where the environment supports it. Record any remaining limits precisely.
