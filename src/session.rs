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
    render::{CellIterator, Dirty, RenderState, RowIterator},
    screen::CellWide,
    style::RgbColor,
    terminal::{
        ConformanceLevel, DeviceAttributeFeature, DeviceAttributes, DeviceType, Mode,
        PrimaryDeviceAttributes, ScrollViewport, SecondaryDeviceAttributes, SizeReportSize,
        Terminal,
    },
};
use parking_lot::Mutex;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::{
    keys::map_key,
    metrics::{
        CELL_HEIGHT, CONTENT_BORDER, CONTENT_PADDING, INITIAL_COLS, INITIAL_ROWS, TERMINAL_PADDING,
        WORKSPACE_HEADER_HEIGHT, cell_width,
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
}

impl TerminalSession {
    pub(crate) fn spawn(
        tab: WorkspaceTab,
        cwd: &Path,
    ) -> Result<(Self, async_channel::Receiver<Vec<u8>>)> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: INITIAL_ROWS,
                cols: INITIAL_COLS,
                pixel_width: (INITIAL_COLS as f32 * cell_width()) as u16,
                pixel_height: (INITIAL_ROWS as f32 * CELL_HEIGHT) as u16,
            })
            .context("opening pseudo-terminal")?;

        let shell = default_shell();
        let mut command = CommandBuilder::new(&shell);
        command.cwd(cwd);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env("TERM_PROGRAM", "devcroft");
        command.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
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
            CELL_HEIGHT as u32,
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
                        cell_height: CELL_HEIGHT as u32,
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

        if let Some(program) = tab.command() {
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
            },
            output,
        ))
    }

    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        self.terminal.vt_write(bytes);
    }

    pub(crate) fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        *self.grid_size.lock() = (cols, rows);
        self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: (cols as f32 * cell_width()) as u16,
            pixel_height: (rows as f32 * CELL_HEIGHT) as u16,
        })?;
        self.terminal
            .resize(cols, rows, cell_width().round() as u32, CELL_HEIGHT as u32)?;
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

            let padding_left = CONTENT_PADDING + CONTENT_BORDER + TERMINAL_PADDING;
            let padding_top =
                WORKSPACE_HEADER_HEIGHT + CONTENT_PADDING + CONTENT_BORDER + TERMINAL_PADDING;
            let (columns, rows) = *self.grid_size.lock();
            let grid_width = columns as f32 * cell_width();
            let grid_height = rows as f32 * CELL_HEIGHT;
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
                    cell_height: CELL_HEIGHT as u32,
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
            let repetitions = lines.unsigned_abs().clamp(1, 12);
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
            let mut encoded = Vec::with_capacity(lines.unsigned_abs().min(12) * sequence.len());
            for _ in 0..lines.unsigned_abs().clamp(1, 12) {
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

    pub(crate) fn snapshot(&mut self) -> Result<Option<Vec<Vec<RenderRun>>>> {
        let snapshot = self.render_state.update(&self.terminal)?;
        if snapshot.dirty()? == Dirty::Clean {
            // Nothing changed since the rows currently on screen were built.
            return Ok(None);
        }
        let colors = snapshot.colors()?;
        let cursor = snapshot.cursor_viewport()?;
        let default_foreground = color_value(colors.foreground);
        let default_background = color_value(colors.background);
        let row_count = snapshot.rows()? as usize;
        let mut rows = Vec::with_capacity(row_count);
        let mut row_iterator = self.row_iterator.update(&snapshot)?;
        let mut row_index = 0_u16;
        let mut grapheme = String::with_capacity(8);

        while let Some(row) = row_iterator.next() {
            let mut runs: Vec<RenderRun> = Vec::new();
            let mut current_style: Option<CellStyle> = None;
            let mut current_text = String::new();
            let mut current_columns = 0_u16;
            let mut cell_iterator = self.cell_iterator.update(row)?;
            let mut column = 0_u16;
            while let Some(cell) = cell_iterator.next() {
                grapheme.clear();
                if cell.graphemes_len()? > 0 {
                    cell.graphemes_utf8(&mut grapheme)?;
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
                        .is_some_and(|position| position.x == column && position.y == row_index),
                };
                if current_style == Some(style) {
                    current_text.push_str(&grapheme);
                    current_columns += 1;
                } else {
                    if let Some(previous_style) = current_style.replace(style) {
                        runs.push(RenderRun {
                            text: std::mem::take(&mut current_text).into(),
                            columns: current_columns,
                            style: previous_style,
                        });
                    }
                    current_text.push_str(&grapheme);
                    current_columns = 1;
                }
                column += 1;
            }
            if let Some(style) = current_style {
                runs.push(RenderRun {
                    text: current_text.into(),
                    columns: current_columns,
                    style,
                });
            }
            row.set_dirty(false)?;
            rows.push(runs);
            row_index += 1;
        }
        snapshot.set_dirty(Dirty::Clean)?;
        Ok(Some(rows))
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
