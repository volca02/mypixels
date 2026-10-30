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
//! fontdb 0.23 exposes no charset API, so `Coverage` below queries each face's
//! `cmap` through skrifa to find out what is actually installed. That is the
//! only trustworthy route: family-name patterns were tried first and are wrong
//! in both directions (an icon font under an unusual name is missed, a font
//! whose name merely looks right is trusted).

use glyphon::cosmic_text::{
    skrifa, skrifa::prelude::*, Attrs, Fallback, Family, FontSystem, PlatformFallback, Style,
    Weight,
};
use glyphon::fontdb::{Database, ID, Style as DbStyle};
use std::collections::HashMap;
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
        let want = pick_face(db, family).map(|p| p.postscript);
        db.faces()
            .find(|face| match &want {
                // Metrics of the face we will actually draw with: Bold has
                // different vertical metrics from the upright face.
                Some(ps) => face.post_script_name == *ps,
                None => face.families.iter().any(|(name, _)| name == family),
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

/// The face we intend to use for a family: which one, and at what weight.
#[derive(Debug, Clone, PartialEq)]
pub struct FacePick {
    pub postscript: String,
    pub weight: u16,
}

/// Resolve a family name to the one face we want from it.
///
/// Asking for a family alone is not enough. A family can ship Bold, Italic and
/// Medium faces and no Regular at all -- Terminus (TTF) here is exactly that --
/// and "whichever face comes first in the database" is then a coin flip that can
/// land on Bold, which is what made the tab strip look emboldened. Rank upright
/// faces by distance from 400 and break ties by database order, so the answer is
/// stable and never an accident of file scan order.
pub fn pick_face(db: &Database, family: &str) -> Option<FacePick> {
    let mut best: Option<(u32, u16, String)> = None; // (penalty, weight, postscript)
    for face in db.faces() {
        if !face
            .families
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case(family))
        {
            continue;
        }
        // An italic cell is wrong in a terminal either way, so sloped faces are
        // only ever a last resort.
        let slope: u32 = match face.style {
            DbStyle::Normal => 0,
            _ => 10_000,
        };
        let penalty = slope + (face.weight.0 as i32 - 400).unsigned_abs();
        if best.as_ref().map(|(b, _, _)| penalty < *b).unwrap_or(true) {
            best = Some((penalty, face.weight.0, face.post_script_name.clone()));
        }
    }
    best.map(|(_, weight, postscript)| FacePick { postscript, weight })
}

/// Pin an `Attrs` to the exact face `pick_face` chose for `family`.
///
/// The postscript name goes FIRST: cosmic-text matches on it directly, which
/// takes face selection out of the matcher's hands entirely. The configured
/// family stays as the second candidate so an unresolvable name degrades to
/// today's behaviour instead of to a different font.
pub fn pin_face(db: &Database, attrs: Attrs<'static>, family: &'static str) -> Attrs<'static> {
    match pick_face(db, family) {
        Some(pick) => {
            log::info!("family {family:?} pinned to {:?} (weight {})", pick.postscript, pick.weight);
            attrs
                .family(Family::Name(Box::leak(pick.postscript.into_boxed_str())))
                .family(Family::Name(family))
                .weight(cosmic_weight(pick.weight))
                .style(Style::Normal)
        },
        None => attrs,
    }
}

fn cosmic_weight(w: u16) -> Weight {
    Weight(w)
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

    let push = |name: &str, chain: &mut Vec<String>| {
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

/// Which font should draw a given codepoint.
///
/// cosmic-text cannot answer this usefully. As `Fallback` it is effectively
/// unused (see `build_font_system`), and the one font it does consult is a
/// single global choice, so icons and CJK compete for the same slot.
///
/// This asks each loaded face's actual `cmap` whether it covers a codepoint, so
/// a font is selected by coverage rather than by name pattern: a Nerd Font
/// installed under an unusual name still works, and a font whose name merely
/// looks right does not get a free pass.
///
/// Results are memoised per codepoint, so per-cell cost is one hash lookup --
/// and nothing at all for ASCII, which any usable terminal font covers.
pub struct Coverage {
    /// Families the terminal already draws with; a hit here is not a fallback.
    primary: Vec<String>,
    /// Every loaded face, ordered so the best candidate comes first: primary
    /// families, then monospaced faces (`FaceInfo::monospaced`, a real font
    /// flag, not a name match), then everything else.
    faces: Vec<(ID, String)>,
    base: &'static Attrs<'static>,
    by_char: HashMap<char, &'static Attrs<'static>>,
    by_family: HashMap<String, &'static Attrs<'static>>,
}

impl Coverage {
    pub fn new(fs: &FontSystem, base: &Attrs<'static>, primary: &[String]) -> Coverage {
        let db: &Database = fs.db();
        let mut faces: Vec<(ID, String, u8)> = db
            .faces()
            .filter_map(|fi| {
                let fam = fi.families.first().map(|(n, _)| n.clone())?;
                let rank = if primary.iter().any(|p| p == &fam) {
                    0
                } else if primary.iter().any(|p| fam.starts_with(p.as_str())) {
                    // A companion variant of the terminal font ("Cascadia Code
                    // NF" for "Cascadia Code"): same design, so an icon's advance
                    // width still agrees with the cell grid. An ordering hint --
                    // coverage decides eligibility, so a variant that lacks the
                    // glyph is never chosen to satisfy the name.
                    1
                } else if fi.monospaced {
                    2
                } else {
                    3
                };
                Some((fi.id, fam, rank))
            })
            .collect();
        faces.sort_by(|a, b| a.2.cmp(&b.2).then_with(|| a.1.cmp(&b.1)));
        Coverage {
            primary: primary.to_vec(),
            faces: faces.into_iter().map(|(id, fam, _)| (id, fam)).collect(),
            base: Box::leak(Box::new(base.clone())),
            by_char: HashMap::new(),
            by_family: HashMap::new(),
        }
    }

    /// Attrs to draw `ch` with: the base attrs, unless some other installed face
    /// is the first one that actually covers the codepoint.
    pub fn attrs_for(&mut self, db: &Database, ch: char) -> &'static Attrs<'static> {
        if ch.is_ascii() {
            return self.base;
        }
        if let Some(a) = self.by_char.get(&ch) {
            return a;
        }
        let hit = self
            .faces
            .iter()
            .find(|(id, fam)| {
                !self.primary.iter().any(|p| p == fam) && face_covers(db, *id, ch)
            })
            .map(|(_, fam)| fam.clone());
        let attrs = match hit {
            Some(ref fam) => self.attrs_for_family(fam),
            // Nothing installed covers it: draw with the terminal font and accept
            // the notdef box, which at least keeps the cell grid intact.
            None => self.base,
        };
        if let Some(ref fam) = hit {
            log::debug!("glyph U+{:04X} -> {}", ch as u32, fam);
        }
        self.by_char.insert(ch, attrs);
        attrs
    }

    /// One leaked Attrs per fallback family -- bounded by how many a session
    /// needs, typically one or two. Everything from the base attrs is kept
    /// (size, weight, metrics); only the family changes.
    fn attrs_for_family(&mut self, fam: &str) -> &'static Attrs<'static> {
        if let Some(a) = self.by_family.get(fam) {
            return a;
        }
        // The leaked name gives the Attrs a 'static lifetime.
        let name: &'static str = Box::leak(fam.to_string().into_boxed_str());
        let attrs: &'static Attrs<'static> =
            Box::leak(Box::new(self.base.clone().family(Family::Name(name))));
        self.by_family.insert(fam.to_string(), attrs);
        attrs
    }

    /// Fallback families resolved so far (diagnostics, tests).
    pub fn families(&self) -> Vec<&str> {
        self.by_family.keys().map(|s| s.as_str()).collect()
    }
}

/// Does this face's `cmap` contain `ch`?
fn face_covers(db: &Database, id: ID, ch: char) -> bool {
    db.with_face_data(id, |data, index| {
        skrifa::FontRef::from_index(data, index)
            .map(|f| f.charmap().map(ch).is_some())
            .unwrap_or(false)
    })
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coverage(primary: &str) -> (FontSystem, Coverage) {
        let (fs, _) = build_font_system(&[primary]);
        // Attrs borrow the family name, and Coverage needs 'static attrs.
        let fam: &'static str = Box::leak(primary.to_string().into_boxed_str());
        let base = Attrs::new().family(Family::Name(fam));
        let cov = Coverage::new(&fs, &base, &[primary.to_string()]);
        (fs, cov)
    }

    /// The family we route to must actually contain the glyph -- otherwise the
    /// routing is just a different kind of tofu.
    fn really_covers(db: &Database, cov: &Coverage, fam: &str, ch: char) -> bool {
        cov.faces
            .iter()
            .any(|(id, f)| f == fam && face_covers(db, *id, ch))
    }

    #[test]
    fn ascii_never_consults_the_index() {
        let (fs, mut cov) = coverage("Cascadia Code");
        for ch in "abcXYZ019 ~!@#$%^&*()".chars() {
            assert!(
                std::ptr::eq(cov.attrs_for(fs.db(), ch), cov.base),
                "ASCII {:?} should draw with the terminal font",
                ch
            );
        }
    }

    #[test]
    fn powerline_glyph_gets_a_font_that_has_it() {
        let (fs, mut cov) = coverage("Cascadia Code");
        // U+E0B0: the powerline separator starship draws.
        let ch = '\u{e0b0}';
        let a = cov.attrs_for(fs.db(), ch);
        match a.family {
            Family::Name(name) if name != "Cascadia Code" => {
                assert!(
                    really_covers(fs.db(), &cov, name, ch),
                    "routed {ch:?} to {name}, which cannot draw it"
                );
            }
            _ => {
                // Acceptable only if the terminal font really does cover it.
                assert!(
                    cov.faces.iter().any(|(id, f)| f == "Cascadia Code"
                        && face_covers(fs.db(), *id, ch)),
                    "no fallback found for {ch:?}, but Cascadia Code cannot draw it either"
                );
            }
        }
    }

    #[test]
    fn resolution_is_stable_and_honest() {
        let (fs, mut cov) = coverage("Cascadia Code");
        // Mixed bag: CJK, box drawing, emoji, a combining mark, an arrow.
        for ch in ['あ', '█', '😀', '́', '⇢', '€', 'Ⅷ'] {
            let first = cov.attrs_for(fs.db(), ch);
            let second = cov.attrs_for(fs.db(), ch);
            assert!(std::ptr::eq(first, second), "{ch:?} resolved twice differently");
            if let Family::Name(name) = first.family {
                if name != "Cascadia Code" {
                    assert!(
                        really_covers(fs.db(), &cov, name, ch),
                        "routed {ch:?} to {name}, which cannot draw it"
                    );
                }
            }
        }
    }

    #[test]
    fn unassigned_codepoints_stay_with_the_terminal_font() {
        let (fs, mut cov) = coverage("Cascadia Code");
        // Plane 15 private use: nothing on a normal desktop covers these.
        for ch in ['\u{f0000}', '\u{fffff}'] {
            assert!(
                std::ptr::eq(cov.attrs_for(fs.db(), ch), cov.base),
                "{ch:?} should not be routed anywhere"
            );
        }
    }

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
