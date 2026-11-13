//! The plugin runtime from the outside: real plugin directories, a fake
//! [`diffui_plugins::Host`], and a local HTTP server standing in for a forge.

mod forgejo;
mod host_api;
mod lifecycle;
mod support;
mod types;
