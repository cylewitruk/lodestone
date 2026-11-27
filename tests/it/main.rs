//! Integration tests, as one binary.

mod daemon;
#[cfg(unix)]
mod doctor;
mod external_grammar;
mod invariants;
mod transcripts;
