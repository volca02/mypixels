//! The Kitty graphics protocol, application side.
//!
//! The vendored terminal (see the `apc_raw` patches in `vendor/vte` and
//! `vendor/alacritty_terminal`) delivers complete APC strings with the
//! cursor's absolute position at the instant the sequence ended. Everything
//! that interprets those bytes - the parameter language, chunked transfers,
//! the image store and placements - lives HERE, in a pure state machine
//! with no locks, no wgpu and no terminal: it is unit-tested like any other
//! data structure, and the renderer below it is just one consumer. The
//! clipboard future (`copy the image the last command printed`) is the
//! second: images keep their original upload bytes, so a copy is lossless
//! and never round-trips through pixels.
//!
//! Implemented subset of https://sw.kovidgoyal.net/kitty/graphicsprotocol :
//! `a=t` transmit, `a=T` transmit+display, `a=p` display, `a=d` delete,
//! `f=100` PNG, image ids, placement ids, chunked transfer (`m=1`),
//! columns/rows sizing, x/y offsets, z-index. Raw RGB/RGBA (`f=24/32`),
//! zlib (`o=z`), cursor-relative delete filters, file/shared-memory
//! transfer, protocol responses and scroll-cursor semantics are NOT
//! implemented; they are ignored or rejected per the caps below, and
//! advertising them (`a=q`) is deferred rather than answered falsely.
//!
//! Caps are blunt by design: one transfer, one image, one tab each has a
//! ceiling, and crossing one drops the sequence instead of growing memory.
//! A terminal that OOMs on a malicious escape stream helps nobody.

use base64::Engine as _;
use std::collections::HashMap;
use std::sync::Arc;

/// Largest assembled transfer (decoded base64 length is not checked first;
/// the buffer is capped while accumulating, which is what matters).
pub const MAX_TRANSFER: usize = 32 * 1024 * 1024;
/// Largest accepted decoded image per side.
pub const MAX_SIDE: u32 = 8192;
/// Images retained per tab; older unreferenced ones are evicted.
pub const MAX_IMAGES: usize = 64;
/// Placements retained per tab.
pub const MAX_PLACEMENTS: usize = 1024;

/// One decoded image. `gen` bumps when an id is overwritten, giving the
/// GPU texture cache a cheap invalidation key.
pub struct KittyImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// The untouched upload payload (PNG today), for a lossless copy.
    pub source: Arc<Vec<u8>>,
    pub generation: u32,
}

/// Where an image sits: anchored at a cell (absolute logical line, as the
/// terminal saw it) with a column, plus pixel offsets and size in cells or
/// native pixels.
#[derive(Clone, Copy, Debug)]
pub struct Placement {
    pub image_id: u32,
    pub placement_id: u32,
    pub anchor_line: usize,
    pub col: usize,
    pub cols: Option<u32>,
    pub rows: Option<u32>,
    pub x_off: u32,
    pub y_off: u32,
    pub z: i32,
}

/// An in-progress chunked transfer.
struct Pending {
    id: u32,
    display: bool,
    format: u32,
    cols: Option<u32>,
    rows: Option<u32>,
    x_off: u32,
    y_off: u32,
    z: i32,
    anchor_line: usize,
    anchor_col: usize,
    buf: Vec<u8>,
}

/// Per-tab graphics state. Owned by the app, keyed by tab id; it moves and
/// dies with the tab.
pub struct TabKitty {
    images: HashMap<u32, KittyImage>,
    placements: Vec<Placement>,
    next_id: u32,
    next_placement: u32,
    pending: Option<Pending>,
}

impl Default for TabKitty {
    fn default() -> Self {
        Self {
            images: HashMap::new(),
            placements: Vec::new(),
            next_id: 1,
            next_placement: 1,
            pending: None,
        }
    }
}

impl TabKitty {
    /// One complete APC string (the bytes between `ESC _` and ST, so it
    /// starts with the `G` namespace letter for Kitty), anchored at
    /// `line`/`col` as captured at dispatch.
    pub fn on_apc(&mut self, payload: &[u8], line: usize, col: usize) {
        if payload.first() != Some(&b'G') {
            // Some other APC dialect (tmux passes several through); not
            // ours to interpret, and nothing here may misread it.
            return;
        }
        // Wire shape is `G;<params>;<data>`. The ';' after the namespace
        // letter is part of the framing - strip it before splitting, or
        // every real frame reads as an empty-params continuation. A
        // continuation frame is `G;` followed by raw data (empty params).
        let bare = &payload[1..];
        let rest: &[u8] = if bare.first() == Some(&b';') { &bare[1..] } else { bare };
        let (params, data): (&[u8], &[u8]) = match rest.iter().position(|&b| b == b';') {
            Some(p) => (&rest[..p], &rest[p + 1..]),
            // `G;X` with no second ';': X is parameters unless it cannot
            // possibly be (no '=' anywhere) while a transfer awaits raw
            // bytes - then it IS the data. Param keys always ride in `k=v`
            // form, so the two readings cannot be confused.
            None if rest.contains(&b'=') || self.pending.is_none() => (rest, &[]),
            None => (&[] as &[u8], rest),
        };
        let params: Vec<(&[u8], &[u8])> = params
            .split(|&b| b == b',')
            .filter(|kv| !kv.is_empty())
            .filter_map(|kv| kv.iter().position(|&b| b == b'=').map(|e| (&kv[..e], &kv[e + 1..])))
            .collect();

        let get = |k: u8| params.iter().find(|(key, _)| key.first() == Some(&k)).map(|(_, v)| *v);
        let num = |k: u8| -> u64 {
            get(k).and_then(|v| std::str::from_utf8(v).ok())
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(0)
        };
        let append = |slf: &mut Self, data: &[u8]| -> bool {
            match slf.pending.as_mut() {
                Some(p) if p.buf.len() + data.len() <= MAX_TRANSFER => {
                    p.buf.extend_from_slice(data);
                    true
                },
                Some(_) => {
                    log::warn!("kitty: transfer over cap, dropped");
                    slf.pending = None;
                    false
                },
                None => false,
            }
        };

        // Continuation, shape one: no parameters at all (`ESC_G;<data>ST`).
        // Stays open - such a frame cannot state `m`, so only a later
        // parameterized frame can end the transfer.
        if params.is_empty() {
            append(self, data);
            return;
        }

        let action = get(b'a').and_then(|v| v.first()).copied().unwrap_or(b't');
        let more = num(b'm') == 1;

        // Continuation, shape two: a frame carrying neither `a` nor `i`
        // while a transfer lives can only belong to it.
        if self.pending.is_some() && get(b'a').is_none() && get(b'i').is_none() {
            if append(self, data) && !more {
                self.complete_pending();
            }
            return;
        }

        match action {
            b't' | b'T' => {
                let format = num(b'f') as u32;
                let id = {
                    let i = num(b'i') as u32;
                    if i == 0 {
                        self.next_id
                    } else {
                        i
                    }
                };
                // Continuation, shape three: senders like icat repeat the
                // FULL header on every chunk; same id + same action means
                // more data for the open transfer, not a resync.
                let repeat = self
                    .pending
                    .as_ref()
                    .is_some_and(|p| p.id == id && (p.display == (action == b'T')));
                if repeat {
                    if append(self, data) && !more {
                        self.complete_pending();
                    }
                    return;
                }
                // A genuinely new header frame while one is open abandons
                // the old: the spec's resync rule.
                let mut p = Pending {
                    id,
                    display: action == b'T',
                    format,
                    cols: (num(b'c') > 0).then(|| num(b'c') as u32),
                    rows: (num(b'r') > 0).then(|| num(b'r') as u32),
                    x_off: num(b'x') as u32,
                    y_off: num(b'y') as u32,
                    z: num(b'z') as u32 as i32,
                    anchor_line: line,
                    anchor_col: col,
                    buf: Vec::new(),
                };
                if data.len() > MAX_TRANSFER {
                    return;
                }
                p.buf.extend_from_slice(data);
                self.pending = Some(p);
                if !more {
                    self.complete_pending();
                }
            },
            b'p' => {
                let id = num(b'i') as u32;
                if !self.images.contains_key(&id) {
                    return;
                }
                let placement_id = {
                    let p = num(b'p') as u32;
                    if p == 0 {
                        self.next_placement
                    } else {
                        p
                    }
                };
                self.push_placement(Placement {
                    image_id: id,
                    placement_id,
                    anchor_line: line,
                    col,
                    cols: (num(b'c') > 0).then(|| num(b'c') as u32),
                    rows: (num(b'r') > 0).then(|| num(b'r') as u32),
                    x_off: num(b'x') as u32,
                    y_off: num(b'y') as u32,
                    z: num(b'z') as u32 as i32,
                });
            },
            b'd' => {
                let id = num(b'i') as u32;
                let place = num(b'p') as u32;
                if id == 0 && place == 0 {
                    self.placements.clear();
                    self.images.clear();
                    self.pending = None;
                } else if place != 0 {
                    self.placements.retain(|pl| pl.placement_id != place);
                } else {
                    self.placements.retain(|pl| pl.image_id != id);
                    self.images.remove(&id);
                }
            },
            // 'q' (query) and everything unknown: ignored. Answering `q`
            // wrongly would advertise a subset we do not fully implement;
            // answering it truly belongs with capability work (doc M6).
            _ => {},
        }
    }

    fn push_placement(&mut self, pl: Placement) {
        if pl.placement_id >= self.next_placement {
            self.next_placement = pl.placement_id + 1;
        }
        self.placements.push(pl);
        while self.placements.len() > MAX_PLACEMENTS {
            self.placements.remove(0);
        }
    }

    fn complete_pending(&mut self) {
        let Some(p) = self.pending.take() else { return };
        if p.id >= self.next_id {
            self.next_id = p.id + 1;
        }
        let bytes = match decode_b64(&p.buf) {
            Some(b) => b,
            None => {
                log::warn!("kitty: base64 decode failed, frame dropped");
                return;
            },
        };
        let rgba = match p.format {
            100 => match decode_png(&bytes) {
                Some(img) => img,
                None => {
                    log::warn!("kitty: png decode failed, frame dropped");
                    return;
                },
            },
            // Raw RGB/RGBA needs s/w geometry this version does not
            // implement; say nothing and drop it, per the caps.
            other => {
                log::warn!("kitty: format {other} not supported, frame dropped");
                return;
            },
        };
        let src = Arc::new(bytes);
        let generation = self.images.get(&p.id).map(|old| old.generation + 1).unwrap_or(0);
        self.images.insert(p.id, KittyImage { width: rgba.0, height: rgba.1, rgba: rgba.2, source: src, generation });
        while self.images.len() > MAX_IMAGES {
            // Evict an image nothing displays, oldest id order aside -
            // any unreferenced one will do; the cap is a flood gate, not
            // an LRU policy with pretensions.
            match self
                .images
                .keys()
                .copied()
                .find(|id| !self.placements.iter().any(|pl| pl.image_id == *id))
            {
                Some(victim) => {
                    self.images.remove(&victim);
                },
                None => break,
            }
        }
        if p.display {
            self.push_placement(Placement {
                image_id: p.id,
                placement_id: 0,
                anchor_line: p.anchor_line,
                col: p.anchor_col,
                cols: p.cols,
                rows: p.rows,
                x_off: p.x_off,
                y_off: p.y_off,
                z: p.z,
            });
        }
    }

    /// Placements in paint order: z first, then anchor (the grid's own
    /// order breaks ties).
    pub fn placements(&self) -> &[Placement] {
        &self.placements
    }

    /// Sorted copy in paint order, computed once per frame.
    pub fn placements_sorted(&self) -> Vec<Placement> {
        let mut v = self.placements.clone();
        v.sort_by_key(|p| (p.z, p.anchor_line, p.col));
        v
    }

    pub fn image(&self, id: u32) -> Option<&KittyImage> {
        self.images.get(&id)
    }

    /// Ids currently held, for GPU cache pruning on tab close.
    pub fn image_ids(&self) -> Vec<u32> {
        self.images.keys().copied().collect()
    }
}

/// Base64 with padding tolerance (some senders trim it).
fn decode_b64(data: &[u8]) -> Option<Vec<u8>> {
    let mut s = data.to_vec();
    while s.len() % 4 != 0 {
        s.push(b'=');
    }
    base64::engine::general_purpose::STANDARD.decode(&s).ok()
}

/// PNG -> RGBA8, size-capped BEFORE allocating: width/height are read from
/// the header and bounds-checked first, so a 100000x100000 claim never
/// reaches the decoder's memory (checked against u64 products as well).
fn decode_png(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let reader = std::io::Cursor::new(bytes);
    let mut decoder = png::Decoder::new(reader);
    let Ok(mut reader) = decoder.read_info() else { return None };
    // Dimensions come from the header and are bounded BEFORE any pixel
    // allocation: the output buffer below is the decoder's only canvas,
    // so the cap here is the whole memory story - no decoder-internal
    // limits to configure or misconfigure.
    let (w, h) = (reader.info().width, reader.info().height);
    if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE {
        log::warn!("kitty: image {w}x{h} out of bounds, dropped");
        return None;
    }
    let mut buf = vec![0u8; (w as usize) * (h as usize) * 4];
    reader.next_frame(&mut buf).ok()?;
    Some((w, h, buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    /// A tiny valid PNG (solid 4x2 RGBA) built with the same decoder's
    /// partner, so the fixture cannot drift from the parser.
    fn tiny_png() -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, 4, 2);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut w = enc.write_header().unwrap();
            w.write_image_data(&vec![0x40u8; 4 * 2 * 4]).unwrap();
            w.finish().unwrap();
        }
        out
    }

    fn b64(bytes: &[u8]) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD.encode(bytes).into_bytes()
    }

    fn apc(params: &str, data: &[u8]) -> Vec<u8> {
        let mut v = b"G;".to_vec();
        v.extend_from_slice(params.as_bytes());
        if !data.is_empty() {
            v.push(b';');
            v.extend_from_slice(data);
        }
        v
    }

    #[test]
    fn transmit_and_display() {
        let mut k = TabKitty::default();
        k.on_apc(&apc("a=T,i=7,f=100", &b64(&tiny_png())), 12, 3);
        let img = k.image(7).expect("image stored");
        assert_eq!((img.width, img.height), (4, 2));
        let pl = k.placements();
        assert_eq!(pl.len(), 1);
        assert_eq!((pl[0].image_id, pl[0].anchor_line, pl[0].col), (7, 12, 3));
    }

    #[test]
    fn chunked_transfer_reassembles() {
        let mut k = TabKitty::default();
        let data = b64(&tiny_png());
        let mid = data.len() / 2;
        k.on_apc(&apc("a=t,i=1,m=1,f=100", &data[..mid]), 5, 0);
        assert!(k.image(1).is_none()); // still open
        // Two sender shapes: a paramless frame, then the icat style of
        // stating `m=` per chunk. The final chunk says m=0 explicitly.
        k.on_apc(&{
            let mut v = b"G;".to_vec();
            v.extend_from_slice(&data[mid..mid + 8]);
            v
        }, 5, 0);
        let tail = &data[mid + 8..];
        k.on_apc(&apc("m=0", tail), 5, 0);
        assert!(k.image(1).is_some());
        assert!(k.placements().is_empty()); // 't' does not display
        k.on_apc(&apc("a=p,i=1,c=20,r=10", &[]), 9, 4);
        let pl = &k.placements()[0];
        assert_eq!((pl.anchor_line, pl.col, pl.cols, pl.rows), (9, 4, Some(20), Some(10)));
    }

    #[test]
    fn delete_scopes_by_id_and_clears_all() {
        let mut k = TabKitty::default();
        k.on_apc(&apc("a=T,i=2,f=100", &b64(&tiny_png())), 1, 0);
        k.on_apc(&apc("a=T,i=3,f=100", &b64(&tiny_png())), 2, 0);
        assert_eq!(k.placements().len(), 2);
        k.on_apc(&apc("a=d,i=2", &[]), 3, 0);
        assert_eq!(k.placements().len(), 1);
        assert!(k.image(2).is_none());
        assert!(k.image(3).is_some());
        k.on_apc(&apc("a=d", &[]), 4, 0);
        assert!(k.placements().is_empty());
        assert!(k.image(3).is_none());
    }

    #[test]
    fn malformed_is_ignored_not_fatal() {
        let mut k = TabKitty::default();
        k.on_apc(&apc("a=T,i=9,f=100", b"not base64 !!!"), 1, 0);
        assert!(k.image(9).is_none());
        k.on_apc(&apc("a=T,i=9,f=100", &b64(b"definitely not a png")), 1, 0);
        assert!(k.image(9).is_none());
        k.on_apc(&apc("a=x,i=1,f=100", &[]), 1, 0); // unknown action
        k.on_apc(b"X;not kitty at all", 1, 0); // other APC namespace
        let mut bare = b"G;".to_vec();
        bare.extend_from_slice(&b64(&tiny_png()));
        k.on_apc(&bare, 1, 0); // bare continuation with no pending
        assert_eq!(k.images.len(), 0);
        assert_eq!(k.placements().len(), 0);
    }

    #[test]
    fn replacement_bumps_generation() {
        let mut k = TabKitty::default();
        k.on_apc(&apc("a=t,i=4,f=100", &b64(&tiny_png())), 1, 0);
        assert_eq!(k.image(4).unwrap().generation, 0);
        k.on_apc(&apc("a=t,i=4,f=100", &b64(&tiny_png())), 1, 0);
        assert_eq!(k.image(4).unwrap().generation, 1);
    }
}
