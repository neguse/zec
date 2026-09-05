//! The terminal boundary: raw mode and restoration, capability detection, the
//! blocking input reader, cell rendering, prompts, pickers, and the clipboard.
//! Nothing here knows about Zed or about the application state.

pub mod capabilities;
pub mod clipboard;
pub mod keys;
pub mod picker;
pub mod prompt;
pub mod reader;
pub mod render;
pub mod session;

pub use capabilities::Capabilities;
pub use reader::{Event, InputReader, ResizeAcknowledgement, ScrollDirection};
pub use session::{Session, Terminal, is_suspend_signal, suspend_and_resume};
