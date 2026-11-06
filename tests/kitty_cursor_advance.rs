//! The Kitty graphics protocol requires a terminal to move the cursor past a
//! displayed image - right by the placement's columns and down by its rows -
//! so that a prompt printed afterwards lands clear of the image. icat relies
//! on this: it prints one trailing newline and expects the cursor to already
//! have advanced by the placement rectangle. Exercised here against the real
//! `Term` (the vendored crate can't be unit-tested on its own because it
//! depends on the patched `vte`), so the cursor actually moves, not just the
//! parser's decision.

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Handler, Processor};

/// Feed one captured APC string to a fresh 40x10 terminal (with the given
/// pixel cell size) anchored at (line, col) and return the cursor point it
/// ends on. A cell of `(0, 0)` leaves the default `(1, 1)`.
fn cursor_after(anchor: (i32, usize), cell: (u32, u32), payload: &[u8]) -> Point {
    let size = TermSize::new(40, 10); // 40 columns, 10 rows
    let mut term = Term::new(Config::default(), &size, VoidListener);
    if cell != (0, 0) {
        term.set_cell_pixels(cell.0, cell.1);
    }
    term.grid_mut().cursor.point = Point::new(Line(anchor.0), Column(anchor.1));
    term.apc_raw(payload);
    term.grid().cursor.point
}

#[test]
fn an_explicit_rectangle_moves_the_cursor() {
    // c/r stated outright: cell size is irrelevant. Anchor (2,3), rectangle
    // 10x4 -> (6,13).
    assert_eq!(
        cursor_after((2, 3), (10, 20), b"G;a=T,i=1,c=10,r=4"),
        Point::new(Line(6), Column(13))
    );
    // A plain display of a stored image (a=p) advances the same way.
    assert_eq!(
        cursor_after((0, 0), (0, 0), b"G;a=p,i=1,c=5,r=2"),
        Point::new(Line(2), Column(5))
    );
}

#[test]
fn a_native_sized_image_advances_by_its_cells() {
    // icat's normal frame: no c/r, only the pixel size s=200,v=100 and an X
    // offset. In a 10x20 px cell that is 20 columns x 5 rows.
    assert_eq!(
        cursor_after((0, 0), (10, 20), b"G;a=T,q=2,f=100,t=f,s=200,v=100,X=2"),
        Point::new(Line(5), Column(20))
    );
    // The size rounds up to the cells it spans: 201x101 -> 21 x 6.
    assert_eq!(
        cursor_after((1, 1), (10, 20), b"G;a=T,f=100,s=201,v=101"),
        Point::new(Line(7), Column(22))
    );
}

#[test]
fn right_clamps_and_down_scrolls() {
    // Near the bottom-right corner: 35+10 clamps to the last column (39), and
    // 4 line feeds scroll the screen at the bottom, leaving the cursor on the
    // last row (9).
    let p = cursor_after((8, 35), (0, 0), b"G;a=T,i=1,c=10,r=4");
    assert_eq!(p.column, Column(39));
    assert_eq!(p.line, Line(9));
}

/// The exact tail of a real `kitty + icat` run (captured under a pty): save
/// cursor, reset scroll region, restore, carriage return, cursor forward 23,
/// the display frame (native size - `s=200,v=100,X=2`, no `c`/`r`), then the
/// trailing `\r\n` icat prints. Fed through the real vte parser into a real
/// 80x24 terminal with a 10x20 px cell (icat's 800x480 / 80x24 assumption).
#[test]
fn real_icat_stream_leaves_the_prompt_clear_of_the_image() {
    let bytes = b"\x1b7\x1b[?r\x1b8\r\x1b[23C\x1b_Ga=T,q=2,f=100,t=f,s=200,v=100,X=2;L3RtcC9lMmUucG5n\x1b\\\r\n";
    let size = TermSize::new(80, 24);
    let mut term = Term::new(Config::default(), &size, VoidListener);
    term.set_cell_pixels(10, 20);
    // The prompt sat at row 10 when icat ran.
    term.grid_mut().cursor.point = Point::new(Line(10), Column(0));
    let mut proc: Processor = Processor::default();
    proc.advance(&mut term, bytes);

    // The image is 100px tall = 5 cells, so it occupies rows 10..14. The
    // cursor must end past it: down 5 for the image plus the trailing LF =
    // row 16, back at column 0. (Without the fix it lands on row 11, inside
    // the image - the prompt-overlap bug.)
    assert_eq!(term.grid().cursor.point, Point::new(Line(16), Column(0)));
}

#[test]
fn no_movement_cases_leave_the_cursor() {
    let anchor = Point::new(Line(2), Column(3));
    let payloads: &[&[u8]] = &[
        b"G;a=t,i=1,c=10,r=4",        // transmit-only: stored, never displayed
        b"G;a=T,i=1,c=10,r=4,C=1",    // explicit no-movement policy
        b"G;a=T,i=1,c=10,r=3,p=4",    // relative placement: anchored elsewhere
        b"G;a=d,i=1,c=10,r=3",        // delete
        b"G;a=T,i=1,c=10",            // missing rows: rectangle unknown
        b"G;a=T,i=1",                 // no c/r and no s/v: rectangle unknown
        b"q;some other dialect",      // not a Kitty graphics frame
    ];
    for payload in payloads {
        assert_eq!(
            cursor_after((2, 3), (10, 20), payload),
            anchor,
            "payload {payload:?}"
        );
    }
}
