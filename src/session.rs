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
    render::{CellIterator, Dirty, RenderState, RowIteration, RowIterator},
    screen::CellWide,
    style::RgbColor,
    terminal::{
        ConformanceLevel, DeviceAttributeFeature, DeviceAttributes, DeviceType, Mode,
        Point, PointCoordinate, PrimaryDeviceAttributes, ScrollViewport, SecondaryDeviceAttributes,
        SizeReportSize, Terminal,
    },
};
use parking_lot::Mutex;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::{
    agent::AgentKind,
    keys::map_key,
    metrics::{
        INITIAL_COLS, INITIAL_ROWS, TERMINAL_PADDING, WORKSPACE_HEADER_HEIGHT, cell_height,
        cell_width,
    },
    workspace::WorkspaceTab,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct CellStyle {
    pub(crate) foreground: u32,
    pub(crate) background: u32,
    pub(crate) bold: bool,
    pub(crate) italic: bool,
    pub(crate) underline: bool,
    pub(crate) cursor: bool,
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
    /// Viewport cursor cell of `cached_rows`, to force-rebuild rows the
    /// cursor entered or left.
    last_cursor: Option<(u16, u16)>,
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
        terminal.set_scrollback_max_lines(Some(10_000))?;
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
                mouse_encoder: MouseEncoder::new()?,
                mouse_event: MouseEvent::new()?,
                writer,
                grid_size,
                master: pair.master,
                _child: child,
                cached_rows: Vec::new(),
                cached_defaults: None,
                last_cursor: None,
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

            let padding_left = TERMINAL_PADDING;
            let padding_top = WORKSPACE_HEADER_HEIGHT + TERMINAL_PADDING;
            let (columns, rows) = *self.grid_size.lock();
            let grid_width = columns as f32 * cell_width();
            let grid_height = rows as f32 * cell_height();
            self.mouse_event
                .set_mods(mods)
                .set_position(mouse::Position {
                    x: pointer_x,
                    y: pointer_y,
                });
            self.mouse_encoder
                .set_options_from_terminal(&self.terminal)
                .set_size(mouse::EncoderSize {
                    screen_width: viewport_width.max(1.) as u32,
                    screen_height: viewport_height.max(1.) as u32,
                    cell_width: cell_width().round() as u32,
                    cell_height: cell_height().round() as u32,
                    padding_top: padding_top as u32,
                    padding_bottom: (viewport_height - padding_top - grid_height).max(0.) as u32,
                    padding_right: (viewport_width - padding_left - grid_width).max(0.) as u32,
                    padding_left: padding_left as u32,
                })
                .set_any_button_pressed(false)
                .set_track_last_cell(true);

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
        let snapshot = self.render_state.update(&self.terminal)?;
        let colors = snapshot.colors()?;
        let cursor = snapshot.cursor_viewport()?;
        let default_foreground = color_value(colors.foreground);
        let default_background = color_value(colors.background);
        let row_count = snapshot.rows()? as usize;
        let cursor_cell = cursor.map(|position| (position.x, position.y));

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
        // and keep working. The loop's `cursor_touched` rebuild then
        // repaints exactly the rows the cursor entered or left.
        if snapshot.dirty()? == Dirty::Clean && !full_rebuild && cursor_cell == self.last_cursor {
            // Nothing changed since the rows currently on screen were built.
            return Ok(None);
        }
        let mut rows = Vec::with_capacity(row_count);
        let mut row_iterator = self.row_iterator.update(&snapshot)?;
        let mut row_index = 0_u16;
        let mut grapheme = String::with_capacity(8);

        while let Some(row) = row_iterator.next() {
            let cursor_touched = Some(row_index) == self.last_cursor.map(|(_, y)| y)
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
        self.last_cursor = cursor_cell;
        Ok(Some(rows))
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

#[cfg(test)]
mod tests {
    use super::*;

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
