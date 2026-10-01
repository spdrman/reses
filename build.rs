use std::env;
use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const LOGO: &str = include_str!("assets/brand/reSES-logo.svg");
const WIDTH: u32 = 702;
const HEIGHT: u32 = 192;

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=assets/brand/reSES-logo.svg");

    let defs = svg_defs(LOGO)?;
    let out = PathBuf::from(
        env::var_os("OUT_DIR")
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Cargo did not set OUT_DIR"))?,
    );

    render_header(defs, "#141b2b", &out.join("reses-header.png"))?;
    render_header(defs, "#f7f9fc", &out.join("reses-header-light.png"))?;
    Ok(())
}

fn svg_defs(source: &str) -> Result<&str, io::Error> {
    let start = source
        .find("<defs")
        .and_then(|at| source[at..].find('>').map(|end| at + end + 1))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "logo has no <defs>"))?;
    let end = source[start..]
        .find("</defs>")
        .map(|at| start + at)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "logo has no </defs>"))?;
    Ok(&source[start..end])
}

fn render_header(defs: &str, wordmark: &str, path: &Path) -> Result<(), Box<dyn Error>> {
    let source = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" viewBox="0 0 {WIDTH} {HEIGHT}" width="{WIDTH}" height="{HEIGHT}">
<defs>{defs}</defs>
<svg x="0" y="0" width="205" height="192" viewBox="182 5 556 520"><use href="#cubes" xlink:href="#cubes"/></svg>
<svg x="241" y="33.75" width="461" height="124.5" viewBox="20 525 870 235" color="{wordmark}"><use href="#wordmark" xlink:href="#wordmark"/></svg>
</svg>"##
    );
    let tree = resvg::usvg::Tree::from_str(&source, &resvg::usvg::Options::default())?;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(WIDTH, HEIGHT).ok_or_else(|| {
        io::Error::new(io::ErrorKind::OutOfMemory, "could not allocate logo pixmap")
    })?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::identity(),
        &mut pixmap.as_mut(),
    );
    fs::write(path, pixmap.encode_png()?)?;
    Ok(())
}
