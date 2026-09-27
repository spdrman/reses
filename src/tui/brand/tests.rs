//! Tests for the header logo: when a terminal gets the image or the text wordmark, what the
//! wordmark looks like, and how much room each leaves the screen below.
//!
//! I render the app with a probe view that only remembers the area it was handed, so the
//! tests can measure the header without any real screen in the way. The plan tests build an
//! `Env` by hand, since reading the real environment would make them depend on the terminal
//! running the suite.

use std::cell::Cell;
use std::rc::Rc;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use ratatui_image::picker::{Picker, ProtocolType};

use super::*;
use crate::tui::testing::{self, buffer, screen};
use crate::tui::{App, Ctx, Transition, View};

/// A view that only has a title and remembers the area it was given to draw in.
struct Probe {
    area: Rc<Cell<Rect>>,
}

impl View for Probe {
    /// I title myself like an inbox, so the header has realistic text next to the logo.
    fn title(&self) -> String {
        "Inbox s3://mail/inbound/".into()
    }
    /// I draw nothing and remember the area, so a test can see how much room the header left.
    fn render(&mut self, _frame: &mut Frame, area: Rect, _ctx: &Ctx) {
        self.area.set(area);
    }
    /// I ignore every key.
    fn on_key(&mut self, _key: KeyEvent, _ctx: &mut Ctx) -> Transition {
        Transition::None
    }
    /// I show a single hint, enough for the status line to have something in it.
    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![("q", "quit")]
    }
}

/// I build an app with `brand` in the header and the probe as its only screen, handing
/// back the cell the probe writes its area into.
fn app(brand: Brand) -> (App, Rc<Cell<Rect>>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut ctx = testing::ctx(dir.path(), None);
    ctx.brand = brand;
    let area = Rc::new(Cell::new(Rect::default()));
    let app = App::with_view(ctx, Box::new(Probe { area: area.clone() }));
    (app, area, dir)
}

/// I build an image picker for `protocol` at an 8x16 font, without asking any terminal.
fn picker(protocol: ProtocolType) -> Picker {
    let mut p = Picker::from_fontsize((8, 16));
    p.set_protocol_type(protocol);
    p
}

/// I read row `y` of the buffer back as text, one symbol per cell.
fn row(buf: &Buffer, y: u16) -> String {
    (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
}

/// The column where `text` starts on row `y`, counting cells: an image escape sits in one cell,
/// so a byte offset into the row would be far off.
fn col_of(buf: &Buffer, y: u16, text: &str) -> Option<u16> {
    let chars: Vec<String> = text.chars().map(String::from).collect();
    let n = chars.len() as u16;
    (0..buf.area.width.saturating_sub(n)).find(|&x| {
        chars
            .iter()
            .enumerate()
            .all(|(i, c)| buf[(x + i as u16, y)].symbol() == c)
    })
}

/// Columns of row `y` whose symbol is `s`, left to right.
fn cols_of(buf: &Buffer, y: u16, s: &str) -> Vec<u16> {
    (0..buf.area.width)
        .filter(|&x| buf[(x, y)].symbol() == s)
        .collect()
}

/// In text mode the wordmark leads the bar on either background and the header stays one row, so no
/// screen loses body height.
#[test]
fn text_mode_puts_the_wordmark_at_the_start_of_the_bar() {
    for bg in [Background::Dark, Background::Light] {
        let (mut app, area, _d) = app(Brand::text());
        let scr = screen(&mut app, 80, 10);
        let header = scr.lines().next().unwrap();
        assert!(
            header.starts_with(" ▄▀▄ re:SES  Inbox s3://mail/inbound/"),
            "{bg:?}: {header:?}"
        );
        // One row of header, as before, so every screen keeps its body height.
        assert_eq!(area.get(), Rect::new(0, 1, 80, 8), "{bg:?}");
    }
}

/// I check the text wordmark draws the three cubes as a pyramid of squares (`▄▀▄`: one on top, two
/// below) in the logo's blues, picks out the colon in logo blue, and bolds only SES, like the
/// image does.
#[test]
fn text_mode_styles_the_wordmark_like_the_logo() {
    let (mut app, _area, _d) = app(Brand::text());
    let buf = buffer(&mut app, 80, 10);
    let at = |x: u16| buf[(x, 0)].clone();
    // " ▄▀▄ re:SES": the squares at 1-3, "re" at 5-6, ":" at 7, "SES" at 8-10. A half-cell block
    // is about square, so the lower halves at 1 and 3 are the bottom pair and the upper half at 2
    // sits over the gap between them. Each square takes one of the logo's blues.
    let squares = [
        (1, "▄", Color::Rgb(0x2A, 0x8F, 0xE9)),
        (2, "▀", Color::Rgb(0x2E, 0xA8, 0xF2)),
        (3, "▄", Color::Rgb(0x1D, 0x78, 0xDE)),
    ];
    for (x, symbol, blue) in squares {
        assert_eq!(at(x).symbol(), symbol, "col {x}");
        assert_eq!(at(x).bg, blue, "col {x}");
        assert!(at(x).modifier.contains(Modifier::REVERSED), "col {x}");
    }
    assert_eq!(at(7).symbol(), ":");
    assert_eq!(at(7).bg, LOGO_BLUE);
    assert!(at(7).modifier.contains(Modifier::REVERSED));
    for x in [5, 6] {
        assert!(
            !at(x).modifier.contains(Modifier::BOLD),
            "re at {x} is bold"
        );
    }
    for x in [8, 9, 10] {
        assert!(
            at(x).modifier.contains(Modifier::BOLD),
            "SES at {x} is not bold"
        );
    }
}

/// In both modes the bar after the logo uses the terminal's own colours reversed and bold, so it
/// reads on any theme.
#[test]
fn the_bar_uses_the_terminals_own_colours_reversed_and_bold() {
    for brand in [
        Brand::text(),
        Brand::with_picker(picker(ProtocolType::Kitty), Background::Dark),
    ] {
        let (mut app, _area, _d) = app(brand);
        let buf = buffer(&mut app, 100, 20);
        let title_x = col_of(&buf, 0, "Inbox").expect("title on row 0");
        assert!(title_x < 30, "title at {title_x}");
        // Every cell of the bar from the title on: no fixed colours, reversed, bold.
        for x in title_x..100 {
            let c = &buf[(x, 0)];
            assert_eq!(c.fg, Color::Reset, "fg at {x}");
            assert_eq!(c.bg, Color::Reset, "bg at {x}");
            assert!(
                c.modifier.contains(Modifier::REVERSED | Modifier::BOLD),
                "style at {x}: {:?}",
                c.modifier
            );
        }
        // The old white-on-blue bar is gone from the whole header.
        for x in 0..100 {
            assert_ne!(buf[(x, 0)].bg, Color::Blue, "blue bar at {x}");
        }
    }
}

/// In image mode the logo gets two rows and fifteen columns to itself, with the title starting
/// clear of it.
#[test]
fn image_mode_reserves_the_logo_area_left_of_the_bar() {
    for bg in [Background::Dark, Background::Light] {
        let (mut app, area, _d) = app(Brand::with_picker(picker(ProtocolType::Kitty), bg));
        let buf = buffer(&mut app, 100, 20);
        // The kitty image goes out through the first cell of its area.
        assert!(
            buf[(0, 0)].symbol().starts_with("\x1b_G"),
            "{bg:?}: no kitty image at 0,0: {:?}",
            buf[(0, 0)].symbol().chars().take(12).collect::<String>()
        );
        // Two rows of header, with the title right of the image and no text wordmark.
        assert_eq!(area.get().y, IMAGE_ROWS, "{bg:?}");
        assert_eq!(area.get().height, 20 - IMAGE_ROWS - 1, "{bg:?}");
        let top = row(&buf, 0);
        assert!(
            !top.contains("re:SES") && !row(&buf, 1).contains("re:SES"),
            "{bg:?}: {top:?}"
        );
        let title_x = col_of(&buf, 0, "Inbox").expect("title");
        // 702x192 at two 16px rows is 117px: 15 columns of 8px, a gap column, then the bar's
        // leading space.
        assert!(
            title_x == 17,
            "{bg:?}: title at {title_x} overlaps the logo: {top:?}"
        );
    }
}

/// A dark background gets the light-text logo and a light one the dark-text logo, so it never
/// vanishes into the theme.
#[test]
fn image_mode_picks_the_variant_for_the_background() {
    let dark = Brand::with_picker(picker(ProtocolType::Kitty), Background::Dark);
    let light = Brand::with_picker(picker(ProtocolType::Sixel), Background::Light);
    assert_eq!(dark.variant(), Some(Variant::LightText));
    assert_eq!(light.variant(), Some(Variant::DarkText));
}

/// A terminal that can only do half blocks gets the text wordmark instead of a blurry picture.
#[test]
fn a_terminal_without_an_image_protocol_gets_the_text_wordmark() {
    let brand = Brand::with_picker(picker(ProtocolType::Halfblocks), Background::Dark);
    assert_eq!(brand.variant(), None);
    let (mut app, area, _d) = app(brand);
    assert!(screen(&mut app, 100, 20).starts_with(" ▄▀▄ re:SES"));
    assert_eq!(area.get().y, 1);
}

/// Too narrow or too short a terminal falls back to text, and even a tiny one shows the wordmark
/// whole.
#[test]
fn a_small_terminal_falls_back_to_the_text_wordmark() {
    for (w, h) in [(59u16, 20u16), (40, 20), (100, 11)] {
        let (mut app, area, _d) = app(Brand::with_picker(
            picker(ProtocolType::Kitty),
            Background::Dark,
        ));
        let buf = buffer(&mut app, w, h);
        assert!(
            !buf[(0, 0)].symbol().starts_with("\x1b_G"),
            "{w}x{h} drew the image"
        );
        assert!(
            row(&buf, 0).starts_with(" ▄▀▄ re:SES"),
            "{w}x{h}: {:?}",
            row(&buf, 0)
        );
        assert_eq!(area.get().y, 1, "{w}x{h}");
    }
    // Narrow enough that the title gets cut, the wordmark still shows whole.
    let (mut app, _area, _d) = app(Brand::text());
    let buf = buffer(&mut app, 12, 6);
    assert!(
        row(&buf, 0).starts_with(" ▄▀▄ re:SES "),
        "{:?}",
        row(&buf, 0)
    );
    assert_eq!(cols_of(&buf, 0, "▄"), [1, 3]);
    assert_eq!(cols_of(&buf, 0, "▀"), [2]);
}

/// I check COLORFGBG maps to dark or light, and that anything missing or garbled falls back to
/// dark.
#[test]
fn the_background_comes_from_colorfgbg() {
    assert_eq!(Background::from_colorfgbg(Some("15;0")), Background::Dark);
    assert_eq!(Background::from_colorfgbg(Some("0;15")), Background::Light);
    assert_eq!(
        Background::from_colorfgbg(Some("0;default;7")),
        Background::Light
    );
    assert_eq!(Background::from_colorfgbg(Some("7;8")), Background::Dark);
    assert_eq!(
        Background::from_colorfgbg(Some("garbage")),
        Background::Dark
    );
    assert_eq!(Background::from_colorfgbg(None), Background::Dark);
}

/// Both logo PNGs decode at 702x192 and come out fifteen columns wide, which the layout above
/// relies on.
#[test]
fn both_header_pngs_decode_at_the_rendered_size() {
    for png in [HEADER_PNG, HEADER_LIGHT_PNG] {
        let img = image::load_from_memory_with_format(png, image::ImageFormat::Png).unwrap();
        assert_eq!((img.width(), img.height()), (702, 192));
    }
    assert_eq!(
        columns_for(&image::load_from_memory(HEADER_PNG).unwrap(), (8, 16)),
        15
    );
}

/// The text header as it renders, printed so it can be eyeballed with --nocapture.
#[test]
fn text_header_frame() {
    let (mut app, _area, _d) = app(Brand::text());
    let scr = screen(&mut app, 80, 4);
    println!("{scr}");
    assert!(
        scr.lines()
            .next()
            .unwrap()
            .starts_with(" ▄▀▄ re:SES  Inbox")
    );
}

/// I build an `Env` from name and value pairs, leaving every other variable unset.
fn env(vars: &[(&str, &str)]) -> Env {
    let mut e = Env::default();
    for (k, v) in vars {
        let v = Some(v.to_string());
        match *k {
            "RESES_LOGO" => e.reses_logo = v,
            "TERM" => e.term = v,
            "TERM_PROGRAM" => e.term_program = v,
            "KITTY_WINDOW_ID" => e.kitty_window_id = v,
            "WEZTERM_EXECUTABLE" => e.wezterm_executable = v,
            "TMUX" => e.tmux = v,
            "MLTERM" => e.mlterm = v,
            other => panic!("unknown variable {other}"),
        }
    }
    e
}

/// Terminals I can name from their variables get the right image protocol straight away, without
/// querying the terminal.
#[test]
fn named_image_terminals_get_the_image_without_a_query() {
    for (vars, protocol) in [
        (&[("TERM_PROGRAM", "iTerm.app")][..], ProtocolType::Iterm2),
        (&[("TERM_PROGRAM", "WezTerm")][..], ProtocolType::Iterm2),
        (
            &[("WEZTERM_EXECUTABLE", "/usr/bin/wezterm-gui")][..],
            ProtocolType::Iterm2,
        ),
        (&[("TERM_PROGRAM", "ghostty")][..], ProtocolType::Kitty),
        (&[("TERM", "xterm-ghostty")][..], ProtocolType::Kitty),
        (&[("TERM", "xterm-kitty")][..], ProtocolType::Kitty),
        (&[("KITTY_WINDOW_ID", "3")][..], ProtocolType::Kitty),
        (&[("TERM", "foot")][..], ProtocolType::Sixel),
        (&[("TERM", "foot-extra")][..], ProtocolType::Sixel),
        (&[("MLTERM", "3.9.3")][..], ProtocolType::Sixel),
    ] {
        assert_eq!(plan(&env(vars)), Plan::Image(protocol), "{vars:?}");
    }
}

/// Every other terminal, tmux included, gets text and no query, since an unanswered query would
/// stall startup.
#[test]
fn every_other_terminal_gets_text_and_no_query() {
    for vars in [
        &[][..],
        &[("TERM", "xterm-256color")][..],
        &[
            ("TERM_PROGRAM", "Apple_Terminal"),
            ("TERM", "xterm-256color"),
        ][..],
        &[("TERM_PROGRAM", "vscode")][..],
        &[("TERM", "screen-256color")][..],
        &[("TERM", "linux")][..],
        // Inside tmux, image escapes need passthrough, so even kitty's own variables don't count.
        &[
            ("TMUX", "/tmp/tmux-1/default,1,0"),
            ("KITTY_WINDOW_ID", "3"),
        ][..],
        &[("TERM_PROGRAM", "tmux"), ("TERM", "tmux-256color")][..],
    ] {
        assert_eq!(plan(&env(vars)), Plan::Text, "{vars:?}");
    }
}

/// RESES_LOGO forces text or a query, and any other value is ignored rather than guessed at.
#[test]
fn reses_logo_forces_either_way() {
    assert_eq!(
        plan(&env(&[("RESES_LOGO", "text"), ("TERM", "xterm-kitty")])),
        Plan::Text
    );
    assert_eq!(
        plan(&env(&[
            ("RESES_LOGO", "image"),
            ("TERM_PROGRAM", "Apple_Terminal")
        ])),
        Plan::Query
    );
    // Anything else is ignored rather than guessed at.
    assert_eq!(plan(&env(&[("RESES_LOGO", "yes")])), Plan::Text);
    assert_eq!(
        plan(&env(&[("RESES_LOGO", "yes"), ("TERM", "xterm-kitty")])),
        Plan::Image(ProtocolType::Kitty)
    );
}

/// The cell size comes from the window's pixels and cells, or not at all when the terminal doesn't
/// report them.
#[test]
fn the_cell_size_comes_from_the_window_or_not_at_all() {
    assert_eq!(cell_size(800, 480, 100, 30), Some((8, 16)));
    // A terminal that reports no pixel size gets the text header rather than a guess.
    assert_eq!(cell_size(0, 0, 100, 30), None);
    assert_eq!(cell_size(800, 480, 0, 0), None);
    assert_eq!(cell_size(80, 16, 100, 30), None);
}
