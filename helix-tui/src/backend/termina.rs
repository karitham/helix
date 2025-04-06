use std::{
    fmt,
    io::{self, Write as _},
};

use helix_view::{
    graphics::{CursorKind, Rect, UnderlineStyle},
    theme::{Color, Modifier},
};
use termina::{
    escape::{
        csi::{self, Csi},
        dcs::{self, Dcs},
        OneBased,
    },
    style::{CursorStyle, RgbColor},
    Event, PlatformTerminal, Terminal as _,
};
use termini::TermInfo;

use crate::{buffer::Cell, terminal::Config};

use super::Backend;

// These macros are helpers to set/unset modes like bracketed paste or enter/exit the alternate
// screen.
macro_rules! decset {
    ($mode:ident) => {
        Csi::Mode(csi::Mode::SetDecPrivateMode(csi::DecPrivateMode::Code(
            csi::DecPrivateModeCode::$mode,
        )))
    };
}
macro_rules! decreset {
    ($mode:ident) => {
        Csi::Mode(csi::Mode::ResetDecPrivateMode(csi::DecPrivateMode::Code(
            csi::DecPrivateModeCode::$mode,
        )))
    };
}

fn term_program() -> Option<String> {
    // Some terminals don't set $TERM_PROGRAM
    match std::env::var("TERM_PROGRAM") {
        Err(_) => std::env::var("TERM").ok(),
        Ok(term_program) => Some(term_program),
    }
}
fn vte_version() -> Option<usize> {
    std::env::var("VTE_VERSION").ok()?.parse().ok()
}
fn reset_cursor_approach(terminfo: TermInfo) -> String {
    let mut reset_str = Csi::Cursor(csi::Cursor::CursorStyle(CursorStyle::Default)).to_string();

    if let Some(termini::Value::Utf8String(se_str)) = terminfo.extended_cap("Se") {
        reset_str.push_str(se_str);
    };

    reset_str.push_str(
        terminfo
            .utf8_string_cap(termini::StringCapability::CursorNormal)
            .unwrap_or(""),
    );

    reset_str
}

#[derive(Debug, Default, Clone, Copy)]
struct Capabilities {
    kitty_keyboard: bool,
    synchronized_output: bool,
    true_color: bool,
    extended_underlines: bool,
}

#[derive(Debug)]
pub struct TerminaBackend {
    terminal: PlatformTerminal,
    config: Config,
    capabilities: Option<Capabilities>,
    reset_cursor_command: String,
}

impl TerminaBackend {
    pub fn new(config: Config) -> io::Result<Self> {
        let terminal = PlatformTerminal::new()?;
        Ok(Self {
            terminal,
            config,
            capabilities: None,
            reset_cursor_command: Csi::Cursor(csi::Cursor::CursorStyle(CursorStyle::Default))
                .to_string(),
        })
    }

    pub fn terminal(&self) -> &PlatformTerminal {
        &self.terminal
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities
            .expect("capabilities must be detected before use")
    }

    /// Helper that runs during the initial [Self::claim] of the terminal to query the terminal
    /// for VT extensions and states.
    fn detect_terminal_capabilities_and_state(&mut self) -> io::Result<()> {
        use std::time::Instant;

        // Colibri "midnight"
        const TEST_COLOR: RgbColor = RgbColor::new(59, 34, 76);

        if self.capabilities.is_some() {
            return Ok(());
        }

        let mut capabilities = Capabilities::default();
        let start = Instant::now();

        // Many terminal extensions can be detected by querying the terminal for the state of the
        // extension and then sending a request for the primary device attributes (which is
        // consistently supported by all terminals). If we receive the status of the feature (for
        // example the current Kitty keyboard flags) then we know that the feature is supported.
        // If we only receive the device attributes then we know it is not.
        write!(
            self.terminal,
            "{}{}{}{}{}{}{}",
            // Kitty keyboard
            Csi::Keyboard(csi::Keyboard::QueryFlags),
            // Synchronized output
            Csi::Mode(csi::Mode::QueryDecPrivateMode(csi::DecPrivateMode::Code(
                csi::DecPrivateModeCode::SynchronizedOutput
            ))),
            // True color and while we're at it, extended underlines:
            // <https://github.com/termstandard/colors?tab=readme-ov-file#querying-the-terminal>
            Csi::Sgr(csi::Sgr::Background(TEST_COLOR.into())),
            Csi::Sgr(csi::Sgr::UnderlineColor(TEST_COLOR.into())),
            Dcs::Request(dcs::DcsRequest::GraphicRendition),
            Csi::Sgr(csi::Sgr::Reset),
            // Finally request the primary device attributes
            Csi::Device(csi::Device::RequestPrimaryDeviceAttributes),
        )?;
        self.terminal.flush()?;

        loop {
            match self.terminal.read(Event::is_escape)? {
                Event::Csi(Csi::Keyboard(csi::Keyboard::ReportFlags(_))) => {
                    capabilities.kitty_keyboard = true;
                }
                Event::Csi(Csi::Mode(csi::Mode::ReportDecPrivateMode {
                    mode: csi::DecPrivateMode::Code(csi::DecPrivateModeCode::SynchronizedOutput),
                    setting: csi::DecModeSetting::Set | csi::DecModeSetting::Reset,
                })) => {
                    capabilities.synchronized_output = true;
                }
                Event::Dcs(dcs::Dcs::Response {
                    value: dcs::DcsResponse::GraphicRendition(sgrs),
                    ..
                }) => {
                    capabilities.true_color =
                        sgrs.contains(&csi::Sgr::Background(TEST_COLOR.into()));
                    capabilities.extended_underlines =
                        sgrs.contains(&csi::Sgr::UnderlineColor(TEST_COLOR.into()));
                }
                Event::Csi(Csi::Device(csi::Device::DeviceAttributes(_))) => break,
                _ => (),
            }
        }

        let end = Instant::now();
        log::debug!(
            "Detected terminal capabilities in {:?}: {capabilities:?}",
            end.duration_since(start)
        );

        capabilities.extended_underlines |= self.config.force_enable_extended_underlines;
        if let Ok(t) = termini::TermInfo::from_env() {
            capabilities.extended_underlines |= t.extended_cap("Smulx").is_some()
                || t.extended_cap("Su").is_some()
                || vte_version() >= Some(5102)
                // HACK: once WezTerm can support DECRQSS/DECRPSS for SGR we can remove this line.
                // <https://github.com/wezterm/wezterm/pull/6856>
                || matches!(term_program().as_deref(), Some("WezTerm"));
            self.reset_cursor_command = reset_cursor_approach(t);
        }

        self.capabilities = Some(capabilities);

        Ok(())
    }

    fn enable_mouse_capture(&mut self) -> io::Result<()> {
        if self.config.enable_mouse_capture {
            write!(
                self.terminal,
                "{}{}{}{}{}",
                decset!(MouseTracking),
                decset!(ButtonEventMouse),
                decset!(AnyEventMouse),
                decset!(RXVTMouse),
                decset!(SGRMouse),
            )?;
        }
        Ok(())
    }

    fn disable_mouse_capture(&mut self) -> io::Result<()> {
        if self.config.enable_mouse_capture {
            write!(
                self.terminal,
                "{}{}{}{}{}",
                decreset!(MouseTracking),
                decreset!(ButtonEventMouse),
                decreset!(AnyEventMouse),
                decreset!(RXVTMouse),
                decreset!(SGRMouse),
            )?;
        }
        Ok(())
    }

    fn enable_extensions(&mut self) -> io::Result<()> {
        const KEYBOARD_FLAGS: csi::KittyKeyboardFlags =
            csi::KittyKeyboardFlags::DISAMBIGUATE_ESCAPE_CODES
                .union(csi::KittyKeyboardFlags::REPORT_ALTERNATE_KEYS);

        let capabilities = &mut self.capabilities.unwrap();

        if capabilities.kitty_keyboard {
            write!(
                self.terminal,
                "{}{}",
                // Enable the flags we need.
                // NOTE: we do not need to `PopFlags` in `Self::restore`. Kitty keyboard flags
                // are automatically popped when the application exits the alternate screen (i.e.
                // enters the main screen).
                Csi::Keyboard(csi::Keyboard::PushFlags(KEYBOARD_FLAGS)),
                // Then request the current flags. We need to make sure the terminal enabled all
                // of the flags we require.
                Csi::Keyboard(csi::Keyboard::QueryFlags),
            )?;
            self.terminal.flush()?;

            let event = self.terminal.read(|event| {
                matches!(
                    event,
                    Event::Csi(Csi::Keyboard(csi::Keyboard::ReportFlags(_)))
                )
            })?;
            let Event::Csi(Csi::Keyboard(csi::Keyboard::ReportFlags(flags))) = event else {
                unreachable!();
            };
            if flags != KEYBOARD_FLAGS {
                log::info!("Turning off enhanced keyboard support because the terminal enabled different flags. Requested {KEYBOARD_FLAGS:?} but got {flags:?}");
                write!(
                    self.terminal,
                    "{}",
                    Csi::Keyboard(csi::Keyboard::PopFlags(1))
                )?;
                self.terminal.flush()?;
                capabilities.kitty_keyboard = false;
            }
        }

        Ok(())
    }
}

impl Backend for TerminaBackend {
    fn claim(&mut self) -> io::Result<()> {
        self.terminal.enter_raw_mode()?;

        write!(
            self.terminal,
            "{}{}{}{}",
            // Enter an alternate screen.
            decset!(ClearAndEnableAlternateScreen),
            decset!(BracketedPaste),
            decset!(FocusTracking),
            // Clear the buffer. TODO: is this needed? We entered an alternate screen above.
            Csi::Edit(csi::Edit::EraseInDisplay(csi::EraseInDisplay::EraseDisplay)),
        )?;
        self.enable_mouse_capture()?;

        self.detect_terminal_capabilities_and_state()?;
        self.enable_extensions()?;

        Ok(())
    }

    fn reconfigure(&mut self, mut config: Config) -> io::Result<()> {
        std::mem::swap(&mut self.config, &mut config);
        if self.config.enable_mouse_capture != config.enable_mouse_capture {
            if self.config.enable_mouse_capture {
                self.enable_mouse_capture()?;
            } else {
                self.disable_mouse_capture()?;
            }
        }
        self.capabilities.unwrap().extended_underlines |=
            self.config.force_enable_extended_underlines;
        Ok(())
    }

    fn restore(&mut self) -> io::Result<()> {
        self.disable_mouse_capture()?;
        write!(
            self.terminal,
            "{}{}{}{}",
            &self.reset_cursor_command,
            decreset!(BracketedPaste),
            decreset!(FocusTracking),
            decreset!(ClearAndEnableAlternateScreen),
        )?;
        self.terminal.flush()?;
        self.terminal.reset_mode()?;
        Ok(())
    }

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        if self.capabilities().synchronized_output {
            write!(self.terminal, "{}", decset!(SynchronizedOutput))?;
        }

        let mut fg = Color::Reset;
        let mut bg = Color::Reset;
        let mut underline_color = Color::Reset;
        let mut underline_style = UnderlineStyle::Reset;
        let mut modifier = Modifier::empty();
        let mut last_pos: Option<(u16, u16)> = None;
        for (x, y, cell) in content {
            // Move the cursor if the previous location was not (x - 1, y)
            if !matches!(last_pos, Some(p) if x == p.0 + 1 && y == p.1) {
                write!(
                    self.terminal,
                    "{}",
                    Csi::Cursor(csi::Cursor::Position {
                        col: OneBased::from_zero_based(x),
                        line: OneBased::from_zero_based(y),
                    })
                )?;
            }
            last_pos = Some((x, y));

            // TODO: multiple SGRs can be combined together into a single SGR escape.
            // Crossterm comments say that this can have a notable FPS boost in some scenarios.
            // It only combines FG+BG but we could write all SGR changes as one escape instead.
            if cell.modifier != modifier {
                write!(
                    self.terminal,
                    "{}",
                    ModifierDiff {
                        from: modifier,
                        to: cell.modifier,
                    }
                )?;
                modifier = cell.modifier;
            }
            if cell.fg != fg {
                write!(
                    self.terminal,
                    "{}",
                    Csi::Sgr(csi::Sgr::Foreground(cell.fg.into()))
                )?;
                fg = cell.fg;
            }
            if cell.bg != bg {
                write!(
                    self.terminal,
                    "{}",
                    Csi::Sgr(csi::Sgr::Background(cell.bg.into()))
                )?;
                bg = cell.bg;
            }
            let mut new_underline_style = cell.underline_style;
            if self.capabilities().extended_underlines {
                if cell.underline_color != underline_color {
                    write!(
                        self.terminal,
                        "{}",
                        Csi::Sgr(csi::Sgr::UnderlineColor(cell.underline_color.into()))
                    )?;
                    underline_color = cell.underline_color;
                }
            } else {
                match new_underline_style {
                    UnderlineStyle::Reset | UnderlineStyle::Line => (),
                    _ => new_underline_style = UnderlineStyle::Line,
                }
            }

            if new_underline_style != underline_style {
                write!(
                    self.terminal,
                    "{}",
                    Csi::Sgr(csi::Sgr::Underline(cell.underline_style.into()))
                )?;
                underline_style = new_underline_style;
            }

            write!(self.terminal, "{}", &cell.symbol)?;
        }

        write!(self.terminal, "{}", Csi::Sgr(csi::Sgr::Reset))?;

        if self.capabilities().synchronized_output {
            write!(self.terminal, "{}", decreset!(SynchronizedOutput))?;
        }

        Ok(())
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        write!(self.terminal, "{}", decreset!(ShowCursor))?;
        self.terminal.flush()
    }

    fn show_cursor(&mut self, kind: CursorKind) -> io::Result<()> {
        let style = match kind {
            CursorKind::Block => CursorStyle::SteadyBlock,
            CursorKind::Bar => CursorStyle::SteadyBar,
            CursorKind::Underline => CursorStyle::SteadyUnderline,
            CursorKind::Hidden => unreachable!(),
        };
        write!(
            self.terminal,
            "{}{}",
            decset!(ShowCursor),
            Csi::Cursor(csi::Cursor::CursorStyle(style)),
        )?;
        self.terminal.flush()
    }

    fn get_cursor(&mut self) -> Result<(u16, u16), io::Error> {
        write!(
            self.terminal,
            "{}",
            csi::Csi::Cursor(csi::Cursor::RequestActivePositionReport),
        )?;
        self.terminal.flush()?;
        let event = self.terminal.read(|event| {
            matches!(
                event,
                Event::Csi(Csi::Cursor(csi::Cursor::ActivePositionReport { .. }))
            )
        })?;
        let Event::Csi(Csi::Cursor(csi::Cursor::ActivePositionReport { line, col })) = event else {
            unreachable!();
        };
        Ok((line.get_zero_based(), col.get_zero_based()))
    }

    fn set_cursor(&mut self, x: u16, y: u16) -> io::Result<()> {
        let col = OneBased::from_zero_based(x);
        let line = OneBased::from_zero_based(y);
        write!(
            self.terminal,
            "{}",
            Csi::Cursor(csi::Cursor::Position { line, col })
        )?;
        self.terminal.flush()
    }

    fn clear(&mut self) -> io::Result<()> {
        write!(
            self.terminal,
            "{}",
            Csi::Edit(csi::Edit::EraseInDisplay(csi::EraseInDisplay::EraseDisplay))
        )?;
        self.terminal.flush()
    }

    fn size(&self) -> io::Result<Rect> {
        let (rows, cols) = self.terminal.get_dimensions()?;
        Ok(Rect::new(0, 0, cols, rows))
    }

    fn flush(&mut self) -> io::Result<()> {
        self.terminal.flush()
    }
}

impl Drop for TerminaBackend {
    fn drop(&mut self) {
        let _ = self.disable_mouse_capture();
        let _ = write!(
            self.terminal,
            "{}{}{}{}",
            &self.reset_cursor_command,
            decreset!(BracketedPaste),
            decreset!(FocusTracking),
            decreset!(ClearAndEnableAlternateScreen),
        );
        let _ = self.terminal.flush();
        let _ = self.terminal.reset_mode();
    }
}

#[derive(Debug)]
struct ModifierDiff {
    from: Modifier,
    to: Modifier,
}

impl fmt::Display for ModifierDiff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use termina::{escape::csi::Sgr, style::*};

        let removed = self.from - self.to;
        if removed.contains(Modifier::REVERSED) {
            write!(f, "{}", Csi::Sgr(Sgr::Reverse(false)))?;
        }
        if removed.contains(Modifier::BOLD) || removed.contains(Modifier::DIM) {
            write!(f, "{}", Csi::Sgr(Sgr::Intensity(Intensity::Normal)))?;
        }
        if removed.contains(Modifier::ITALIC) {
            write!(f, "{}", Csi::Sgr(Sgr::Italic(false)))?;
        }
        if removed.contains(Modifier::CROSSED_OUT) {
            write!(f, "{}", Csi::Sgr(Sgr::StrikeThrough(false)))?;
        }
        if removed.contains(Modifier::HIDDEN) {
            write!(f, "{}", Csi::Sgr(Sgr::Invisible(false)))?;
        }
        if removed.contains(Modifier::SLOW_BLINK) || removed.contains(Modifier::RAPID_BLINK) {
            write!(f, "{}", Csi::Sgr(Sgr::Blink(Blink::None)))?;
        }

        let added = self.to - self.from;
        if added.contains(Modifier::REVERSED) {
            write!(f, "{}", Csi::Sgr(Sgr::Reverse(true)))?;
        }
        if added.contains(Modifier::BOLD) {
            write!(f, "{}", Csi::Sgr(Sgr::Intensity(Intensity::Bold)))?;
        }
        if added.contains(Modifier::DIM) {
            write!(f, "{}", Csi::Sgr(Sgr::Intensity(Intensity::Dim)))?;
        }
        if added.contains(Modifier::ITALIC) {
            write!(f, "{}", Csi::Sgr(Sgr::Italic(true)))?;
        }
        if added.contains(Modifier::CROSSED_OUT) {
            write!(f, "{}", Csi::Sgr(Sgr::StrikeThrough(true)))?;
        }
        if added.contains(Modifier::HIDDEN) {
            write!(f, "{}", Csi::Sgr(Sgr::Invisible(true)))?;
        }
        if added.contains(Modifier::SLOW_BLINK) {
            write!(f, "{}", Csi::Sgr(Sgr::Blink(Blink::Slow)))?;
        }
        if added.contains(Modifier::RAPID_BLINK) {
            write!(f, "{}", Csi::Sgr(Sgr::Blink(Blink::Rapid)))?;
        }

        Ok(())
    }
}
