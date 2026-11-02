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
    skrifa, skrifa::prelude::*, Attrs, Buffer, Fallback, Family, FeatureTag, FontFeatures,
    FontSystem, Hinting, PlatformFallback, Shaping, Style, Weight, Wrap,
};
pub use glyphon::fontdb::{Database, ID, Style as DbStyle};
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
    /// (unitsPerEm, ascender, descender) from the `hhea` table, when it could be
    /// read. That table -- not the OS/2 typo box skrifa reports -- is the one
    /// FreeType, Cairo and Pango use for a font's line, and it is what decides
    /// whether a full-height glyph fits its cell.
    pub hhea: Option<(f32, f32, f32)>,
}

impl LineMetrics {
    /// Fallback when a family cannot be found: 1.25 em, a typical terminal.
    pub const DEFAULT: LineMetrics = LineMetrics {
        upem: 1000.0,
        ascent: 1000.0,
        descent: -250.0,
        leading: 0.0,
        hhea: None,
    };

    /// Pixel line height, using Pango's rule: the font's ascent and descent each
    /// rounded UP to a whole pixel, and no leading term. Verified against Pango
    /// itself over 16 font/size combinations (`ceil(hhea.asc) + ceil(-hhea.desc)`
    /// matched 14 of them; the misses are Terminus at its bitmap-strike sizes,
    /// where FreeType answers with the strike's metrics instead of hhea).
    ///
    /// The old rule -- round each of ascent, descent and leading from skrifa's
    /// typo metrics -- was one pixel short for Cascadia Code 9 (14 instead of
    /// 15). Ordinary text only inks 9px so nothing showed, but a powerline peak
    /// inks 14px and had nowhere to go, so it bled into the row below.
    pub fn height_at(&self, px: f32) -> f32 {
        let (upem, asc, desc) = match self.hhea {
            Some(h) => h,
            None => (self.upem, self.ascent, self.descent),
        };
        if upem <= 0.0 {
            return (px * 1.25).max(1.0);
        }
        let scale = px / upem;
        let a = (asc * scale).ceil();
        let d = (desc.abs() * scale).ceil();
        (a + d).max(1.0)
    }
}

/// Read (unitsPerEm, ascender, descender) out of the `hhea` table.
///
/// Neither fontdb 0.23 nor skrifa 0.40 exposes it -- skrifa's `Metrics` gives
/// the OS/2 typo box instead, which for DejaVu Sans Mono, Hack and Liberation
/// Mono is several pixels shorter than hhea and would silently shrink the row
/// height out of agreement with Pango. So: walk the sfnt table directory.
fn read_hhea(db: &Database, id: ID) -> Option<(f32, f32, f32)> {
    fn be16(b: &[u8], at: usize) -> Option<u16> {
        Some(u16::from_be_bytes(b.get(at..at + 2)?.try_into().ok()?))
    }
    fn be32(b: &[u8], at: usize) -> Option<u32> {
        Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
    }
    db.with_face_data(id, |data, index| {
        // A TrueType collection keeps each font at its own offset.
        let base = if data.get(0..4) == Some(b"ttcf") {
            let n = be32(data, 8)? as usize;
            let i = (index as usize).min(n.saturating_sub(1));
            be32(data, 12 + i * 4)? as usize
        } else {
            0
        };
        let ntables = be16(data, base + 4)? as usize;
        let mut head: Option<usize> = None;
        let mut hhea: Option<usize> = None;
        for i in 0..ntables {
            let rec = base + 12 + i * 16;
            match data.get(rec..rec + 4) {
                Some(b"head") => head = be32(data, rec + 8).map(|o| o as usize),
                Some(b"hhea") => hhea = be32(data, rec + 8).map(|o| o as usize),
                _ => {}
            }
            if head.is_some() && hhea.is_some() {
                break;
            }
        }
        let upem = be16(data, head? + 18)? as f32;
        // hhea opens with a 4-byte version, then ascender, descender, lineGap --
        // reading the ascent at +2 gets the version's minor word instead, which
        // is 0, and the ascent then shows up as the descent.
        let asc = (be16(data, hhea? + 4)? as i16) as f32;
        let desc = (be16(data, hhea? + 6)? as i16) as f32;
        if upem <= 0.0 {
            return None;
        }
        Some((upem, asc, desc))
    })
    .flatten()
}

/// Read a family's vertical metrics out of the loaded database.
pub fn line_metrics(
    fs: &mut FontSystem,
    family: &str,
    weight: u16,
    slant: DbStyle,
) -> Option<LineMetrics> {
    let id = {
        let db = fs.db_mut();
        let want = pick_face_near(db, family, weight, slant).map(|p| p.postscript);
        db.faces()
            .find(|face| match &want {
                // Metrics of the face we will actually draw with: Bold has
                // different vertical metrics from the upright face.
                Some(ps) => face.post_script_name == *ps,
                None => face.families.iter().any(|(name, _)| name == family),
            })
            .map(|face| face.id)?
    };
    // hhea first: skrifa's Metrics borrows the face (and through it the
    // FontSystem), so nothing can touch the database while it is alive.
    let hhea = {
        let db = fs.db_mut();
        read_hhea(db, id)
    };
    let (upem, ascent, descent, leading) = {
        // The requested weight, not NORMAL: for a variable face this picks the
        // named instance the terminal actually draws with.
        let font = fs.get_font(id, glyphon::fontdb::Weight(weight))?;
        let m = font.metrics();
        (
            m.units_per_em as f32,
            m.ascent,
            m.descent,
            m.leading,
        )
    };
    if upem <= 0.0 {
        return None;
    }
    let lm = LineMetrics {
        upem,
        ascent,
        descent,
        leading,
        hhea,
    };
    log::debug!(
        "line_metrics({family}): typo upem={upem} asc={ascent} desc={descent} lead={leading} hhea={:?} -> {}px at 12px",
        lm.hhea,
        lm.height_at(12.0)
    );
    Some(lm)
}

/// The face we intend to use for a family: which one, and at what weight.
#[derive(Debug, Clone, PartialEq)]
pub struct FacePick {
    pub postscript: String,
    pub weight: u16,
    pub slant: DbStyle,
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
    pick_face_near(db, family, 400, DbStyle::Normal)
}

/// The face of `family` nearest the requested weight and slant.
///
/// Asking for a family alone is not enough. A family can ship Bold, Italic and
/// Medium faces and no Regular at all -- Terminus (TTF) here is exactly that --
/// and "whichever face comes first in the database" is then a coin flip that can
/// land on Bold, which is what made the tab strip look emboldened. Rank faces by
/// distance from the request (slant first: an unintended italic is worse than an
/// unintended weight) and break ties by database order, so the answer is stable
/// and never an accident of file scan order. With the default request (400,
/// Normal) this is the old upright-near-400 picker.
pub fn pick_face_near(
    db: &Database,
    family: &str,
    weight: u16,
    slant: DbStyle,
) -> Option<FacePick> {
    let mut best: Option<(u32, u16, DbStyle, String)> = None; // (penalty, weight, slant, postscript)
    for face in db.faces() {
        if !face
            .families
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case(family))
        {
            continue;
        }
        // An unintended slope is wrong in a terminal either way, so mismatched
        // slant is a last resort; a requested slant pays the same tax in
        // reverse, keeping an upright family's faked-slope fall from grace
        // symmetric.
        let slope: u32 = if face.style == slant {
            0
        } else if face.style == DbStyle::Normal {
            10_000 // upright face, slope requested
        } else if slant == DbStyle::Normal {
            10_000 // slope face, upright requested (the old rule)
        } else {
            5_000 // Italic vs Oblique: closer than either extreme
        };
        let penalty = slope + (face.weight.0 as i32 - weight as i32).unsigned_abs();
        if best.as_ref().map(|(b, _, _, _)| penalty < *b).unwrap_or(true) {
            best = Some((
                penalty,
                face.weight.0,
                face.style,
                face.post_script_name.clone(),
            ));
        }
    }
    best.map(|(_, weight, slant, postscript)| FacePick { postscript, weight, slant })
}

/// Pin default-text attrs to a resolved face: the weight and slant the
/// terminal draws ordinary text with. Keep the family name so SGR
/// bold/italic can still select sibling faces.
pub fn pin_face(
    db: &Database,
    attrs: Attrs<'static>,
    family: &'static str,
    weight: u16,
    slant: DbStyle,
) -> Attrs<'static> {
    match pick_face_near(db, family, weight, slant) {
        Some(pick) => {
            log::info!(
                "family {family:?} pinned to {:?} (weight {} {pick_slant:?})",
                pick.postscript,
                pick.weight,
                pick_slant = pick.slant
            );
            attrs
                .family(Family::Name(family))
                .weight(cosmic_weight(pick.weight))
                .style(match pick.slant {
                    DbStyle::Normal => Style::Normal,
                    DbStyle::Italic | DbStyle::Oblique => Style::Italic,
                })
        },
        // Family unknown to the database: keep the name and let cosmic-text's
        // own fallback do what it always did.
        None => attrs.family(Family::Name(family)),
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

/// One face of an installed family, as the font popup offers it.
#[derive(Clone, Debug)]
pub struct FaceChoice {
    pub weight: u16,
    pub slant: DbStyle,
    pub path: std::path::PathBuf,
    /// Face index within the file (matters for `.ttc` collections).
    pub index: u32,
}

/// One selectable installed family: every file-backed face it ships, so the
/// popup can offer its real styles and preview each one.
#[derive(Clone, Debug)]
pub struct FontChoice {
    pub name: String,
    /// Lowercased name, precomputed: the popup filters on it every time the
    /// filter text changes.
    pub name_lower: String,
    pub faces: Vec<FaceChoice>,
}

/// One choice per installed family, each carrying its faces sorted by weight
/// then slant (duplicated (weight, slant) faces keep the first, database
/// order). Sorted by family name, case-insensitive.
pub fn font_choices(db: &Database) -> Vec<FontChoice> {
    let mut families: std::collections::HashMap<String, Vec<FaceChoice>> =
        std::collections::HashMap::new();
    for face in db.faces() {
        let Some((name, _)) = face.families.first() else {
            continue;
        };
        let Some(path) = source_path(&face.source) else {
            continue;
        };
        families
            .entry(name.clone())
            .or_default()
            .push(FaceChoice {
                weight: face.weight.0,
                slant: face.style,
                path,
                index: face.index,
            });
    }
    let mut out: Vec<FontChoice> = families
        .into_iter()
        .map(|(name, mut faces)| {
            faces.sort_by_key(|f| (f.weight, f.slant as u8));
            faces.dedup_by(|a, b| a.weight == b.weight && a.slant == b.slant);
            let name_lower = name.to_lowercase();
            FontChoice { name, name_lower, faces }
        })
        .collect();
    out.sort_by(|a, b| a.name_lower.cmp(&b.name_lower));
    out
}

/// Style words a Pango font description can carry between the family and the
/// size: (word, weight). "Regular" is a word too, so "Foo Regular 12" parses
/// as well as it writes back.
const WEIGHT_WORDS: &[(&str, u16)] = &[
    ("Thin", 100),
    ("UltraLight", 200),
    ("ExtraLight", 200),
    ("Light", 300),
    ("Book", 400),
    ("Regular", 400),
    ("Medium", 500),
    ("SemiBold", 600),
    ("DemiBold", 600),
    ("Bold", 700),
    ("ExtraBold", 800),
    ("UltraBold", 800),
    ("Black", 900),
    ("Heavy", 900),
];

/// The words the popup writes back; deliberately single tokens so a family
/// name that swallows spaces ("Semi Bold" written back as "SemiBold") still
/// round-trips through `split_style_suffix`.
fn weight_word(weight: u16) -> &'static str {
    match weight {
        0..=150 => "Thin",
        151..=250 => "UltraLight",
        251..=350 => "Light",
        351..=450 => "",
        451..=550 => "Medium",
        551..=650 => "SemiBold",
        651..=750 => "Bold",
        751..=850 => "ExtraBold",
        _ => "Black",
    }
}

fn slant_word(slant: DbStyle) -> &'static str {
    match slant {
        DbStyle::Normal => "",
        DbStyle::Italic | DbStyle::Oblique => "Italic",
    }
}

/// The style portion of a font description: "Bold Italic", "Light",
/// "Italic", or "" for the plain upright face (then the caller writes just
/// "Family Size").
pub fn style_words(weight: u16, slant: DbStyle) -> String {
    [weight_word(weight), slant_word(slant)]
        .into_iter()
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Peel style words off the tail of a family spec: "Foo Bold Italic" ->
/// ("Foo", 700, Italic). Returns None when the tail is not a style word or
/// the whole spec would be consumed ("Bold" alone is a family name as far as
/// we are concerned). Only used when the full spec does not name an
/// installed family -- families that ARE named "Roboto Medium" keep winning.
pub fn split_style_suffix(spec: &str) -> Option<(String, u16, DbStyle)> {
    let tokens: Vec<&str> = spec.split_whitespace().collect();
    let (mut weight, mut slant) = (None, None);
    let mut end = tokens.len();
    for _ in 0..2 {
        let Some(t) = tokens.get(end.wrapping_sub(1)) else { break };
        let low = t.to_lowercase();
        if low == "italic" || low == "oblique" {
            slant = Some(DbStyle::Italic);
        } else if let Some((_, w)) = WEIGHT_WORDS.iter().find(|(n, _)| n.to_lowercase() == low) {
            weight = Some(*w);
        } else {
            break;
        }
        end -= 1;
    }
    if weight.is_none() && slant.is_none() {
        return None;
    }
    if end == 0 {
        return None;
    }
    Some((
        tokens[..end].join(" "),
        weight.unwrap_or(400),
        slant.unwrap_or(DbStyle::Normal),
    ))
}

/// A font description's family spec resolved against the real database.
pub struct ResolvedFont {
    /// The installed family to ask for (the spec itself when it names one).
    pub family: String,
    /// Weight and slant to pin the default-text attrs to.
    pub weight: u16,
    pub slant: DbStyle,
}

/// Resolve a family spec (which may carry style words) against the database.
///
/// A spec that names an installed family is used whole, whatever it
/// contains: some families ARE called "Roboto Medium", and peeling that to
/// "Roboto" + 500 would change the design. Only an unknown spec is peeled
/// ("Foo Bold 12" -> family "Foo", pinned to 700). With no match at all the
/// spec passes through unchanged -- face matching then falls back exactly as
/// it always has.
pub fn resolve_font(db: &Database, spec: &str) -> ResolvedFont {
    if is_family(db, spec) {
        let (weight, slant) = pick_face_near(db, spec, 400, DbStyle::Normal)
            .map(|p| (p.weight, p.slant))
            .unwrap_or((400, DbStyle::Normal));
        return ResolvedFont { family: spec.to_string(), weight, slant };
    }
    if let Some((base, weight, slant)) = split_style_suffix(spec) {
        if is_family(db, &base) {
            let (weight, slant) = pick_face_near(db, &base, weight, slant)
                .map(|p| (p.weight, p.slant))
                .unwrap_or((weight, slant));
            return ResolvedFont { family: base, weight, slant };
        }
    }
    ResolvedFont { family: spec.to_string(), weight: 400, slant: DbStyle::Normal }
}

fn is_family(db: &Database, name: &str) -> bool {
    db.faces()
        .any(|f| f.families.iter().any(|(n, _)| n.eq_ignore_ascii_case(name)))
}

fn source_path(source: &glyphon::fontdb::Source) -> Option<std::path::PathBuf> {
    match source {
        glyphon::fontdb::Source::File(path) => Some(path.clone()),
        glyphon::fontdb::Source::SharedFile(path, _) => Some(path.clone()),
        // Memory-only sources carry no readable path.
        glyphon::fontdb::Source::Binary(_) => None,
    }
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
        // Resolve style words first: icon siblings of "Foo Bold" are the ones
        // of "Foo" ("Foo Nerd Font"), and a literal "Foo Bold Nerd Font"
        // suffix almost never exists.
        let resolved = resolve_font(&db, primary).family;
        for fam in icon_families(&resolved, &db) {
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
    styled: HashMap<(usize, bool, bool), &'static Attrs<'static>>,
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
            styled: HashMap::new(),
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
        // A fallback is only needed when the primary really lacks the glyph.
        // Previously every non-ASCII character skipped the primary outright,
        // mixing unrelated designs even for ordinary accented filenames.
        if self.faces.iter().any(|(id, fam)| {
            self.primary.iter().any(|p| p == fam) && face_covers(db, *id, ch)
        }) {
            self.by_char.insert(ch, self.base);
            return self.base;
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

    pub fn styled_attrs(&mut self, db: &Database, ch: char, bold: bool, italic: bool) -> &'static Attrs<'static> {
        let base = self.attrs_for(db, ch);
        if !bold && !italic {
            return base;
        }
        let key = (base as *const Attrs<'static> as usize, bold, italic);
        self.styled.entry(key).or_insert_with(|| {
            Box::leak(Box::new(base.clone()
                .weight(if bold { Weight::BOLD } else { base.weight })
                .style(if italic { Style::Italic } else { Style::Normal })))
        })
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

/// Shape at the requested font size, then place clusters on the terminal's
/// existing cell grid. A terminal row must NEVER word-wrap inside a text buffer.
/// `set_monospace_width` is unsuitable here: cosmic-text rescales glyphs to fit
/// that width and wraps using their original advances, so fractional advances
/// can push filename tails onto the next row. It also changes fallback sizes.
///
/// `cells` maps UTF-8 byte offsets to columns, including combining marks in the
/// same cell and wide characters spanning two. Shaping handles accents and
/// scripts; the terminal remains the authority on where each cluster belongs.
pub fn shape_terminal_run(
    buffer: &mut Buffer,
    fs: &mut FontSystem,
    text: &str,
    attrs: &Attrs,
    cells: &[(usize, u32)],
    cell_w: f32,
) {
    buffer.set_wrap(Wrap::None);
    buffer.set_hinting(Hinting::Enabled);
    buffer.set_monospace_width(None);
    buffer.set_size(None, None);
    let ascii = text.is_ascii();
    if ascii {
        // ASCII has no combining clusters or complex scripts. Running every
        // find/ls path through the complex shaper is expensive on a large grid.
        // The same explicit cell placement below still prevents spacing drift.
        buffer.set_text(text, attrs, Shaping::Basic, None);
    } else {
        let mut features = FontFeatures::new();
        for tag in [b"liga", b"clig", b"dlig", b"calt", b"kern"] {
            features.disable(FeatureTag::new(tag));
        }
        buffer.set_text(text, &attrs.clone().font_features(features), Shaping::Advanced, None);
    }
    buffer.shape_until_scroll(fs, false);
    for line in &mut buffer.lines {
        let Some(layout) = line.layout_opt_mut() else { continue };
        for row in layout {
            if ascii {
                for glyph in &mut row.glyphs {
                    glyph.x = cells.get(glyph.start).map_or(0, |(_, col)| *col) as f32 * cell_w;
                }
                continue;
            }
            // Glyphs in one cluster retain their relative positioning (marks,
            // decomposed accents), while cluster origins land on whole pixels.
            let mut origins = HashMap::new();
            for glyph in &mut row.glyphs {
                let source = *origins.entry(glyph.start).or_insert(glyph.x);
                let i = cells.partition_point(|(byte, _)| *byte <= glyph.start).saturating_sub(1);
                let column = cells.get(i).map_or(0, |(_, col)| *col);
                glyph.x = (column as f32 * cell_w).round() + glyph.x - source;
            }
        }
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

    /// The row height must be Pango's, because that is the box VTE lays a cell
    /// out from: a full-height glyph only fits when the cell is tall enough, and
    /// a row one pixel short pushes a powerline peak into the row below. Both
    /// numbers are Pango's own, measured with a layout; the general rule was
    /// checked over 16 font/size combinations.
    #[test]
    fn row_height_matches_pango() {
        for (family, pt, want) in [("Cascadia Code", 9.0, 15.0), ("Terminus (TTF)", 12.0, 17.0)] {
            let (mut fs, _) = build_font_system(&[family]);
            let lm = match line_metrics(&mut fs, family, 400, DbStyle::Normal) {
                Some(lm) => lm,
                None => {
                    eprintln!("skipping {family}: not installed here");
                    continue;
                },
            };
            assert_eq!(
                lm.height_at(pt * 96.0 / 72.0),
                want,
                "{family} {pt}pt row height should match Pango"
            );
        }
    }

    /// Pango style words: peeled from a spec only when the spec does not
    /// name a real family; written back as single round-trippable words.
    #[test]
    fn style_suffix_parses_and_composes() {
        assert_eq!(
            split_style_suffix("Cascadia Code Bold Italic"),
            Some(("Cascadia Code".to_string(), 700, DbStyle::Italic))
        );
        assert_eq!(
            split_style_suffix("Mono Light"),
            Some(("Mono".to_string(), 300, DbStyle::Normal))
        );
        assert_eq!(
            split_style_suffix("Roboto BOOK"),
            Some(("Roboto".to_string(), 400, DbStyle::Normal))
        );
        // No style tail: the spec is a plain family name.
        assert_eq!(split_style_suffix("Cascadia Code"), None);
        // Never peel the whole spec: a family really can be called "Bold".
        assert_eq!(split_style_suffix("Bold"), None);
        // Only the TAIL counts.
        assert_eq!(split_style_suffix("Bold Sans"), None);

        assert_eq!(style_words(400, DbStyle::Normal), "");
        assert_eq!(style_words(700, DbStyle::Normal), "Bold");
        assert_eq!(style_words(400, DbStyle::Italic), "Italic");
        assert_eq!(style_words(700, DbStyle::Oblique), "Bold Italic");
        // Words written back are words that parse again.
        for (w, s) in [(100, DbStyle::Normal), (300, DbStyle::Italic), (700, DbStyle::Normal), (900, DbStyle::Italic)] {
            let words = style_words(w, s);
            let spec = format!("FontTest {words}");
            let (base, pw, ps) = split_style_suffix(&spec).expect(&format!("round trip {words:?}"));
            assert_eq!(base, "FontTest");
            assert_eq!((pw, ps), (w, s), "round trip of {words:?} must recover weight/slant");
        }
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
    fn ls_columns_never_wrap_or_accumulate_fractional_spacing() {
        let (mut fs, mut cov) = coverage("Cascadia Code");
        let attrs = cov.attrs_for(fs.db(), 'a');
        let text = "drwxr-xr-x  9 volca volca 4096 Oct 31 00:41 .claude";
        let cells: Vec<_> = text.char_indices().enumerate().map(|(col, (byte, _))| (byte, col as u32)).collect();
        let mut buffer = Buffer::new(&mut fs, glyphon::Metrics::new(12.0, 15.0));
        shape_terminal_run(&mut buffer, &mut fs, text, attrs, &cells, 7.0);
        let rows: Vec<_> = buffer.layout_runs().collect();
        assert_eq!(rows.len(), 1, "a terminal run must stay on its row");
        assert_eq!(rows[0].glyphs.len(), text.len());
        for glyph in rows[0].glyphs {
            assert_eq!(glyph.x, glyph.start as f32 * 7.0);
            assert_eq!(glyph.font_size, 12.0, "grid placement must not rescale the font");
        }
    }

    #[test]
    fn wide_and_combining_characters_keep_following_columns_aligned() {
        let (mut fs, mut cov) = coverage("Cascadia Code");
        let attrs = cov.attrs_for(fs.db(), 'a');
        let text = "a\u{301}界b";
        let cells = [(0, 0), (3, 1), (6, 3)];
        let mut buffer = Buffer::new(&mut fs, glyphon::Metrics::new(12.0, 15.0));
        shape_terminal_run(&mut buffer, &mut fs, text, attrs, &cells, 7.0);
        let rows: Vec<_> = buffer.layout_runs().collect();
        assert_eq!(rows.len(), 1);
        let b = rows[0].glyphs.iter().find(|g| g.start == 6).unwrap();
        assert_eq!(b.x, 21.0);
    }

    #[test]
    fn bold_and_italic_are_distinct_from_regular() {
        let (fs, mut cov) = coverage("Cascadia Code");
        let regular = cov.styled_attrs(fs.db(), 'd', false, false);
        let bold = cov.styled_attrs(fs.db(), 'd', true, false);
        let italic = cov.styled_attrs(fs.db(), 'd', false, true);
        assert_ne!(regular.weight, bold.weight);
        assert_eq!(bold.weight, Weight::BOLD);
        assert_eq!(italic.style, Style::Italic);
        assert!(std::ptr::eq(bold, cov.styled_attrs(fs.db(), 'd', true, false)));
    }

    #[test]
    fn supported_accents_stay_in_the_primary_family() {
        let (fs, mut cov) = coverage("Cascadia Code");
        for ch in ['é', 'č', 'ü'] {
            if cov.faces.iter().any(|(id, fam)| fam == "Cascadia Code" && face_covers(fs.db(), *id, ch)) {
                assert!(std::ptr::eq(cov.attrs_for(fs.db(), ch), cov.base));
            }
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
        let lm = LineMetrics {
            upem: 1000.0,
            ascent: 800.0,
            descent: -200.0,
            leading: 0.0,
            hhea: None,
        };
        assert_eq!(lm.height_at(10.0), 10.0);
    }

    #[test]
    fn hhea_box_drives_row_height_and_leading_does_not() {
        // Terminus (TTF): hhea is (1000, 849, -183) while the OS/2 typo box skrifa
        // reports is (1000, 833, -167, leading 90). Pango's row at 16px is 17,
        // which is ceil(13.584) + ceil(2.928) from hhea -- and NOT typo+leading
        // (13+3+1), which only agrees by accident, nor typo+leading with hhea's
        // numbers (19), which is too tall. The strip the original measures is 21px
        // = this 17 plus 2px margins.
        let lm = LineMetrics {
            upem: 1000.0,
            ascent: 833.0,
            descent: -167.0,
            leading: 90.0,
            hhea: Some((1000.0, 849.0, -183.0)),
        };
        assert_eq!(lm.height_at(16.0), 17.0);
        // The case that started this: Cascadia Code 9, hhea (2048, 1900, -480).
        // ceil(11.13) + ceil(2.81) = 15, where the old rule gave 14 and a
        // powerline peak (14px of ink) spilled into the row below.
        let cas = LineMetrics {
            upem: 2048.0,
            ascent: 1900.0,
            descent: -480.0,
            leading: 0.0,
            hhea: Some((2048.0, 1900.0, -480.0)),
        };
        assert_eq!(cas.height_at(12.0), 15.0);
    }

    #[test]
    fn degenerate_upem_does_not_panic() {
        let lm = LineMetrics {
            upem: 0.0,
            ascent: 0.0,
            descent: 0.0,
            leading: 0.0,
            hhea: None,
        };
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
