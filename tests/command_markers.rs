//! Command input must be copied in the parser, before a fast command can
//! overwrite the grid, and must use cell positions rather than byte offsets.
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;

#[derive(Clone, Default)]
struct Marks(Arc<Mutex<Vec<(u8, Option<i32>, Option<String>)>>>);

impl EventListener for Marks {
    fn send_event(&self, event: Event) {
        if let Event::Semantic { code, exit, command, .. } = event {
            self.0.lock().unwrap().push((code, exit, command));
        }
    }
}

fn commands(bytes: &[u8], columns: usize) -> Vec<(u8, Option<i32>, Option<String>)> {
    let marks = Marks::default();
    let mut term = Term::new(Config::default(), &TermSize::new(columns, 8), marks.clone());
    let mut parser: Processor = Processor::default();
    // A PTY can split markers or Unicode characters across any read boundary.
    for byte in bytes {
        parser.advance(&mut term, &[*byte]);
    }
    let result = marks.0.lock().unwrap().clone();
    result
}

#[test]
fn input_is_captured_before_output_clears_the_screen() {
    let stream = concat!(
        "\x1b]133;A\x07project\r\n界 ❯ \x1b]133;B\x07",
        "printf 'héllo'\r\n\x1b]133;C\x07",
        "\x1b[2J\x1b[Hhéllo\r\n\x1b]133;D;0\x07",
        "\x1b]133;A\x07next prompt\x1b]133;B\x07",
    );
    let marks = commands(stream.as_bytes(), 80);
    assert_eq!(marks.iter().filter(|m| m.0 == 2).count(), 1);
    assert_eq!(marks.iter().find(|m| m.0 == 2).unwrap().2.as_deref(), Some("printf 'héllo'"));
    assert_eq!(marks.iter().find(|m| m.0 == 3).unwrap().1, Some(0));
    assert!(marks.last().unwrap().2.is_none(), "an idle prompt has no command");
}

#[test]
fn wrapped_input_and_wide_characters_keep_the_whole_command() {
    let command = "printf '%s' '界 e\u{301} abcdefghijklmnopqrstuvwxyz'";
    let stream = format!("\x1b]133;A\x07λ> \x1b]133;B\x07{command}\r\n\x1b]133;C\x07\x1b]133;D;7\x07");
    let marks = commands(stream.as_bytes(), 20);
    assert_eq!(marks.iter().find(|m| m.0 == 2).unwrap().2.as_deref(), Some(command));
    assert_eq!(marks.iter().find(|m| m.0 == 3).unwrap().1, Some(7));
}

#[test]
fn command_can_end_on_the_same_row_as_the_execution_marker() {
    let marks = commands(b"\x1b]133;A\x07$ \x1b]133;B\x07echo ok\x1b]133;C\x07", 80);
    assert_eq!(marks.last().unwrap().2.as_deref(), Some("echo ok"));
}

#[test]
fn stale_or_missing_input_anchors_do_not_capture_unrelated_text() {
    let stream = b"\x1b]133;A\x07$ \x1b]133;B\x07old input\r\n\x1b]133;D;0\x07\x1b]133;C\x07";
    assert!(commands(stream, 80).last().unwrap().2.is_none());
}
