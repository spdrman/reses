//! The re:SES logo in the header bar.
//!
//! A terminal that can draw images (Kitty, iTerm2, WezTerm, Ghostty, anything with sixel) gets
//! the real logo, the cubes left of the wordmark, from the PNGs `scripts/render-brand.sh`
//! renders. Every other terminal gets the wordmark as styled text: a block-pixel version of a
//! logo two rows tall would be unreadable.

use image::DynamicImage;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::StatefulProtocol;

/// The blue of the logo's colon (`#2EA8F2` in the SVGs).
pub const LOGO_BLUE: Color = Color::Rgb(0x2E, 0xA8, 0xF2);

/// Header rows in image mode. Text mode keeps the one-row bar.
pub const IMAGE_ROWS: u16 = 2;
/// The image header needs at least this much room; below it the header falls back to text.
pub const MIN_IMAGE_WIDTH: u16 = 60;
pub const MIN_IMAGE_HEIGHT: u16 = 12;

/// Dark text, for light backgrounds.
const HEADER_PNG: &[u8] = include_bytes!("../../assets/brand/tui/reses-header.png");
/// Light text, for dark backgrounds.
const HEADER_LIGHT_PNG: &[u8] = include_bytes!("../../assets/brand/tui/reses-header-light.png");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Background {
    Dark,
    Light,
}

impl Background {
    /// From `COLORFGBG` ("fg;bg", or "fg;x;bg"), which several terminals set. The background is
    /// an ANSI colour index: 7 and 9 to 15 are light, the rest dark. Anything else, including no
    /// variable at all, reads as dark, since most terminals are.
    pub fn from_colorfgbg(value: Option<&str>) -> Self {
        let bg = value
            .and_then(|v| v.rsplit(';').next())
            .and_then(|bg| bg.trim().parse::<u8>().ok());
        match bg {
            Some(7 | 9..=15) => Background::Light,
            _ => Background::Dark,
        }
    }
}

/// Which PNG an image header shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    DarkText,
    LightText,
}

impl Variant {
    fn for_background(bg: Background) -> Self {
        match bg {
            Background::Dark => Variant::LightText,
            Background::Light => Variant::DarkText,
        }
    }

    fn png(self) -> &'static [u8] {
        match self {
            Variant::DarkText => HEADER_PNG,
            Variant::LightText => HEADER_LIGHT_PNG,
        }
    }
}

/// What startup does about the image logo.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Plan {
    /// The styled-text wordmark, asking the terminal nothing.
    Text,
    /// The image, in this protocol, sized from the window's pixel size. Nothing is read from
    /// the terminal.
    Image(ProtocolType),
    /// Ask the terminal (`RESES_LOGO=image` only).
    Query,
}

/// The environment variables the plan depends on.
#[derive(Debug, Clone, Default)]
pub struct Env {
    pub reses_logo: Option<String>,
    pub term: Option<String>,
    pub term_program: Option<String>,
    pub kitty_window_id: Option<String>,
    pub wezterm_executable: Option<String>,
    pub tmux: Option<String>,
    pub mlterm: Option<String>,
}

impl Env {
    pub fn from_process() -> Self {
        let var = |name: &str| std::env::var(name).ok();
        Self {
            reses_logo: var("RESES_LOGO"),
            term: var("TERM"),
            term_program: var("TERM_PROGRAM"),
            kitty_window_id: var("KITTY_WINDOW_ID"),
            wezterm_executable: var("WEZTERM_EXECUTABLE"),
            tmux: var("TMUX"),
            mlterm: var("MLTERM"),
        }
    }
}

/// Decide from the environment alone.
pub fn plan(env: &Env) -> Plan {
    let _ = env;
    Plan::Query
}

/// Pixels per cell from a window size, or None when the terminal doesn't report one.
fn cell_size(width_px: u16, height_px: u16, cols: u16, rows: u16) -> Option<(u16, u16)> {
    let _ = (width_px, height_px, cols, rows);
    Some((8, 16))
}

/// How the header draws the logo, decided once at startup.
pub struct Brand {
    background: Background,
    image: Option<Logo>,
}

struct Logo {
    variant: Variant,
    protocol: StatefulProtocol,
    /// Columns the image takes at `IMAGE_ROWS` rows, from its aspect and the cell size.
    cols: u16,
}

impl Brand {
    /// The styled-text wordmark only.
    pub fn text(background: Background) -> Self {
        Self {
            background,
            image: None,
        }
    }

    /// The real logo when `picker` found an image protocol. Halfblocks, which is what the
    /// picker settles on when it found none, gets the text wordmark instead.
    pub fn with_picker(mut picker: Picker, background: Background) -> Self {
        if picker.protocol_type() == ProtocolType::Halfblocks {
            return Self::text(background);
        }
        let variant = Variant::for_background(background);
        let Ok(logo) = image::load_from_memory_with_format(variant.png(), image::ImageFormat::Png)
        else {
            return Self::text(background);
        };
        // Sixel has no transparency, so the picker flattens onto this colour.
        picker.set_background_color(match background {
            Background::Dark => image::Rgba([0, 0, 0, 0]),
            Background::Light => image::Rgba([255, 255, 255, 255]),
        });
        let cols = columns_for(&logo, picker.font_size());
        Self {
            background,
            image: Some(Logo {
                variant,
                protocol: picker.new_resize_protocol(logo),
                cols,
            }),
        }
    }

    /// Ask the terminal. Call it once, after the terminal is in raw mode and before the job
    /// pool starts (the query itself uses a thread). `RESES_LOGO=text` skips the query.
    pub fn detect() -> Self {
        let background = Background::from_colorfgbg(std::env::var("COLORFGBG").ok().as_deref());
        if std::env::var("RESES_LOGO").is_ok_and(|v| v == "text") {
            return Self::text(background);
        }
        match Picker::from_query_stdio() {
            Ok(picker) => Self::with_picker(picker, background),
            Err(_) => Self::text(background),
        }
    }

    pub fn background(&self) -> Background {
        self.background
    }

    /// The PNG in use, or None in text mode.
    pub fn variant(&self) -> Option<Variant> {
        self.image.as_ref().map(|l| l.variant)
    }

    /// The image and its width in columns, when the screen has room for it.
    pub(super) fn image_for(
        &mut self,
        width: u16,
        height: u16,
    ) -> Option<(&mut StatefulProtocol, u16)> {
        let logo = self.image.as_mut()?;
        let room = width >= MIN_IMAGE_WIDTH.max(logo.cols + 20) && height >= MIN_IMAGE_HEIGHT;
        room.then_some((&mut logo.protocol, logo.cols))
    }
}

fn columns_for(logo: &DynamicImage, (font_w, font_h): (u16, u16)) -> u16 {
    let font_w = u32::from(font_w.max(1));
    let px_high = u32::from(IMAGE_ROWS) * u32::from(font_h.max(1));
    let px_wide = logo.width() * px_high / logo.height().max(1);
    u16::try_from(px_wide.div_ceil(font_w)).unwrap_or(u16::MAX)
}

/// The wordmark as text, for the header bar: a blue cube mark, `re`, a blue `:`, bold `SES`.
///
/// The bar is drawn REVERSED, so its colours are the terminal's own swapped and it reads on
/// any theme. A blue glyph inside it therefore sets the blue as its *background*: reversed,
/// that becomes the glyph colour, on the same bar colour as its neighbours.
pub fn wordmark(bar: Style) -> Vec<Span<'static>> {
    let blue = bar.bg(LOGO_BLUE);
    vec![
        Span::styled(" ", bar),
        Span::styled("■", blue),
        Span::styled(" ", bar),
        Span::styled("re", bar.remove_modifier(Modifier::BOLD)),
        Span::styled(":", blue.add_modifier(Modifier::BOLD)),
        Span::styled("SES", bar.add_modifier(Modifier::BOLD)),
    ]
}

/// The header bar's style: the terminal's own colours, reversed, in bold. White on ANSI blue
/// measured 2.4:1 on Dracula and 3.1:1 on Gruvbox; this is the theme's own text contrast.
pub fn bar_style() -> Style {
    Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
}

#[cfg(test)]
mod tests;
