use std::borrow::Cow;
use std::fmt::{self, Debug, Formatter};
use std::process::ExitStatus;
use std::sync::Arc;

use crate::term::ClipboardType;
use crate::vte::ansi::Rgb;

/// Terminal event.
///
/// These events instruct the UI over changes that can't be handled by the terminal emulation layer
/// itself.
#[derive(Clone)]
pub enum Event {
    /// Grid has changed possibly requiring a mouse cursor shape change.
    MouseCursorDirty,

    /// Window title change.
    Title(String),

    /// Reset to the default window title.
    ResetTitle,

    /// Request to store a text string in the clipboard.
    ClipboardStore(ClipboardType, String),

    /// Request to write the contents of the clipboard to the PTY.
    ///
    /// The attached function is a formatter which will correctly transform the clipboard content
    /// into the expected escape sequence format.
    ClipboardLoad(ClipboardType, Arc<dyn Fn(&str) -> String + Sync + Send + 'static>),

    /// Request to write the RGB value of a color to the PTY.
    ///
    /// The attached function is a formatter which will correctly transform the RGB color into the
    /// expected escape sequence format.
    ColorRequest(usize, Arc<dyn Fn(Rgb) -> String + Sync + Send + 'static>),

    /// Write some text to the PTY.
    PtyWrite(String),

    /// Request to write the text area size.
    TextAreaSizeRequest(Arc<dyn Fn(WindowSize) -> String + Sync + Send + 'static>),

    /// Cursor blinking state has changed.
    CursorBlinkingChange,

    /// New terminal content available.
    Wakeup,

    /// Terminal bell ring.
    Bell,

    /// Shutdown request.
    Exit,

    /// Child process exited.
    ChildExit(ExitStatus),

    /// OSC 133 shell semantic mark, captured AT DISPATCH TIME with the
    /// terminal's own cursor (mypixels patch; docs/graphics_osc133_
    /// implementation.md M1). `line` is an absolute logical line:
    /// scrollback size plus viewport line at the moment the sequence was
    /// parsed - the only moment that position is guaranteed true.
    /// mypixels: code 0=A prompt start, 1=B prompt end, 2=C command start,
    /// 3=D command end (with exit status when the shell sent one).
    /// mypixels further: `row` is a row ID that survives scrollback
    /// eviction (scrolled-out count + viewport row), and `alt` says the
    /// alternate screen owned the display at dispatch - output under a
    /// TUI is not command output to capture.
    Semantic {
        code: u8,
        exit: Option<i32>,
        line: usize,
        col: usize,
        row: usize,
        alt: bool,
    },

    /// One complete APC string (mypixels patch; the Kitty graphics
    /// protocol). Carries the cursor as an ABSOLUTE line + column at the
    /// instant the sequence ended - the placement anchor, which must be
    /// taken here and never reconstructed later.
    Apc {
        payload: Vec<u8>,
        line: usize,
        col: usize,
    },

    /// CSI 16 t: the application asks the window for the cell size in
    /// pixels. Only the app knows that - it is broot (and sixel detection)
    /// asking how to scale an image, answered from the live font metrics.
    CellSizeRequest,

    /// RIS (`ESC c`) executed: a full reset. The Kitty graphics store must
    /// die with everything else on the screen - a `reset` that leaves a
    /// picture burned in is not a reset.
    GraphicsReset,
}

impl Debug for Event {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Event::ClipboardStore(ty, text) => write!(f, "ClipboardStore({ty:?}, {text})"),
            Event::ClipboardLoad(ty, _) => write!(f, "ClipboardLoad({ty:?})"),
            Event::TextAreaSizeRequest(_) => write!(f, "TextAreaSizeRequest"),
            Event::ColorRequest(index, _) => write!(f, "ColorRequest({index})"),
            Event::PtyWrite(text) => write!(f, "PtyWrite({text})"),
            Event::Title(title) => write!(f, "Title({title})"),
            Event::CursorBlinkingChange => write!(f, "CursorBlinkingChange"),
            Event::MouseCursorDirty => write!(f, "MouseCursorDirty"),
            Event::ResetTitle => write!(f, "ResetTitle"),
            Event::Wakeup => write!(f, "Wakeup"),
            Event::Bell => write!(f, "Bell"),
            Event::Semantic { .. } => write!(f, "Semantic"),
            Event::Apc { payload, .. } => write!(f, "Apc({} bytes)", payload.len()),
            Event::GraphicsReset => write!(f, "GraphicsReset"),
            Event::CellSizeRequest => write!(f, "CellSizeRequest"),
            Event::Exit => write!(f, "Exit"),
            Event::ChildExit(status) => write!(f, "ChildExit({status:?})"),
        }
    }
}

/// Byte sequences are sent to a `Notify` in response to some events.
pub trait Notify {
    /// Notify that an escape sequence should be written to the PTY.
    ///
    /// TODO this needs to be able to error somehow.
    fn notify<B: Into<Cow<'static, [u8]>>>(&self, _: B);
}

#[derive(Copy, Clone, Debug)]
pub struct WindowSize {
    pub num_lines: u16,
    pub num_cols: u16,
    pub cell_width: u16,
    pub cell_height: u16,
}

/// Types that are interested in when the display is resized.
pub trait OnResize {
    fn on_resize(&mut self, window_size: WindowSize);
}

/// Event Loop for notifying the renderer about terminal events.
pub trait EventListener {
    fn send_event(&self, _event: Event) {}
}

/// Null sink for events.
pub struct VoidListener;

impl EventListener for VoidListener {}
