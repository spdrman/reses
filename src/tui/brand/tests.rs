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
    fn title(&self) -> String {
        "Inbox s3://mail/inbound/".into()
    }
    fn render(&mut self, _frame: &mut Frame, area: Rect, _ctx: &Ctx) {
        self.area.set(area);
    }
    fn on_key(&mut self, _key: KeyEvent, _ctx: &mut Ctx) -> Transition {
        Transition::None
    }
    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![("q", "quit")]
    }
}

fn app(brand: Brand) -> (App, Rc<Cell<Rect>>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut ctx = testing::ctx(dir.path(), None);
    ctx.brand = brand;
    let area = Rc::new(Cell::new(Rect::default()));
    let app = App::with_view(ctx, Box::new(Probe { area: area.clone() }));
    (app, area, dir)
}

fn picker(protocol: ProtocolType) -> Picker {
    let mut p = Picker::from_fontsize((8, 16));
    p.set_protocol_type(protocol);
    p
}

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

#[test]
fn text_mode_puts_the_wordmark_at_the_start_of_the_bar() {
    for bg in [Background::Dark, Background::Light] {
        let (mut app, area, _d) = app(Brand::text(bg));
        let scr = screen(&mut app, 80, 10);
        let header = scr.lines().next().unwrap();
        assert!(
            header.starts_with(" ■ re:SES  Inbox s3://mail/inbound/"),
            "{bg:?}: {header:?}"
        );
        // One row of header, as before, so every screen keeps its body height.
        assert_eq!(area.get(), Rect::new(0, 1, 80, 8), "{bg:?}");
    }
}

#[test]
fn text_mode_styles_the_wordmark_like_the_logo() {
    let (mut app, _area, _d) = app(Brand::text(Background::Dark));
    let buf = buffer(&mut app, 80, 10);
    let at = |x: u16| buf[(x, 0)].clone();
    // " ■ re:SES": the cube at 1, "re" at 3-4, ":" at 5, "SES" at 6-8.
    assert_eq!(at(1).symbol(), "■");
    assert_eq!(at(5).symbol(), ":");
    for x in [1, 5] {
        assert_eq!(at(x).bg, LOGO_BLUE, "col {x}");
        assert!(at(x).modifier.contains(Modifier::REVERSED), "col {x}");
    }
    for x in [3, 4] {
        assert!(
            !at(x).modifier.contains(Modifier::BOLD),
            "re at {x} is bold"
        );
    }
    for x in [6, 7, 8] {
        assert!(
            at(x).modifier.contains(Modifier::BOLD),
            "SES at {x} is not bold"
        );
    }
}

#[test]
fn the_bar_uses_the_terminals_own_colours_reversed_and_bold() {
    for brand in [
        Brand::text(Background::Dark),
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

#[test]
fn image_mode_picks_the_variant_for_the_background() {
    let dark = Brand::with_picker(picker(ProtocolType::Kitty), Background::Dark);
    let light = Brand::with_picker(picker(ProtocolType::Sixel), Background::Light);
    assert_eq!(dark.variant(), Some(Variant::LightText));
    assert_eq!(light.variant(), Some(Variant::DarkText));
}

#[test]
fn a_terminal_without_an_image_protocol_gets_the_text_wordmark() {
    let brand = Brand::with_picker(picker(ProtocolType::Halfblocks), Background::Dark);
    assert_eq!(brand.variant(), None);
    let (mut app, area, _d) = app(brand);
    assert!(screen(&mut app, 100, 20).starts_with(" ■ re:SES"));
    assert_eq!(area.get().y, 1);
}

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
            row(&buf, 0).starts_with(" ■ re:SES"),
            "{w}x{h}: {:?}",
            row(&buf, 0)
        );
        assert_eq!(area.get().y, 1, "{w}x{h}");
    }
    // Narrow enough that the title gets cut, the wordmark still shows whole.
    let (mut app, _area, _d) = app(Brand::text(Background::Dark));
    let buf = buffer(&mut app, 12, 6);
    assert!(row(&buf, 0).starts_with(" ■ re:SES "), "{:?}", row(&buf, 0));
    assert_eq!(cols_of(&buf, 0, "■"), [1]);
}

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
    let (mut app, _area, _d) = app(Brand::text(Background::Dark));
    let scr = screen(&mut app, 80, 4);
    println!("{scr}");
    assert!(scr.lines().next().unwrap().starts_with(" ■ re:SES  Inbox"));
}

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

#[test]
fn the_cell_size_comes_from_the_window_or_not_at_all() {
    assert_eq!(cell_size(800, 480, 100, 30), Some((8, 16)));
    // A terminal that reports no pixel size gets the text header rather than a guess.
    assert_eq!(cell_size(0, 0, 100, 30), None);
    assert_eq!(cell_size(800, 480, 0, 0), None);
    assert_eq!(cell_size(80, 16, 100, 30), None);
}
