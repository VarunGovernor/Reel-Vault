//! Library root: the server modules are public so integration tests
//! (and a later packaged shell) can build the same router as the binary.

pub mod api;
pub mod datadir;
pub mod db;
pub mod import;
pub mod ollama;
pub mod security;
