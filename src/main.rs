//! fonts — TUI font picker with live previews.
//!
//! Enumerates every installed font via `glyph --list`, shows a live preview
//! of the highlighted family (rendered by `glyph --preview`, drawn with glow),
//! and returns the chosen family + point size. Mirrors prism: launch with
//! `--out=FILE` and the caller (scribe, grid, …) reads the result.
//!
//! CLI:
//!   fonts                 standalone; prints the picked font on Enter
//!   fonts --out=FILE      write the result to FILE instead of stdout
//!   fonts --size 14       start at 14 pt
//!   fonts "DejaVu Serif"  preselect a family
//!
//! Navigation: ↑/↓ + PgUp/PgDn move · Shift+↑/↓ change size · type to filter ·
//! Backspace deletes · Enter picks · Esc/Ctrl-C cancels.
//!
//! Output on Enter (to --out FILE or stdout):
//!   family=<name>
//!   size=<pt>
//!   path=<file>

use std::io::Write;
use std::process::Command;

use crust::{Crust, Input};

struct Family {
    name: String,
    path: String,
}

struct App {
    fams: Vec<Family>,
    filtered: Vec<usize>,
    cursor: usize, // index into filtered
    top: usize,    // first visible filtered row
    filter: String,
    size: u32, // document point size
}

// ── helpers ───────────────────────────────────────────────────────────

fn move_to(row: u16, col: u16) -> String {
    format!("\x1b[{};{}H", row, col)
}

/// Truncate to at most `w` chars (char-based; font names are ~ASCII).
fn trunc(s: &str, w: usize) -> String {
    if s.chars().count() <= w {
        s.to_string()
    } else {
        s.chars().take(w.saturating_sub(1)).collect::<String>() + "…"
    }
}

/// True if the font carries a `glyf` table (TrueType outlines, which is what
/// glyph rasterizes). CFF/OpenType (.otf, PostScript outlines), bitmap, and
/// colour fonts lack it and glyph can't preview them — so they're dropped from
/// the list. Just reads the SFNT table directory (a few hundred bytes), no
/// subprocess.
fn has_glyf_outlines(path: &str) -> bool {
    use std::io::Read;
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut head = [0u8; 12];
    if f.read_exact(&mut head).is_err() {
        return false;
    }
    let num = u16::from_be_bytes([head[4], head[5]]) as usize;
    if num == 0 || num > 4096 {
        return false;
    }
    let mut dir = vec![0u8; num * 16];
    if f.read_exact(&mut dir).is_err() {
        return false;
    }
    dir.chunks_exact(16).any(|r| &r[0..4] == b"glyf")
}

fn load_families() -> Vec<Family> {
    let home = std::env::var("HOME").unwrap_or_default();
    let cmd = format!(
        "find /usr/share/fonts {h}/.fonts {h}/.local/share/fonts -type f \
         \\( -iname '*.ttf' -o -iname '*.otf' \\) 2>/dev/null | glyph --list 2>/dev/null",
        h = home
    );
    let out = match Command::new("sh").arg("-c").arg(&cmd).output() {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    use std::collections::HashMap;
    // family -> (path, have_regular): keep the Regular face's path as the
    // representative so previews aren't rendered in Bold/Italic by accident.
    let mut map: HashMap<String, (String, bool)> = HashMap::new();
    for line in text.lines() {
        let mut it = line.split('\t');
        let path = it.next().unwrap_or("");
        let family = it.next().unwrap_or("");
        let style = it.next().unwrap_or("");
        if path.is_empty() || family.is_empty() {
            continue;
        }
        // Drop faces glyph can't rasterize (CFF/OTF, bitmap, colour) so the
        // list only holds fonts that actually preview.
        if !has_glyf_outlines(path) {
            continue;
        }
        let is_reg = style.is_empty()
            || style.eq_ignore_ascii_case("Regular")
            || style.eq_ignore_ascii_case("Book");
        match map.get(family) {
            Some((_, true)) => {} // already hold a Regular face
            _ => {
                map.insert(family.to_string(), (path.to_string(), is_reg));
            }
        }
    }
    let mut v: Vec<Family> = map
        .into_iter()
        .map(|(name, (path, _))| Family { name, path })
        .collect();
    v.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    v
}

fn refilter(app: &mut App) {
    let f = app.filter.to_lowercase();
    app.filtered = app
        .fams
        .iter()
        .enumerate()
        .filter(|(_, fam)| f.is_empty() || fam.name.to_lowercase().contains(&f))
        .map(|(i, _)| i)
        .collect();
    if app.cursor >= app.filtered.len() {
        app.cursor = app.filtered.len().saturating_sub(1);
    }
    app.top = 0;
}

/// Render `sample` in `font_path` at `px` pixels via glyph; decode the P5 PGM
/// and re-encode it as PNG at `out_png` for glow. Returns true on success.
fn make_preview(font_path: &str, px: u32, sample: &str, out_png: &str) -> bool {
    let out = Command::new("glyph")
        .arg("--preview")
        .arg(font_path)
        .arg(px.to_string())
        .arg(sample)
        .output();
    let pgm = match out {
        Ok(o) if o.status.success() && o.stdout.len() > 16 => o.stdout,
        _ => return false,
    };
    match image::load_from_memory_with_format(&pgm, image::ImageFormat::Pnm) {
        Ok(img) => img.save_with_format(out_png, image::ImageFormat::Png).is_ok(),
        Err(_) => false,
    }
}

// ── render ────────────────────────────────────────────────────────────

const LIST_W: u16 = 34;

/// Redraw the text chrome only (header, list column, footer). Deliberately
/// avoids clear-to-EOL inside the list rows so the preview image to the right
/// is never wiped.
fn render_text(app: &App, cols: u16, rows: u16, list_h: usize) {
    let mut s = String::new();

    // Header (row 1, full width — above the image, safe to clear).
    let hdr = format!(
        " fonts  ·  {} families  ·  {}pt{}",
        app.fams.len(),
        app.size,
        if app.filter.is_empty() {
            String::new()
        } else {
            format!("  ·  /{}", app.filter)
        }
    );
    let hdr = trunc(&hdr, cols as usize);
    s.push_str(&move_to(1, 1));
    s.push_str(&format!(
        "\x1b[48;2;30;30;42m\x1b[38;2;235;235;245m{:<w$}\x1b[0m",
        hdr,
        w = cols as usize
    ));

    // List rows 2..=rows-1.
    for row in 0..list_h {
        let y = 2 + row as u16;
        s.push_str(&move_to(y, 1));
        let idx = app.top + row;
        if let Some(&fi) = app.filtered.get(idx) {
            let name = trunc(&app.fams[fi].name, LIST_W as usize - 2);
            let body = format!("{}{}", if idx == app.cursor { "▸ " } else { "  " }, name);
            let pad = (LIST_W as usize).saturating_sub(body.chars().count());
            if idx == app.cursor {
                s.push_str(&format!("\x1b[1;38;2;120;200;255m{}\x1b[0m", body));
            } else {
                s.push_str(&format!("\x1b[38;2;200;200;210m{}\x1b[0m", body));
            }
            s.push_str(&" ".repeat(pad));
        } else {
            s.push_str(&" ".repeat(LIST_W as usize));
        }
    }

    // Footer (last row — below the image).
    let foot = " ↑↓ move · ⇧↑↓ size · type filter · Enter pick · Esc cancel";
    let foot = trunc(foot, cols as usize);
    s.push_str(&move_to(rows, 1));
    s.push_str(&format!(
        "\x1b[48;2;30;30;42m\x1b[38;2;180;180;195m{:<w$}\x1b[0m",
        foot,
        w = cols as usize
    ));

    print!("{}", s);
    let _ = std::io::stdout().flush();
}

fn main() {
    // ── argv ──────────────────────────────────────────────────────────
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut out_file: Option<String> = None;
    let mut size: u32 = 12;
    let mut preselect: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if let Some(rest) = a.strip_prefix("--out=") {
            out_file = Some(rest.to_string());
        } else if a == "--out" && i + 1 < args.len() {
            out_file = Some(args[i + 1].clone());
            i += 1;
        } else if let Some(rest) = a.strip_prefix("--size=") {
            size = rest.parse().unwrap_or(size);
        } else if a == "--size" && i + 1 < args.len() {
            size = args[i + 1].parse().unwrap_or(size);
            i += 1;
        } else if a == "-h" || a == "--help" {
            println!("fonts — TUI font picker with live previews");
            println!();
            println!("Usage: fonts [--out=FILE] [--size N] [FAMILY]");
            println!("  ↑/↓ PgUp/PgDn move · Shift+↑/↓ size · type to filter ·");
            println!("  Enter pick · Esc cancel");
            println!("Output: family=… / size=… / path=… on Enter.");
            return;
        } else if !a.starts_with('-') {
            preselect = Some(a.clone());
        }
        i += 1;
    }

    let fams = load_families();
    if fams.is_empty() {
        eprintln!("fonts: no fonts found (is `glyph` on PATH?)");
        std::process::exit(2);
    }

    let mut app = App {
        fams,
        filtered: Vec::new(),
        cursor: 0,
        top: 0,
        filter: String::new(),
        size: size.clamp(6, 96),
    };
    refilter(&mut app);
    if let Some(p) = preselect {
        let pl = p.to_lowercase();
        if let Some(pos) = app.filtered.iter().position(|&i| app.fams[i].name.to_lowercase() == pl) {
            app.cursor = pos;
        }
    }

    Crust::init();
    let (mut cols, mut rows) = Crust::terminal_size();
    if cols < 80 { cols = 100; }
    if rows < 24 { rows = 30; }
    Crust::clear_screen();

    let preview_png = format!("/tmp/fonts-preview-{}.png", std::process::id());
    let mut disp = glow::Display::new();
    let mut shown: Option<usize> = None; // family idx currently previewed
    let mut selected = false;

    loop {
        let list_h = rows.saturating_sub(2) as usize;
        // Keep cursor in the visible window.
        if app.cursor < app.top {
            app.top = app.cursor;
        } else if app.cursor >= app.top + list_h {
            app.top = app.cursor + 1 - list_h;
        }

        render_text(&app, cols, rows, list_h);

        // Preview (only when the highlighted family changed).
        let cur_fi = app.filtered.get(app.cursor).copied();
        if disp.supported() && cur_fi != shown {
            disp.forget_path(&preview_png);
            if let Some(fi) = cur_fi {
                let px = 40u32;
                let preview_x = LIST_W + 3;
                let preview_w = cols.saturating_sub(preview_x);
                let preview_h = rows.saturating_sub(3);
                if make_preview(&app.fams[fi].path, px, &app.fams[fi].name, &preview_png) {
                    disp.show(&preview_png, preview_x, 2, preview_w, preview_h);
                }
            }
            shown = cur_fi;
        }

        let key = match Input::getchr(None) {
            Some(k) => k,
            None => continue,
        };
        match key.as_str() {
            "ESC" | "C-C" => break,
            "ENTER" => {
                if cur_fi.is_some() {
                    selected = true;
                    break;
                }
            }
            "DOWN" => {
                if app.cursor + 1 < app.filtered.len() {
                    app.cursor += 1;
                }
            }
            "UP" => {
                app.cursor = app.cursor.saturating_sub(1);
            }
            "PgDOWN" => {
                app.cursor = (app.cursor + list_h).min(app.filtered.len().saturating_sub(1));
            }
            "PgUP" => {
                app.cursor = app.cursor.saturating_sub(list_h);
            }
            "S-UP" => {
                app.size = (app.size + 1).min(96);
            }
            "S-DOWN" => {
                app.size = app.size.saturating_sub(1).max(6);
            }
            "BACK" => {
                app.filter.pop();
                refilter(&mut app);
                shown = None;
                Crust::clear_screen();
            }
            "C-U" => {
                app.filter.clear();
                refilter(&mut app);
                shown = None;
                Crust::clear_screen();
            }
            "RESIZE" => {
                let (c, r) = Crust::terminal_size();
                cols = if c < 80 { 100 } else { c };
                rows = if r < 24 { 30 } else { r };
                shown = None;
                disp.forget_path(&preview_png);
                Crust::clear_screen();
            }
            s if s.chars().count() == 1 => {
                let ch = s.chars().next().unwrap();
                if !ch.is_control() {
                    app.filter.push(ch);
                    refilter(&mut app);
                    shown = None;
                    Crust::clear_screen();
                }
            }
            _ => {}
        }
    }

    disp.forget_path(&preview_png);
    let _ = std::fs::remove_file(&preview_png);
    Crust::cleanup();
    print!("\x1b[?25h");
    let _ = std::io::stdout().flush();

    if selected {
        if let Some(&fi) = app.filtered.get(app.cursor) {
            let result = format!(
                "family={}\nsize={}\npath={}\n",
                app.fams[fi].name, app.size, app.fams[fi].path
            );
            match out_file {
                Some(p) => {
                    let _ = std::fs::write(p, result);
                }
                None => print!("{}", result),
            }
            return;
        }
    }
    // Cancelled: nothing written, non-zero exit so callers can tell.
    std::process::exit(1);
}
