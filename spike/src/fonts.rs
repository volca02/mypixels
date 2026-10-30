//! Font fallback chains.
//!
//! cosmic-text's Unix fallback list is a hardcoded `&'static [&'static str]`
//! (Noto Sans, DejaVu, FreeMono, Noto Color Emoji, ...) and it never consults
//! fontconfig. A starship/powerline prompt draws Private Use Area codepoints --
//! U+E0B0..U+E0B6 separators, U+E70E branch, U+F300.. OS logos -- and none of
//! those families carry PUA glyphs, so we draw tofu exactly where VTE/Pango
//! draw icons (fontconfig resolves those codepoints, e.g. to
//! "Cascadia Code NF").
//!
//! So we supply our own `Fallback`: PUA-capable icon families first, then the
//! platform list so CJK and emoji keep working. Variants of the configured
//! family come first because they share its advance width -- an icon then still
//! occupies exactly one cell instead of drifting the grid.
//!
//! fontdb 0.23 exposes no charset API, so coverage is inferred from family
//! names. That is a heuristic, and deliberately a conservative one: it only
//! ever *adds* families, and fallback is consulted only for glyphs the
//! configured family is missing.

use glyphon::cosmic_text::{Fallback, FontSystem, PlatformFallback};
use glyphon::fontdb::Database;
use unicode_script::Script;

/// 'static so we can borrow the platform fallback list out of it.
static PLATFORM: PlatformFallback = PlatformFallback;

/// A font's own vertical metrics, for deriving line height at any pixel size.
#[derive(Clone, Copy, Debug)]
pub struct LineMetrics {
    pub upem: f32,
    pub ascent: f32,
    /// Below the baseline; skrifa's convention makes this negative.
    pub descent: f32,
    pub leading: f32,
}

impl LineMetrics {
    /// Fallback when a family cannot be found: 1.25 em, a typical terminal.
    pub const DEFAULT: LineMetrics = LineMetrics {
        upem: 1000.0,
        ascent: 1000.0,
        descent: -250.0,
        leading: 0.0,
    };

    /// Pixel line height at `px`, rounding ascent, descent and leading
    /// separately the way Pango does. Two things bit us here: skrifa reports
    /// descent *negative* (so `ascent + descent` is not a height at all), and
    /// some fonts carry leading that must be counted -- Terminus 12 needs it to
    /// reach the 17px the original measures, while Cascadia Code has none.
    pub fn height_at(&self, px: f32) -> f32 {
        if self.upem <= 0.0 {
            return (px * 1.25).max(1.0);
        }
        let scale = px / self.upem;
        let a = (self.ascent * scale).round();
        let d = (self.descent.abs() * scale).round();
        let l = (self.leading * scale).round();
        (a + d + l).max(1.0)
    }
}

/// Read a family's vertical metrics out of the loaded database.
pub fn line_metrics(fs: &mut FontSystem, family: &str) -> Option<LineMetrics> {
    let id = {
        let db = fs.db_mut();
        db.faces()
            .find(|face| {
                face.families
                    .iter()
                    .any(|(name, _lang)| name == family)
            })
            .map(|face| face.id)?
    };
    let font = fs.get_font(id, glyphon::fontdb::Weight::NORMAL)?;
    let m = font.metrics();
    if m.units_per_em == 0 {
        return None;
    }
    let lm = LineMetrics {
        upem: m.units_per_em as f32,
        ascent: m.ascent,
        descent: m.descent,
        leading: m.leading,
    };
    log::debug!(
        "line_metrics({family}): upem={} ascent={} descent={} leading={}",
        m.units_per_em, m.ascent, m.descent, m.leading
    );
    Some(lm)
}

/// Every family name known to the database, deduped, in database order.
fn installed_families(db: &Database) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for face in db.faces() {
        for (name, _lang) in &face.families {
            if !out.iter().any(|n| n == name) {
                out.push(name.clone());
            }
        }
    }
    out
}

/// Icon-font families available for `primary`, best match first.
pub fn icon_families(primary: &str, db: &Database) -> Vec<String> {
    let fams = installed_families(db);
    let mut chain: Vec<String> = Vec::new();

    let mut push = |name: &str, chain: &mut Vec<String>| {
        let name = name.to_string();
        if name != primary && !chain.contains(&name) && fams.iter().any(|f| f == &name) {
            chain.push(name);
        }
    };

    // 1. Same design, icon-patched: identical advance width, so icons land on
    //    the cell grid. ("PL" = powerline+ligatures for Cascadia, "NF" = Nerd.)
    for suffix in [" NF", " PL", " Nerd Font Mono", " Nerd Font"] {
        push(&(primary.to_owned() + suffix), &mut chain);
    }
    // 2. Any monospace Nerd Font: still one cell per icon.
    for f in &fams {
        if f.contains("Nerd Font Mono") {
            push(f, &mut chain);
        }
    }
    // 3. Icons-only companion fonts.
    push("Symbols Nerd Font Mono", &mut chain);
    push("Symbols Nerd Font", &mut chain);
    // 4. Last resort: proportional Nerd Fonts. Their advance will not match
    //    the cell width, but a slightly wide icon beats a tofu box.
    for f in &fams {
        if f.contains("Nerd Font") && !f.contains("Propo") {
            push(f, &mut chain);
        }
    }
    chain.truncate(8);
    chain
}

pub struct TerminalFallback {
    common: &'static [&'static str],
}

impl Fallback for TerminalFallback {
    fn common_fallback(&self) -> &[&'static str] {
        self.common
    }

    fn forbidden_fallback(&self) -> &[&'static str] {
        &[]
    }

    fn script_fallback(&self, script: Script, locale: &str) -> &[&'static str] {
        PLATFORM.script_fallback(script, locale)
    }
}

/// A `FontSystem` whose fallback reaches icon glyphs for the given primary
/// families (terminal first, then tablist). Also returns the chain it chose,
/// for logging.
///
/// Two levers, because cosmic-text 0.19 has two half-implementations:
///
/// * The real one. When shaping yields glyph_id 0 for a `Family::Name` run,
///   `shape.rs` builds fallback `Attrs` with `Family::Monospace` (for a
///   monospaced primary) and takes **only `fb_iter.next()`** -- a single font.
///   cosmic-text seeds the generic monospace family with "Noto Sans Mono", so
///   that one shot always misses PUA and the box stays. Pointing the generic
///   monospace family at an icon-patched monospace font is what actually fixes
///   it. (Consequence: the *second* candidate is never consulted, so CJK/emoji
///   fallback through this path is limited to whatever that one family covers.
///   That is already true today -- it is Noto Sans Mono, which covers neither.)
/// * Our `Fallback` impl, which `FontFallbackIter` does consult whenever it
///   iterates. Kept so iteration-based callers get the same chain.
pub fn build_font_system(primary_families: &[&str]) -> (FontSystem, Vec<String>) {
    // Reuse cosmic-text's own database + locale so system font dirs, fontconfig
    // settings and the locale stay exactly what `FontSystem::new()` would use.
    let seed = FontSystem::new();
    let (locale, mut db) = seed.into_locale_and_db();

    let mut chain: Vec<String> = Vec::new();
    for primary in primary_families {
        for fam in icon_families(primary, &db) {
            if !chain.contains(&fam) {
                chain.push(fam);
            }
        }
    }
    // The per-family chain is already capped; bound the union too, keeping
    // order so the terminal's own variants survive.
    chain.truncate(8);

    if let Some(first) = chain.first() {
        db.set_monospace_family(first);
    }

    let mut names: Vec<&'static str> = chain
        .iter()
        .map(|s| -> &'static str { Box::leak(s.clone().into_boxed_str()) })
        .collect();
    names.extend(PLATFORM.common_fallback().iter().copied());
    let names: &'static [&'static str] = Box::leak(names.into_boxed_slice());

    let fs = FontSystem::new_with_locale_and_db_and_fallback(
        locale,
        db,
        TerminalFallback { common: names },
    );
    (fs, chain)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db_with(names: &[&str]) -> Database {
        // Faces cannot be synthesised without font files, so `installed_families`
        // is exercised against the real system database instead.
        let _ = names;
        Database::new()
    }

    #[test]
    fn line_height_counts_negative_descent_as_height() {
        // skrifa gives descent negative; the height must still grow with it.
        let lm = LineMetrics { upem: 1000.0, ascent: 800.0, descent: -200.0, leading: 0.0 };
        assert_eq!(lm.height_at(10.0), 10.0);
    }

    #[test]
    fn line_height_counts_leading() {
        // Terminus (TTF) 12pt: upem 1000, ascent 833, descent -167, leading 90
        // -> 13 + 3 + 1 = 17px, which is what the original's tab strip measures
        // (21px total with 2px margins above and below).
        let lm = LineMetrics { upem: 1000.0, ascent: 833.0, descent: -167.0, leading: 90.0 };
        assert_eq!(lm.height_at(16.0), 17.0);
    }

    #[test]
    fn degenerate_upem_does_not_panic() {
        let lm = LineMetrics { upem: 0.0, ascent: 0.0, descent: 0.0, leading: 0.0 };
        assert_eq!(lm.height_at(12.0), 15.0);
        assert!(lm.height_at(0.0) >= 1.0);
    }

    #[test]
    fn empty_database_yields_empty_chain() {
        // No families installed at all -> nothing to fall back to, no panics.
        assert!(icon_families("Cascadia Code", &db_with(&[])).is_empty());
    }

    #[test]
    fn never_lists_the_primary_family_itself() {
        let (fs, chain) = build_font_system(&["Cascadia Code"]);
        drop(fs);
        assert!(!chain.iter().any(|f| f == "Cascadia Code"));
    }

    #[test]
    fn prefers_same_family_icon_variant() {
        // Only meaningful on a machine that actually has the icon variant.
        let db = {
            let seed = FontSystem::new();
            let (_locale, db) = seed.into_locale_and_db();
            db
        };
        let chain = icon_families("Cascadia Code", &db);
        if let Some(pos_nf) = chain.iter().position(|f| f == "Cascadia Code NF") {
            // an icon variant of the same design must outrank a foreign one
            if let Some(pos_other) = chain.iter().position(|f| f.starts_with("JetBrains")) {
                assert!(pos_nf < pos_other);
            }
        }
    }

    #[test]
    fn chain_is_bounded_and_deduped() {
        let (fs, chain) = build_font_system(&["Cascadia Code", "Terminus (TTF)"]);
        drop(fs);
        assert!(chain.len() <= 8, "chain too long: {}", chain.len());
        for i in 0..chain.len() {
            assert!(!chain[i + 1..].contains(&chain[i]), "duplicate: {}", chain[i]);
        }
    }
}
