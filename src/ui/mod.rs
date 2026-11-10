//! Setup and Settings windows. They run in a separate process
//! (`murmur --ui setup|settings`) so the daemon never loads a GUI toolkit
//! while idle; see `protocol` for the pipe protocol.

pub mod protocol;
