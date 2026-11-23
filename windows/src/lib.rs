//! The parts of the app that stand on their own and have test programs of their own
//! (src/bin/): sound capture, the link to the engine, and the door for the Chrome extension.
pub mod capture;
pub mod engine;
mod net;
pub mod tabaudio;
