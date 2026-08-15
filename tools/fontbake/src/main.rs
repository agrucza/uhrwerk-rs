//! fontbake - bake Inter TTFs into the UI's alpha glyph atlases.
//!
//! For each font role this rasterizes every charset glyph with
//! `fontdue`, packs the coverage down to 4-bit alpha (two pixels per
//! byte, rows padded to whole bytes), and emits:
//!
//! * `app-core/src/ui/fonts/gen/<role>.alpha` - the packed bitmap blob
//! * `app-core/src/ui/fonts/gen/<role>.rs`    - glyph metric tables
//!
//! Sizes are specified as a target *ink height* of a reference glyph
//! ('H' for text roles, '0' for digit roles) instead of an em size, so
//! each role visually matches the u8g2 font it replaced regardless of
//! the font's internal metrics. The generated files are committed;
//! firmware builds never run this tool.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// One font role to bake.
struct Role {
    name: &'static str,
    ttf: &'static str,
    /// Reference glyph whose ink height is driven to `target_px`.
    ref_glyph: char,
    target_px: u32,
    charset: Charset,
}

enum Charset {
    /// Printable ASCII + Latin-1 (matches the old u8g2 `_te` range).
    Text,
    /// Digits plus the punctuation the numeric readouts use.
    Digits,
}

impl Charset {
    fn chars(&self) -> Vec<char> {
        match self {
            Charset::Text => (0x20u32..=0x7E)
                .chain(0xA0..=0xFF)
                .filter_map(char::from_u32)
                .collect(),
            Charset::Digits => "0123456789:.-+% ".chars().collect(),
        }
    }
}

const ROLES: &[Role] = &[
    Role { name: "caption",  ttf: "Inter-Regular.ttf",          ref_glyph: 'H', target_px: 10, charset: Charset::Text },
    Role { name: "body",     ttf: "Inter-Regular.ttf",          ref_glyph: 'H', target_px: 14, charset: Charset::Text },
    Role { name: "headline", ttf: "Inter-Regular.ttf",          ref_glyph: 'H', target_px: 18, charset: Charset::Text },
    Role { name: "label",    ttf: "Inter-SemiBold.ttf",         ref_glyph: 'H', target_px: 10, charset: Charset::Text },
    Role { name: "value",    ttf: "Inter-SemiBold.ttf",         ref_glyph: 'H', target_px: 24, charset: Charset::Text },
    Role { name: "hero",     ttf: "InterDisplay-SemiBold.ttf",  ref_glyph: '0', target_px: 49, charset: Charset::Digits },
    Role { name: "mega",     ttf: "InterDisplay-SemiBold.ttf",  ref_glyph: '0', target_px: 78, charset: Charset::Digits },
];

fn main() {
    // Workspace root = two levels up from this crate's manifest.
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap()
        .parent().unwrap()
        .to_path_buf();
    let font_dir = root.join("assets/fonts");
    let out_dir = root.join("app-core/src/ui/fonts/gen");

    for role in ROLES {
        bake(role, &font_dir, &out_dir);
    }
    println!("done - rebuild app-core to pick up the new atlases");
}

fn bake(role: &Role, font_dir: &Path, out_dir: &Path) {
    let ttf = std::fs::read(font_dir.join(role.ttf))
        .unwrap_or_else(|e| panic!("read {}: {e}", role.ttf));
    let font = fontdue::Font::from_bytes(ttf.as_slice(), fontdue::FontSettings::default())
        .unwrap_or_else(|e| panic!("parse {}: {e}", role.ttf));

    // Find the em size that renders the reference glyph at the target
    // ink height. Ink height scales ~linearly with em size, so a few
    // proportional corrections converge.
    let mut px = role.target_px as f32;
    for _ in 0..8 {
        let (m, _) = font.rasterize(role.ref_glyph, px);
        if m.height == role.target_px as usize {
            break;
        }
        if m.height == 0 {
            panic!("{}: reference glyph '{}' has no ink", role.name, role.ref_glyph);
        }
        px *= role.target_px as f32 / m.height as f32;
    }
    let (m, _) = font.rasterize(role.ref_glyph, px);
    assert_eq!(
        m.height, role.target_px as usize,
        "{}: cap-height search did not converge (got {} px)", role.name, m.height,
    );

    // Sorted + deduped: the runtime does a binary search on `cp`.
    let mut chars = role.charset.chars();
    chars.sort();
    chars.dedup();

    // ascent = the REFERENCE glyph's ink above the baseline (cap
    // height for text roles, digit height for digit roles) - u8g2's
    // capital-letter Top-anchor semantics. Accented capitals overhang
    // ABOVE top_y, exactly as the u8g2 fonts did (screens pad their
    // dirty rects for that). Both alternatives shift every
    // Top-anchored string visibly down: the em line metrics wildly
    // (99 px ascent for 78 px digits), the charset's tallest-ink
    // extent by the accent overhang (verified on hardware).
    // descent = the charset's real ink extent below the baseline;
    // only line-box height consumers use it.
    let ascent = (m.height as i32 + m.ymin) as i16;
    let mut descent: i16 = 0;
    let mut alpha: Vec<u8> = Vec::new();
    let mut glyph_rows = String::new();
    let mut n_glyphs = 0usize;

    for &ch in &chars {
        // Chars the font has no mapping for fall to the notdef glyph;
        // skip those so the runtime's missing-glyph path handles them.
        if ch != ' ' && font.lookup_glyph_index(ch) == 0 {
            continue;
        }
        let (met, cov) = font.rasterize(ch, px);
        let w = met.width;
        let h = met.height;
        assert!(w <= 255 && h <= 255, "{}: '{}' bitmap {}x{}", role.name, ch, w, h);

        let off = alpha.len();
        let row_bytes = (w + 1) / 2;
        for row in 0..h {
            for bx in 0..row_bytes {
                let c0 = cov[row * w + bx * 2] as u16;
                let c1 = if bx * 2 + 1 < w { cov[row * w + bx * 2 + 1] as u16 } else { 0 };
                // 0..=255 -> 0..=15, round to nearest step (17 = 255/15).
                let n0 = ((c0 + 8) / 17).min(15) as u8;
                let n1 = ((c1 + 8) / 17).min(15) as u8;
                alpha.push((n0 << 4) | n1);
            }
        }

        let adv = met.advance_width.round() as i32;
        assert!((0..=255).contains(&adv), "{}: '{}' advance {}", role.name, ch, adv);
        // Bitmap top relative to the baseline in y-down screen coords.
        let top = -(met.ymin + h as i32);
        let left = met.xmin;
        if h > 0 {
            descent = descent.max((top + h as i32) as i16);
        }
        assert!(
            (-128..=127).contains(&top) && (-128..=127).contains(&left),
            "{}: '{}' bearings out of i8 range (left={left}, top={top})", role.name, ch,
        );

        writeln!(
            glyph_rows,
            "    Glyph {{ cp: {}, adv: {}, w: {}, h: {}, left: {}, top: {}, off: {} }},",
            ch as u32, adv, w, h, left, top, off,
        ).unwrap();
        n_glyphs += 1;
    }

    // No kerning: Inter keeps its kerning in GPOS, which fontdue does
    // not read (legacy `kern` table only, and Inter ships none), so a
    // kern table here could never be non-empty.

    std::fs::write(out_dir.join(format!("{}.alpha", role.name)), &alpha).unwrap();
    let src = format!(
        "\
// GENERATED by tools/fontbake - do not edit by hand.
// Regenerate with `cargo run -p fontbake` (needs assets/fonts/).
// {} @ ref '{}' = {} px ink height (em {:.2} px).
use super::super::{{FontData, Glyph}};

pub static FONT: FontData = FontData {{
    ascent: {},
    descent: {},
    glyphs: &GLYPHS,
    alpha: include_bytes!(\"{}.alpha\"),
}};

static GLYPHS: [Glyph; {}] = [
{}];
",
        role.ttf, role.ref_glyph, role.target_px, px,
        ascent, descent, role.name,
        n_glyphs, glyph_rows,
    );
    std::fs::write(out_dir.join(format!("{}.rs", role.name)), src).unwrap();

    println!(
        "{:<9} {:<26} em {:>6.2}  ascent {:>3}  descent {:>2}  glyphs {:>3}  alpha {:>6} B",
        role.name, role.ttf, px, ascent, descent, n_glyphs, alpha.len(),
    );
}
