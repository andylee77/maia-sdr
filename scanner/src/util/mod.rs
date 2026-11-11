//! Small shared helpers: time stamps, atomic file writes, serde helpers, a ZIP archive written
//! as a stream, and the hand-off of a response's chunks from the thread that makes them.

pub mod atomic_file;
pub mod handoff;
pub mod ser;
pub mod time;
pub mod zip;
