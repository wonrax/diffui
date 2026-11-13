//! The host functions that need no app state: they run here, off the
//! plugin's script thread, under the manifest's permissions.

pub(crate) mod auth;
pub(crate) mod exec;
pub(crate) mod http;
pub(crate) mod kv;
