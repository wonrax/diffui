//! The crate's integration tests, built as one binary: every test file is a
//! module here, so a change to the crate links one test executable rather
//! than one per file. Scratch repos are named after their test and the
//! modules now run concurrently, so a fixture name must be unique across all
//! of them, not only within its file.

mod harness;

mod fetch_parity;
mod git_scenarios;
mod history;
mod hunk_scenarios;
mod jj_scenarios;
mod push_scenarios;
mod repo_actor;
mod revset_scenarios;
mod watch_scenarios;
