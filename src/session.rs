//! PTY management and the Ghostty terminal state machine.
//!
//! A [`TerminalSession`] owns one pseudo-terminal child process plus the
//! `libghostty-vt` terminal that parses its output. It has no GPUI
//! dependencies beyond key-event types, so the engine stays separable from
//! the view layer in [`crate::pane`].

use std::{
    env,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    thread,
};

use anyhow::{Context as _, Result};
use gpui_kit::{KeyDownEvent, Modifiers};
use libghostty_vt::{
    key::{self, Action, Encoder as KeyEncoder, Event as KeyEvent, Mods},
    mouse::{
        self, Action as MouseAction, Button as MouseButton, Encoder as MouseEncoder,
        Event as MouseEvent,
    },
    render::{CellIterator, CursorVisualStyle, Dirty, RenderState, RowIteration, RowIterator},
    screen::CellWide,
    style::RgbColor,
    terminal::{
        ConformanceLevel, DeviceAttributeFeature, DeviceAttributes, DeviceType, Mode, Point,
        PointCoordinate, PrimaryDeviceAttributes, ScrollViewport, SecondaryDeviceAttributes,
        SizeReportSize, Terminal,
    },
};
use parking_lot::Mutex;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::{
    agent::AgentKind,
    keys::map_key,
    metrics::{INITIAL_COLS, INITIAL_ROWS, TERMINAL_PADDING, cell_height, cell_width},
    workspace::WorkspaceTab,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct CellStyle {
    pub(crate) foreground: u32,
    pub(crate) background: u32,
    pub(crate) bold: bool,
    pub(crate) italic: bool,
    pub(crate) underline: bool,
    /// Marks the terminal's cursor cell. The view decides how to paint it
    /// (see [`CursorShape`] and the pane's cursor rendering): a block
    /// cursor swaps the run's colors, the other shapes overlay a rect.
    pub(crate) cursor: bool,
}

/// The cursor shapes applications request through DECSCUSR (`CSI Ps SP q`).
///
/// nvim switches between these as the editor mode changes (steady block in
/// normal mode, steady bar in insert), so the view needs the shape alongside
/// the presented rows instead of reading only the terminal's cells.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CursorShape {
    Block,
    Bar,
    Underline,
    /// An outlined block, typically shown by unfocused windows.
    Hollow,
}

/// The cursor state of a presented frame.
///
/// `cell` is `None` whenever the cursor is not paintable: DEC mode 25 hid
/// it (nvim hides the cursor around redraws), or the viewport scrolled it
/// off-screen. It pairs with `shape` and `blinking` so the view can render
/// the cursor exactly as the application asked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct CursorPresentation {
    pub(crate) cell: Option<(u16, u16)>,
    pub(crate) shape: CursorShape,
    /// Whether the cursor should blink. DECSCUSR carries no rate, so the
    /// view picks the interval like every other terminal.
    pub(crate) blinking: bool,
}

impl CursorPresentation {
    /// The state before the first frame presents: no cursor to paint yet.
    pub(crate) const HIDDEN: Self = Self {
        cell: None,
        shape: CursorShape::Block,
        blinking: false,
    };
}

#[derive(Clone)]
pub(crate) struct RenderRun {
    pub(crate) text: gpui_kit::SharedString,
    pub(crate) columns: u16,
    pub(crate) style: CellStyle,
    /// Block fills (`▀▄█`) are painted as solid rects instead of shaped text.
    ///
    /// Adjacent block glyphs carry continuous ink across cell boundaries, but
    /// GPU text renders each glyph as an independently antialiased sprite, so
    /// every shared boundary darkens slightly and solid fills (the opencode
    /// logo, its input-box border) render beaded instead of seamless like in
    /// Ghostty. Marking the run lets the view substitute a space (which keeps
    /// the cell advance but contributes no ink) and paint an exact,
    /// grid-aligned rect in the run's foreground color instead.
    pub(crate) block: Option<BlockKind>,
}

/// Half/full-cell block fills that must tile seamlessly across columns.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum BlockKind {
    /// `▀`: foreground fills the upper half of the cells.
    Upper,
    /// `▄`: foreground fills the lower half of the cells.
    Lower,
    /// `█`: foreground fills the whole cells.
    Full,
}

/// Classifies a cell's emitted text as a paintable block fill.
///
/// Only exact single-character cells qualify; anything else (including
/// grapheme clusters that merely contain these codepoints) falls back to
/// ordinary text shaping.
fn block_kind(text: &str) -> Option<BlockKind> {
    match text {
        "▀" => Some(BlockKind::Upper),
        "▄" => Some(BlockKind::Lower),
        "█" => Some(BlockKind::Full),
        _ => None,
    }
}

pub(crate) struct TerminalSession {
    terminal: Terminal<'static, 'static>,
    pub(crate) graphics: crate::terminal_graphics::Graphics,
    render_state: RenderState<'static>,
    row_iterator: RowIterator<'static>,
    cell_iterator: CellIterator<'static>,
    key_encoder: KeyEncoder<'static>,
    key_event: KeyEvent<'static>,
    mouse_encoder: MouseEncoder<'static>,
    mouse_event: MouseEvent<'static>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    grid_size: Arc<Mutex<(u16, u16)>>,
    master: Box<dyn MasterPty + Send>,
    _child: Box<dyn Child + Send + Sync>,
    /// Last presented grid. Snapshots rebuild only dirty rows (plus rows the
    /// cursor entered or left, whose highlight state row dirtiness does not
    /// cover) and clone the rest from here, so a picker step that rewrites a
    /// handful of rows no longer pays a full-grid rebuild.
    cached_rows: Vec<Vec<RenderRun>>,
    /// Terminal defaults the unstyled cells of `cached_rows` were built with.
    /// A palette change without row dirtiness still invalidates the cache.
    cached_defaults: Option<(u32, u32)>,
    /// Cursor state of the last presented frame. The cell forces row
    /// rebuilds when the cursor entered or left a row (cursor motion
    /// alone dirties nothing), and the whole presentation feeds the
    /// no-change early return: applications restyle the cursor with
    /// DECSCUSR sequences that dirty no cells (nvim sends a steady bar on
    /// every insert-mode entry), so a shape or blink change must present
    /// a frame too.
    presented_cursor: CursorPresentation,
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        // Closing the terminal also stops its login shell; the PTY hangup
        // propagates to the foreground agent. Reap away from the UI thread.
        let _ = self._child.kill();
    }
}

impl TerminalSession {
    /// Spawn the login shell plus the tab's startup command. The Agent tab
    /// launches `agent` ([`AgentKind::command`]); every other tab uses its
    /// fixed [`WorkspaceTab::command`].
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn spawn(
        tab: WorkspaceTab,
        cwd: &Path,
        agent: AgentKind,
    ) -> Result<(Self, async_channel::Receiver<Vec<u8>>)> {
        Self::spawn_with_startup(tab, cwd, agent, None, &[])
    }

    /// Spawn with a prepared Agent command. Other tabs ignore the override.
    /// Keeping the original [`Self::spawn`] entry point makes the terminal
    /// engine independently testable with the ordinary harness command.
    pub(crate) fn spawn_with_startup(
        tab: WorkspaceTab,
        cwd: &Path,
        agent: AgentKind,
        startup_command: Option<&str>,
        startup_environment: &[(String, String)],
    ) -> Result<(Self, async_channel::Receiver<Vec<u8>>)> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: INITIAL_ROWS,
                cols: INITIAL_COLS,
                pixel_width: (INITIAL_COLS as f32 * cell_width()) as u16,
                pixel_height: (INITIAL_ROWS as f32 * cell_height()) as u16,
            })
            .context("opening pseudo-terminal")?;

        let shell = default_shell();
        let mut command = CommandBuilder::new(&shell);
        command.cwd(cwd);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env("TERM_PROGRAM", "devcroft");
        command.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
        if tab == WorkspaceTab::Agent {
            for (name, value) in startup_environment {
                command.env(name, value);
            }
        }
        command.arg("-l");

        let child = pair
            .slave
            .spawn_command(command)
            .with_context(|| format!("launching {} with {}", tab.label(), shell.display()))?;
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .context("cloning PTY reader")?;
        let writer = Arc::new(Mutex::new(
            pair.master.take_writer().context("opening PTY writer")?,
        ));
        let (sender, output) = async_channel::unbounded();
        thread::Builder::new()
            .name(format!("{}-pty-reader", tab.label().to_lowercase()))
            .spawn(move || {
                let mut buffer = [0_u8; 16 * 1024];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(length) => {
                            if sender.send_blocking(buffer[..length].to_vec()).is_err() {
                                break;
                            }
                        }
                    }
                }
            })
            .context("spawning PTY reader")?;

        let grid_size = Arc::new(Mutex::new((INITIAL_COLS, INITIAL_ROWS)));
        let mut terminal = Terminal::new(INITIAL_COLS, INITIAL_ROWS)?;
        crate::terminal_graphics::configure(&mut terminal)?;
        terminal.set_scrollback_max_lines(Some(10_000))?;
        // Ghostty's default cursor blinks (ghostty(5), `cursor-style-blink`:
        // "If this is not set, the cursor blinks by default"), so a fresh
        // shell prompt blinks here too. Applications still control both the
        // shape and the blink through DECSCUSR (nvim requests steady
        // shapes) and DEC mode 12, exactly like in Ghostty.
        terminal.set_default_cursor_blink(Some(true))?;
        terminal.resize(
            INITIAL_COLS,
            INITIAL_ROWS,
            cell_width().round() as u32,
            cell_height().round() as u32,
        )?;
        terminal
            .on_pty_write({
                let writer = Arc::clone(&writer);
                move |_, data| {
                    let _ = writer.lock().write_all(data);
                    let _ = writer.lock().flush();
                }
            })?
            .on_size({
                let grid_size = Arc::clone(&grid_size);
                move |_| {
                    let (columns, rows) = *grid_size.lock();
                    Some(SizeReportSize {
                        rows,
                        columns,
                        cell_width: cell_width().round() as u32,
                        cell_height: cell_height().round() as u32,
                    })
                }
            })?
            .on_device_attributes(|_| {
                Some(DeviceAttributes {
                    primary: PrimaryDeviceAttributes::new(
                        ConformanceLevel::VT220,
                        &[
                            DeviceAttributeFeature::COLUMNS_132,
                            DeviceAttributeFeature::SELECTIVE_ERASE,
                            DeviceAttributeFeature::ANSI_COLOR,
                        ],
                    ),
                    secondary: SecondaryDeviceAttributes {
                        device_type: DeviceType::VT220,
                        firmware_version: 1,
                        rom_cartridge: 0,
                    },
                    tertiary: Default::default(),
                })
            })?
            .on_xtversion(|_| Some("devcroft"))?;

        if let Some(program) = match tab {
            WorkspaceTab::Agent => startup_command.or_else(|| Some(agent.command())),
            _ => tab.command(),
        } {
            let mut pty = writer.lock();
            pty.write_all(format!("{program}\r").as_bytes())
                .with_context(|| format!("starting {} in the login shell", tab.label()))?;
            pty.flush()?;
        }

        Ok((
            Self {
                terminal,
                render_state: RenderState::new()?,
                row_iterator: RowIterator::new()?,
                cell_iterator: CellIterator::new()?,
                key_encoder: KeyEncoder::new()?,
                key_event: KeyEvent::new()?,
                graphics: crate::terminal_graphics::Graphics::default(),
                mouse_encoder: MouseEncoder::new()?,
                mouse_event: MouseEvent::new()?,
                writer,
                grid_size,
                master: pair.master,
                _child: child,
                cached_rows: Vec::new(),
                cached_defaults: None,
                presented_cursor: CursorPresentation::HIDDEN,
            },
            output,
        ))
    }

    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        self.terminal.vt_write(bytes);
    }

    pub(crate) fn title(&self) -> Result<String> {
        Ok(self.terminal.title()?.to_owned())
    }

    /// Read the terminal's live active area independently of the user's
    /// scrollback viewport. Activity detection needs the current dialog and
    /// prompt; historical approval text must never become live evidence.
    pub(crate) fn activity_screen(&self) -> Result<String> {
        const REGION_ROWS: u16 = 20;

        let cols = self.terminal.cols()?;
        let rows = self.terminal.rows()?;
        let mut selected = Vec::with_capacity((REGION_ROWS * 2).min(rows) as usize);
        if rows <= REGION_ROWS * 2 {
            selected.extend(0..rows);
        } else {
            selected.extend(0..REGION_ROWS);
            selected.extend(rows - REGION_ROWS..rows);
        }

        let mut screen = String::new();
        for (index, row) in selected.into_iter().enumerate() {
            if index > 0 {
                screen.push('\n');
            }
            if rows > REGION_ROWS * 2 && index == REGION_ROWS as usize {
                // Preserve the region boundary as a line without exposing
                // omitted scrollback-like content to the classifier.
                screen.push_str("…\n");
            }
            let row_start = screen.len();
            for column in 0..cols {
                let cell = self
                    .terminal
                    .grid_ref(Point::Active(PointCoordinate {
                        x: column,
                        y: u32::from(row),
                    }))?
                    .cell()?;
                let codepoint = cell.codepoint()?;
                screen.push(
                    char::from_u32(codepoint)
                        .filter(|character| *character != '\0')
                        .unwrap_or(' '),
                );
            }
            screen.truncate(screen.trim_end().len().max(row_start));
        }
        Ok(screen)
    }

    pub(crate) fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        *self.grid_size.lock() = (cols, rows);
        self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: (cols as f32 * cell_width()) as u16,
            pixel_height: (rows as f32 * cell_height()) as u16,
        })?;
        self.terminal.resize(
            cols,
            rows,
            cell_width().round() as u32,
            cell_height().round() as u32,
        )?;
        Ok(())
    }

    pub(crate) fn paste(&mut self, text: &str) -> Result<()> {
        let bracketed = self
            .terminal
            .mode(libghostty_vt::terminal::Mode::BRACKETED_PASTE)?;
        let mut writer = self.writer.lock();
        if bracketed {
            writer.write_all(b"\x1b[200~")?;
        }
        writer.write_all(text.as_bytes())?;
        if bracketed {
            writer.write_all(b"\x1b[201~")?;
        }
        writer.flush()?;
        Ok(())
    }

    pub(crate) fn scroll_to_bottom(&mut self) {
        self.terminal.scroll_viewport(ScrollViewport::Bottom);
    }

    pub(crate) fn viewport_active(&self) -> Result<bool> {
        Ok(self.terminal.viewport_active()?)
    }

    /// Whether wheel input is forwarded to the application instead of moving
    /// the local viewport. Mirrors the branching in [`Self::scroll`] so
    /// callers can pre-scale deltas for the application path.
    pub(crate) fn app_handles_scroll(&self) -> Result<bool> {
        if self.terminal.is_mouse_tracking()? {
            return Ok(true);
        }
        Ok(self.terminal.mode(Mode::ALT_SCREEN_SAVE)?
            || self.terminal.mode(Mode::ALT_SCREEN)?
            || self.terminal.mode(Mode::ALT_SCREEN_LEGACY)?)
    }

    pub(crate) fn mouse_tracking(&self) -> bool {
        self.terminal.is_mouse_tracking().unwrap_or(false)
    }

    pub(crate) fn row_wraps(&self, row: usize) -> bool {
        self.terminal
            .grid_ref(Point::Viewport(PointCoordinate {
                x: 0,
                y: row as u32,
            }))
            .and_then(|cell| cell.row())
            .and_then(|row| row.is_wrapped())
            .unwrap_or(false)
    }

    pub(crate) fn hyperlink_at(&self, column: usize, row: usize) -> Option<String> {
        let cell = self
            .terminal
            .grid_ref(Point::Viewport(PointCoordinate {
                x: column as u16,
                y: row as u32,
            }))
            .ok()?;
        let mut bytes = vec![0; 8192];
        let len = cell.hyperlink_uri(&mut bytes).ok()?;
        (len > 0).then(|| String::from_utf8_lossy(&bytes[..len]).into_owned())
    }

    pub(crate) fn send_mouse(
        &mut self,
        action: MouseAction,
        button: Option<MouseButton>,
        position: (f32, f32),
        modifiers: Modifiers,
        size: (f32, f32),
    ) -> Result<()> {
        self.configure_mouse(position, modifiers, size);
        self.mouse_event.set_button(button).set_action(action);
        self.mouse_encoder
            .set_any_button_pressed(button.is_some() && action != MouseAction::Release);
        let mut bytes = [0; 128];
        let len = self.mouse_encoder.encode(&self.mouse_event, &mut bytes)?;
        self.write_input(&bytes[..len])
    }

    /// Ghostty's encoder takes integer cell pixels; the GPUI grid has fractional
    /// metrics. Normalize into the same integer pixel space as Terminal::resize
    /// so clicks near the bottom/right never drift into the preceding cell.
    fn configure_mouse(&mut self, position: (f32, f32), modifiers: Modifiers, size: (f32, f32)) {
        let mut mods = Mods::empty();
        if modifiers.shift {
            mods |= Mods::SHIFT;
        }
        if modifiers.control {
            mods |= Mods::CTRL;
        }
        if modifiers.alt {
            mods |= Mods::ALT;
        }
        if modifiers.platform {
            mods |= Mods::SUPER;
        }
        let (columns, rows) = *self.grid_size.lock();
        let cell_w = cell_width().round();
        let cell_h = cell_height().round();
        let right = (size.0 - TERMINAL_PADDING - columns as f32 * cell_width()).max(0.) as u32;
        let bottom = (size.1 - TERMINAL_PADDING - rows as f32 * cell_height()).max(0.) as u32;
        self.mouse_event
            .set_mods(mods)
            .set_position(mouse::Position {
                x: TERMINAL_PADDING + (position.0 - TERMINAL_PADDING) * cell_w / cell_width(),
                y: TERMINAL_PADDING + (position.1 - TERMINAL_PADDING) * cell_h / cell_height(),
            });
        self.mouse_encoder
            .set_options_from_terminal(&self.terminal)
            .set_size(mouse::EncoderSize {
                screen_width: TERMINAL_PADDING as u32 + columns as u32 * cell_w as u32 + right,
                screen_height: TERMINAL_PADDING as u32 + rows as u32 * cell_h as u32 + bottom,
                cell_width: cell_width().round() as u32,
                cell_height: cell_height().round() as u32,
                padding_top: TERMINAL_PADDING as u32,
                padding_left: TERMINAL_PADDING as u32,
                padding_bottom: bottom,
                padding_right: right,
            })
            .set_track_last_cell(true);
    }

    pub(crate) fn scroll(
        &mut self,
        lines: isize,
        pointer_x: f32,
        pointer_y: f32,
        modifiers: Modifiers,
        viewport_width: f32,
        viewport_height: f32,
    ) -> Result<bool> {
        if self.terminal.is_mouse_tracking()? {
            self.configure_mouse(
                (pointer_x, pointer_y),
                modifiers,
                (viewport_width, viewport_height),
            );
            self.mouse_encoder.set_any_button_pressed(false);

            let button = if lines < 0 {
                MouseButton::Four
            } else {
                MouseButton::Five
            };
            // Pace application input: one wheel event forwards at most a few
            // presses even when the gesture carries more lines. Sustained
            // motion keeps arriving as further events, while a single coarse
            // event can no longer queue a redraw storm — nvim answers every
            // press with a full ~2KB redraw and does not coalesce them.
            let repetitions = lines.unsigned_abs().clamp(1, 3);
            let mut encoded = Vec::with_capacity(repetitions * 64);
            let mut event_bytes = [0_u8; 128];
            for _ in 0..repetitions {
                self.mouse_event
                    .set_button(Some(button))
                    .set_action(MouseAction::Press);
                let written = self
                    .mouse_encoder
                    .encode(&self.mouse_event, &mut event_bytes)?;
                encoded.extend_from_slice(&event_bytes[..written]);
                self.mouse_event.set_action(MouseAction::Release);
                let written = self
                    .mouse_encoder
                    .encode(&self.mouse_event, &mut event_bytes)?;
                encoded.extend_from_slice(&event_bytes[..written]);
            }
            self.write_input(&encoded)?;
            return Ok(false);
        }

        let alternate_screen = self.terminal.mode(Mode::ALT_SCREEN_SAVE)?
            || self.terminal.mode(Mode::ALT_SCREEN)?
            || self.terminal.mode(Mode::ALT_SCREEN_LEGACY)?;
        if alternate_screen {
            let sequence = if lines < 0 { b"\x1b[A" } else { b"\x1b[B" };
            // Same pacing as the mouse path above: at most a few keys per
            // wheel event so one coarse event cannot flood the application.
            let mut encoded = Vec::with_capacity(lines.unsigned_abs().min(3) * sequence.len());
            for _ in 0..lines.unsigned_abs().clamp(1, 3) {
                encoded.extend_from_slice(sequence);
            }
            self.write_input(&encoded)?;
            Ok(false)
        } else {
            self.terminal.scroll_viewport(ScrollViewport::Delta(lines));
            Ok(true)
        }
    }

    pub(crate) fn write_input(&self, bytes: &[u8]) -> Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let mut writer = self.writer.lock();
        writer.write_all(bytes)?;
        writer.flush()?;
        Ok(())
    }

    pub(crate) fn send_key(&mut self, event: &KeyDownEvent) -> Result<()> {
        let Some((key, unshifted)) =
            map_key(&event.keystroke.key, event.keystroke.key_char.as_deref())
        else {
            return Ok(());
        };
        let mut modifiers = Mods::empty();
        if event.keystroke.modifiers.shift {
            modifiers |= Mods::SHIFT;
        }
        if event.keystroke.modifiers.control {
            modifiers |= Mods::CTRL;
        }
        if event.keystroke.modifiers.alt {
            modifiers |= Mods::ALT;
        }
        if event.keystroke.modifiers.platform {
            modifiers |= Mods::SUPER;
        }

        self.key_event
            .set_action(Action::Press)
            .set_key(key)
            .set_mods(modifiers)
            .set_consumed_mods(if event.keystroke.modifiers.shift {
                Mods::SHIFT
            } else {
                Mods::empty()
            })
            .set_unshifted_codepoint(unshifted)
            .set_utf8(event.keystroke.key_char.clone());

        let mut encoded = Vec::with_capacity(32);
        self.key_encoder
            .set_options_from_terminal(&self.terminal)
            .set_macos_option_as_alt(key::OptionAsAlt::True)
            .encode_to_vec(&self.key_event, &mut encoded)?;
        if !encoded.is_empty() {
            let mut writer = self.writer.lock();
            writer.write_all(&encoded)?;
            writer.flush()?;
        }
        Ok(())
    }

    /// Background of the last presented frame, including application theme changes.
    pub(crate) fn background_color(&self) -> u32 {
        self.cached_defaults
            .map_or(0x000000, |(_, background)| background)
    }

    /// Foreground of the last presented frame. The view paints bar and
    /// underline cursors with it, the way terminals default the cursor to
    /// the cell's foreground when no OSC 12 cursor color is set.
    pub(crate) fn foreground_color(&self) -> u32 {
        self.cached_defaults
            .map_or(0xFFFFFF, |(foreground, _)| foreground)
    }

    pub(crate) fn snapshot(&mut self) -> Result<Option<Vec<Vec<RenderRun>>>> {
        // Respect synchronized output (DEC 2026): applications such as opencode
        // wrap each frame in begin/end sync so capable terminals present it
        // atomically. Presenting mid-frame dirty state shows torn intermediates
        // (logo/status-bar flicker) that Ghostty itself never displays. Defer
        // until the closing sequence; the pending dirty state accumulates and
        // the next post-sync snapshot presents the complete frame at once.
        if self.terminal.mode(Mode::SYNC_OUTPUT)? {
            return Ok(None);
        }
        let graphics_changed = self.graphics.update(&self.terminal)?;
        let snapshot = self.render_state.update(&self.terminal)?;
        let colors = snapshot.colors()?;
        let cursor = snapshot.cursor_viewport()?;
        let cursor_visible = snapshot.cursor_visible()?;
        let presentation = CursorPresentation {
            // DEC mode 25 can hide the cursor while the viewport still
            // reports its cell, so visibility gates the painted cell.
            cell: if cursor_visible {
                cursor.map(|position| (position.x, position.y))
            } else {
                None
            },
            shape: cursor_shape(snapshot.cursor_visual_style()?),
            blinking: snapshot.cursor_blinking()?,
        };
        let cursor_cell = presentation.cell;
        let default_foreground = color_value(colors.foreground);
        let default_background = color_value(colors.background);
        let row_count = snapshot.rows()? as usize;

        // Reuse cached rows unless the grid shape, the defaults unstyled
        // cells inherit, or a row's own content changed. The cursor row is
        // rebuilt whenever the cursor entered or left it, since cursor
        // movement alone does not dirty rows.
        let full_rebuild = row_count != self.cached_rows.len()
            || self.cached_defaults != Some((default_foreground, default_background));
        // Line editors (the shell, opencode's input) reposition the cursor
        // with cursor-addressing sequences that change no cells, so the
        // render state stays clean. Without this check the early return
        // below swallows pure cursor moves and the visible cursor freezes —
        // while full-screen apps such as nvim redraw cells on every move
        // and keep working. The same is true of pure cursor restyles:
        // nvim's mode switches (DECSCUSR) and blink requests (DEC mode 12)
        // dirty nothing, so the whole presentation joins the comparison.
        // The loop's `cursor_touched` rebuild then repaints exactly the
        // rows the cursor entered or left.
        if !graphics_changed
            && snapshot.dirty()? == Dirty::Clean
            && !full_rebuild
            && presentation == self.presented_cursor
        {
            // Nothing changed since the rows currently on screen were built.
            return Ok(None);
        }
        let mut rows = Vec::with_capacity(row_count);
        let mut row_iterator = self.row_iterator.update(&snapshot)?;
        let mut row_index = 0_u16;
        let mut grapheme = String::with_capacity(8);

        while let Some(row) = row_iterator.next() {
            let cursor_touched = Some(row_index) == self.presented_cursor.cell.map(|(_, y)| y)
                || cursor_cell.is_some_and(|(_, y)| y == row_index);
            if !full_rebuild && !cursor_touched && !row.dirty()? {
                rows.push(self.cached_rows[row_index as usize].clone());
            } else {
                rows.push(Self::build_row(
                    &mut self.cell_iterator,
                    row,
                    cursor_cell,
                    default_foreground,
                    default_background,
                    row_index,
                    &mut grapheme,
                )?);
                row.set_dirty(false)?;
            }
            row_index += 1;
        }
        snapshot.set_dirty(Dirty::Clean)?;
        self.cached_rows = rows.clone();
        self.cached_defaults = Some((default_foreground, default_background));
        self.presented_cursor = presentation;
        Ok(Some(rows))
    }

    /// The cursor state of the last presented frame (see
    /// [`CursorPresentation`]). Read by the view after each present.
    pub(crate) fn cursor_presentation(&self) -> CursorPresentation {
        self.presented_cursor
    }

    /// Builds the [`RenderRun`]s for one grid row from the terminal cells.
    ///
    /// Takes the cell iterator explicitly (rather than `&mut self`) so the
    /// snapshot loop can hold the row iterator and the row cache side by
    /// side: all three are disjoint field borrows.
    fn build_row<'alloc>(
        cell_iterator: &mut CellIterator<'alloc>,
        row: &RowIteration<'alloc, '_>,
        cursor: Option<(u16, u16)>,
        default_foreground: u32,
        default_background: u32,
        row_index: u16,
        grapheme: &mut String,
    ) -> Result<Vec<RenderRun>> {
        let mut runs: Vec<RenderRun> = Vec::new();
        let mut current: Option<(CellStyle, Option<BlockKind>)> = None;
        let mut current_text = String::new();
        let mut current_columns = 0_u16;
        let mut cell_iteration = cell_iterator.update(row)?;
        let mut column = 0_u16;
        while let Some(cell) = cell_iteration.next() {
            grapheme.clear();
            if cell.graphemes_len()? > 0 {
                cell.graphemes_utf8(grapheme)?;
            } else if !matches!(
                cell.raw_cell()?.wide()?,
                CellWide::SpacerTail | CellWide::SpacerHead
            ) {
                grapheme.push(' ');
            }

            if grapheme.starts_with('\u{10EEEE}') {
                grapheme.clear();
                grapheme.push(' ');
            }

            // Cells without explicit styling use the terminal defaults,
            // which avoids color and style lookups for empty regions.
            let (foreground, background, bold, italic, underline) = if cell.has_styling()? {
                let mut foreground = cell
                    .fg_color()?
                    .map(color_value)
                    .unwrap_or(default_foreground);
                let mut background = cell
                    .bg_color()?
                    .map(color_value)
                    .unwrap_or(default_background);
                let style = cell.style()?;
                let bold = style.bold;
                let italic = style.italic;
                let underline = style.underline != libghostty_vt::style::Underline::None;
                if style.inverse {
                    std::mem::swap(&mut foreground, &mut background);
                }
                (foreground, background, bold, italic, underline)
            } else {
                (default_foreground, default_background, false, false, false)
            };

            let style = CellStyle {
                foreground,
                background,
                bold,
                italic,
                underline,
                cursor: cursor
                    .is_some_and(|position| position.0 == column && position.1 == row_index),
            };
            let kind = block_kind(grapheme);
            // Block fills are emitted as spaces so the view paints them as
            // rects (see `RenderRun::block`); the space keeps the cell
            // advance while contributing no glyph ink.
            let emitted = if kind.is_some() {
                " "
            } else {
                grapheme.as_str()
            };
            if current == Some((style, kind)) {
                current_text.push_str(emitted);
                current_columns += 1;
            } else {
                if let Some((previous_style, previous_kind)) = current.replace((style, kind)) {
                    runs.push(RenderRun {
                        text: std::mem::take(&mut current_text).into(),
                        columns: current_columns,
                        style: previous_style,
                        block: previous_kind,
                    });
                }
                current_text.push_str(emitted);
                current_columns = 1;
            }
            column += 1;
        }
        if let Some((style, kind)) = current {
            runs.push(RenderRun {
                text: current_text.into(),
                columns: current_columns,
                style,
                block: kind,
            });
        }
        Ok(runs)
    }
}

fn default_shell() -> PathBuf {
    env::var_os("SHELL")
        .filter(|shell| !shell.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/bin/sh"))
}

fn color_value(color: RgbColor) -> u32 {
    ((color.r as u32) << 16) | ((color.g as u32) << 8) | color.b as u32
}

/// Maps the render state's cursor visual style onto the app's shape enum.
/// The libghostty enum is non-exhaustive, so unknown styles fall back to
/// the terminal's block default.
fn cursor_shape(style: CursorVisualStyle) -> CursorShape {
    match style {
        CursorVisualStyle::Bar => CursorShape::Bar,
        CursorVisualStyle::Underline => CursorShape::Underline,
        CursorVisualStyle::BlockHollow => CursorShape::Hollow,
        CursorVisualStyle::Block => CursorShape::Block,
        // Future non-exhaustive additions fall back to the terminal default.
        _ => CursorShape::Block,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_reporting_and_osc8_links_follow_terminal_modes() {
        struct Capture(Arc<Mutex<Vec<u8>>>);
        impl Write for Capture {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let (mut session, _) = TerminalSession::spawn(
            crate::workspace::WorkspaceTab::Terminal,
            &std::env::temp_dir(),
            AgentKind::DEFAULT,
        )
        .unwrap();
        let captured = Arc::new(Mutex::new(Vec::new()));
        session.writer = Arc::new(Mutex::new(Box::new(Capture(captured.clone()))));
        let position = (
            TERMINAL_PADDING + cell_width() * 2.5,
            TERMINAL_PADDING + cell_height() * 1.5,
        );
        let size = (800., 600.);
        assert!(!session.mouse_tracking());
        session.feed(b"\x1b[?1002h\x1b[?1006h");
        assert!(session.mouse_tracking());
        session
            .send_mouse(
                MouseAction::Press,
                Some(MouseButton::Left),
                position,
                Modifiers::default(),
                size,
            )
            .unwrap();
        session
            .send_mouse(
                MouseAction::Release,
                Some(MouseButton::Left),
                position,
                Modifiers::default(),
                size,
            )
            .unwrap();
        assert_eq!(&*captured.lock(), b"\x1b[<0;3;2M\x1b[<0;3;2m");
        captured.lock().clear();
        session
            .send_mouse(
                MouseAction::Motion,
                None,
                position,
                Modifiers::default(),
                size,
            )
            .unwrap();
        assert!(
            captured.lock().is_empty(),
            "button-motion mode must suppress hover"
        );
        // Fractional GPUI cell heights must not accumulate a row of drift.
        captured.lock().clear();
        let lower_cell = (
            TERMINAL_PADDING + cell_width() * 90.2,
            TERMINAL_PADDING + cell_height() * 30.2,
        );
        session
            .send_mouse(
                MouseAction::Press,
                Some(MouseButton::Left),
                lower_cell,
                Modifiers::default(),
                (1000., 800.),
            )
            .unwrap();
        assert_eq!(&*captured.lock(), b"\x1b[<0;91;31M");
        session.feed(b"\x1b[?1002l");
        assert!(!session.mouse_tracking());
        session.feed(b"\x1b[H\x1b]8;;https://example.com/hidden\x1b\\label\x1b]8;;\x1b\\");
        assert_eq!(
            session.hyperlink_at(0, 0).as_deref(),
            Some("https://example.com/hidden")
        );
        assert_eq!(session.hyperlink_at(5, 0), None);
        session._child.kill().ok();
    }

    /// Locks in the contract `snapshot()` relies on: DEC 2026 toggles
    /// `Mode::SYNC_OUTPUT`, and skipping `RenderState::update` while sync is
    /// active does not lose content — the post-sync update still presents the
    /// complete frame. `snapshot()` returns `Ok(None)` (keep cached rows)
    /// while sync is active for exactly this reason.
    #[test]
    fn synchronized_output_buffers_a_frame_until_sync_end() {
        let mut terminal = Terminal::new(20, 5).unwrap();
        let mut render_state = RenderState::new().unwrap();

        terminal.vt_write(b"\x1b[?2026h");
        assert!(
            terminal.mode(Mode::SYNC_OUTPUT).unwrap(),
            "DECSET 2026 should enter synchronized-output mode"
        );

        // Partial frame content written inside sync; snapshot() would defer
        // here, so deliberately perform no render-state update yet.
        terminal.vt_write(b"hello");
        terminal.vt_write(b" world");

        terminal.vt_write(b"\x1b[?2026l");
        assert!(
            !terminal.mode(Mode::SYNC_OUTPUT).unwrap(),
            "DECRST 2026 should leave synchronized-output mode"
        );

        // One post-sync update presents the whole buffered frame.
        let snapshot = render_state.update(&terminal).unwrap();
        assert_ne!(snapshot.dirty().unwrap(), Dirty::Clean);

        let mut row_iterator_store = RowIterator::new().unwrap();
        let mut row_iterator = row_iterator_store.update(&snapshot).unwrap();
        let row = row_iterator.next().expect("first row should exist");
        let mut cell_iterator_store = CellIterator::new().unwrap();
        let mut cell_iterator = cell_iterator_store.update(row).unwrap();
        let mut text = String::new();
        let mut grapheme = String::new();
        while let Some(cell) = cell_iterator.next() {
            grapheme.clear();
            if cell.graphemes_len().unwrap() > 0 {
                cell.graphemes_utf8(&mut grapheme).unwrap();
                text.push_str(&grapheme);
            } else {
                text.push(' ');
            }
        }
        assert!(
            text.starts_with("hello world"),
            "buffered frame should present complete content, got {text:?}"
        );
    }

    #[test]
    fn block_kind_classifies_only_exact_fill_cells() {
        assert_eq!(block_kind("▀"), Some(BlockKind::Upper));
        assert_eq!(block_kind("▄"), Some(BlockKind::Lower));
        assert_eq!(block_kind("█"), Some(BlockKind::Full));
        assert_eq!(block_kind(" "), None);
        assert_eq!(block_kind(""), None);
        assert_eq!(block_kind("▀̲"), None);
        assert_eq!(block_kind("─"), None);
    }

    /// Block fills must split into their own runs with space text so the view
    /// can paint them as rects: `▀▄█` fed side by side become three runs.
    #[test]
    fn block_fills_split_into_marked_space_runs() {
        let cwd = std::env::temp_dir();
        let (mut session, _output) = TerminalSession::spawn(
            crate::workspace::WorkspaceTab::Terminal,
            &cwd,
            AgentKind::DEFAULT,
        )
        .expect("spawning a shell for the block-fill test");
        // Only these bytes are fed, so the snapshot is fully determined by
        // them regardless of any shell output waiting in the channel.
        session.feed("▀▄█".as_bytes());
        let rows = session
            .snapshot()
            .expect("snapshot should succeed")
            .expect("fresh content should present a frame");
        let first: Vec<(String, u16, Option<BlockKind>)> = rows[0]
            .iter()
            .take(3)
            .map(|run| (run.text.to_string(), run.columns, run.block))
            .collect();
        assert_eq!(
            first,
            vec![
                (" ".to_string(), 1, Some(BlockKind::Upper)),
                (" ".to_string(), 1, Some(BlockKind::Lower)),
                (" ".to_string(), 1, Some(BlockKind::Full)),
            ]
        );
        session._child.kill().ok();
    }

    /// Incremental snapshots must stay correct when clean rows are reused:
    /// overwriting one row keeps the others, and moving the cursor rebuilds
    /// exactly the rows it entered and left (cursor motion alone does not
    /// dirty rows, so the old cell must lose its cursor flag).
    #[test]
    fn snapshot_reuses_clean_rows_and_tracks_cursor() {
        let cwd = std::env::temp_dir();
        let (mut session, _output) = TerminalSession::spawn(
            crate::workspace::WorkspaceTab::Terminal,
            &cwd,
            AgentKind::DEFAULT,
        )
        .expect("spawning a shell for the incremental test");
        let row_text = |rows: &[Vec<RenderRun>], index: usize| -> String {
            rows[index].iter().map(|run| run.text.as_ref()).collect()
        };

        session.feed(b"aaa");
        let rows = session
            .snapshot()
            .expect("snapshot should succeed")
            .expect("fresh content should present a frame");
        assert!(row_text(&rows, 0).starts_with("aaa"));

        // Overwrite the same row: only it rebuilds, the rest is reused.
        session.feed(b"\rbbb");
        let rows = session
            .snapshot()
            .expect("snapshot should succeed")
            .expect("changed row should present a frame");
        assert!(row_text(&rows, 0).starts_with("bbb"));
        assert!(rows[0].iter().any(|run| run.style.cursor));

        // Move the cursor down and write: row 0 must lose its cursor flag
        // even though its text did not change.
        session.feed(b"\x1b[2;1Hccc");
        let rows = session
            .snapshot()
            .expect("snapshot should succeed")
            .expect("cursor move should present a frame");
        assert!(row_text(&rows, 0).starts_with("bbb"));
        assert!(
            rows[0].iter().all(|run| !run.style.cursor),
            "row 0 should lose the cursor flag, got {:?}",
            rows[0]
                .iter()
                .map(|run| (run.text.to_string(), run.style.cursor))
                .collect::<Vec<_>>(),
        );
        assert!(row_text(&rows, 1).starts_with("ccc"));
        assert!(rows[1].iter().any(|run| run.style.cursor));
        session._child.kill().ok();
    }

    #[test]
    fn activity_screen_ignores_scrolled_viewport_history() {
        let cwd = std::env::temp_dir();
        let (mut session, _output) = TerminalSession::spawn(
            crate::workspace::WorkspaceTab::Terminal,
            &cwd,
            AgentKind::DEFAULT,
        )
        .expect("spawning a shell for the active-screen test");
        session.feed(b"OLD APPROVAL\r\n");
        for index in 0..(crate::metrics::INITIAL_ROWS + 10) {
            session.feed(format!("live-{index}\r\n").as_bytes());
        }
        session.terminal.scroll_viewport(ScrollViewport::Top);

        let screen = session
            .activity_screen()
            .expect("active screen should be readable while scrolled back");
        assert!(!screen.contains("OLD APPROVAL"));
        assert!(screen.contains("live-"));
        session._child.kill().ok();
    }

    /// A grid-shape change invalidates the row cache: after a resize the
    /// snapshot presents the new row count instead of stale cached rows.
    #[test]
    fn snapshot_rebuilds_after_resize() {
        let cwd = std::env::temp_dir();
        let (mut session, _output) = TerminalSession::spawn(
            crate::workspace::WorkspaceTab::Terminal,
            &cwd,
            AgentKind::DEFAULT,
        )
        .expect("spawning a shell for the resize test");
        session.feed(b"aaa");
        let rows = session
            .snapshot()
            .expect("snapshot should succeed")
            .expect("fresh content should present a frame");
        assert_eq!(rows.len(), crate::metrics::INITIAL_ROWS as usize);

        session
            .resize(80, 24)
            .expect("headless resize should succeed");
        let rows = session
            .snapshot()
            .expect("snapshot should succeed")
            .expect("resized grid should present a frame");
        assert_eq!(rows.len(), 24);
        session._child.kill().ok();
    }

    /// Pure cursor restyles and visibility toggles must present a frame.
    ///
    /// DECSCUSR (nvim's mode switches) and DEC modes 12/25 change no cells,
    /// so the render state stays clean; swallowing those frames would freeze
    /// the cursor's shape and leave a hidden cursor painted. Hiding the
    /// cursor must also drop its cell flag from the rows.
    #[test]
    fn snapshot_presents_cursor_restyles_and_visibility() {
        let cwd = std::env::temp_dir();
        let (mut session, _output) = TerminalSession::spawn(
            crate::workspace::WorkspaceTab::Terminal,
            &cwd,
            AgentKind::DEFAULT,
        )
        .expect("spawning a shell for the cursor-restyle test");
        // Only these bytes are fed, so the snapshot is fully determined by
        // them regardless of any shell output waiting in the channel.
        session.feed(b"abc");
        session
            .snapshot()
            .expect("snapshot should succeed")
            .expect("fresh content should present a frame");
        // The spawn-time default matches Ghostty: a blinking block.
        assert_eq!(session.cursor_presentation().shape, CursorShape::Block);
        assert!(session.cursor_presentation().blinking);
        assert_eq!(session.cursor_presentation().cell, Some((3, 0)));

        // nvim's insert mode: a steady bar. No cell changed, so this
        // exercises exactly the early return the shape must survive.
        session.feed(b"\x1b[6 q");
        session
            .snapshot()
            .expect("snapshot should succeed")
            .expect("a pure restyle should present a frame");
        assert_eq!(session.cursor_presentation().shape, CursorShape::Bar);
        assert!(!session.cursor_presentation().blinking);

        // A blink request through DECSCUSR 5 (blinking bar), then DEC 12.
        session.feed(b"\x1b[5 q");
        session
            .snapshot()
            .expect("snapshot should succeed")
            .expect("a blink request should present a frame");
        assert!(session.cursor_presentation().blinking);
        session.feed(b"\x1b[?12l");
        session
            .snapshot()
            .expect("snapshot should succeed")
            .expect("a blink stop should present a frame");
        assert!(!session.cursor_presentation().blinking);

        // Hiding (DEC 25) must drop the cursor flag from the row it sat on.
        session.feed(b"\x1b[?25l");
        let rows = session
            .snapshot()
            .expect("snapshot should succeed")
            .expect("hiding the cursor should present a frame");
        assert_eq!(session.cursor_presentation().cell, None);
        assert!(
            rows[0].iter().all(|run| !run.style.cursor),
            "a hidden cursor must not flag its cell, got {:?}",
            rows[0]
                .iter()
                .map(|run| (run.text.to_string(), run.style.cursor))
                .collect::<Vec<_>>(),
        );

        // Showing restores the cell flag.
        session.feed(b"\x1b[?25h");
        let rows = session
            .snapshot()
            .expect("snapshot should succeed")
            .expect("showing the cursor should present a frame");
        assert_eq!(session.cursor_presentation().cell, Some((3, 0)));
        assert!(rows[0].iter().any(|run| run.style.cursor));

        // A settled grid with an unchanged cursor presents nothing, so
        // idle frames stay free.
        assert!(
            session
                .snapshot()
                .expect("snapshot should succeed")
                .is_none(),
            "a settled grid should present no frame"
        );
        session._child.kill().ok();
    }

    /// A pure cursor move (no cell changes) must still present a frame.
    ///
    /// Line editors such as the shell and opencode's input reposition the
    /// cursor with cursor-addressing sequences instead of redrawing cells,
    /// so the render state stays clean. Swallowing that frame freezes the
    /// visible cursor and reads as broken left/right navigation, while
    /// full-screen apps such as nvim redraw on every move and keep working.
    #[test]
    fn snapshot_presents_pure_cursor_moves() {
        let cwd = std::env::temp_dir();
        let (mut session, _output) = TerminalSession::spawn(
            crate::workspace::WorkspaceTab::Terminal,
            &cwd,
            AgentKind::DEFAULT,
        )
        .expect("spawning a shell for the cursor-move test");
        // Only these bytes are fed, so the snapshot is fully determined by
        // them regardless of any shell output waiting in the channel.
        let cursor_column = |rows: &[Vec<RenderRun>]| -> usize {
            let mut column = 0_usize;
            for run in &rows[0] {
                if run.style.cursor {
                    return column;
                }
                column += run.columns as usize;
            }
            panic!("row 0 should carry the cursor flag");
        };
        let row_text = |rows: &[Vec<RenderRun>]| -> String {
            rows[0].iter().map(|run| run.text.as_ref()).collect()
        };

        session.feed(b"abc");
        let rows = session
            .snapshot()
            .expect("snapshot should succeed")
            .expect("fresh content should present a frame");
        assert!(row_text(&rows).starts_with("abc"));
        assert_eq!(cursor_column(&rows), 3);

        // Cursor back one: no cell changes, but the frame must present so
        // the visible cursor follows.
        session.feed(b"\x1b[D");
        let rows = session
            .snapshot()
            .expect("snapshot should succeed")
            .expect("a pure cursor move should present a frame");
        assert!(row_text(&rows).starts_with("abc"));
        assert_eq!(cursor_column(&rows), 2);

        // A settled grid presents nothing, so idle frames stay free.
        assert!(
            session
                .snapshot()
                .expect("snapshot should succeed")
                .is_none(),
            "a settled grid should present no frame"
        );
        session._child.kill().ok();
    }
}
