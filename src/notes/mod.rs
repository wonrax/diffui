//! Review notes: what an agent leaves on its changes from its own shell
//! (`diffui notes`, see [`cli`]), read and answered inline in the diff.
//!
//! A note is a [`store::Thread`]: anchored to a change id, optionally a file
//! and lines of it, with the agent's message and the replies after it.
//! [`locate`] finds where each one sits after the change was amended or
//! rebased.

pub(crate) mod app;
pub(crate) mod cli;
pub(crate) mod inline;
pub(crate) mod locate;
pub(crate) mod review;
pub(crate) mod store;
pub(crate) mod view;

pub(crate) use app::{NoteReply, NotesMsg, TabNotes};
#[cfg(test)]
pub(crate) mod scenarios;
