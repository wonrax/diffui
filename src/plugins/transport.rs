//! Transport plugins as the app uses them: which are running, the hosts
//! each finds, and a path on this machine put to the one that claims it.
//!
//! The asking that happens on the core's threads (how to reach a host, what
//! a failed start means) is `crate::remote`'s connector; this keeps the
//! directory it reads up to date.

use diffui_plugins::protocol::{FoundHost, Located, PluginCall, TransportCall};

use super::{FromPlugin, State};
use crate::remote::{Transport, lock, read_hosts};
use crate::{Cx, Diffui};

impl Diffui {
    /// Make the directory's hosts the config's as it stands, and its
    /// transports the plugins that are running or loading. Returns what is
    /// wrong with the host tables.
    pub(crate) fn refresh_hosts(&mut self) {
        let transports = self
            .plugins
            .found
            .iter()
            .filter_map(|found| {
                if !matches!(found.state, State::Running(_) | State::Loading) {
                    return None;
                }
                let manifest = found.manifest.clone()?;
                manifest.transport.as_ref()?;
                let plugin = found.running.as_ref()?.plugin.clone();
                let settings = manifest.resolve_config(&self.plugin_config(&found.id)).0;
                let transport = Transport {
                    name: found.name.clone(),
                    plugin,
                    manifest,
                    settings,
                };
                Some((found.id.clone(), transport))
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        let (hosts, problems) = read_hosts(&self.host_tables());
        let mut directory = lock(&self.remote.directory);
        directory.found.retain(|id, _| transports.contains_key(id));
        directory.transports = transports;
        directory.hosts = hosts;
        drop(directory);
        self.remote.problems = problems;
    }

    /// Ask the transport `id` which hosts it finds here, if it can say.
    pub(super) fn ask_transport_hosts(&mut self, id: &str) {
        let Some(found) = self.plugins.get(id) else {
            return;
        };
        let (Some(running), State::Running(registration)) = (&found.running, &found.state) else {
            return;
        };
        let Some(methods) = &registration.transport else {
            return;
        };
        if !methods.iter().any(|method| method == "hosts") {
            // It finds none: nothing to wait for.
            lock(&self.remote.directory)
                .found
                .entry(id.to_owned())
                .or_default();
            return;
        }
        let (to, plugin, instance) = (self.plugins.to_app.clone(), id.to_owned(), running.instance);
        running.plugin.call::<Vec<FoundHost>>(
            PluginCall::Transport {
                call: TransportCall::Hosts,
            },
            move |result| match result {
                Ok(hosts) => {
                    let _ = to.unbounded_send(FromPlugin::TransportHosts {
                        plugin,
                        instance,
                        hosts,
                    });
                }
                Err(error) => tracing::warn!(plugin, "transport hosts() failed: {error}"),
            },
        );
    }

    /// Ask every transport again: what it finds can change while the app
    /// runs (a distribution installed, a `Host` added).
    pub(crate) fn refresh_found_hosts(&mut self) {
        let ids: Vec<String> = lock(&self.remote.directory)
            .transports
            .keys()
            .cloned()
            .collect();
        for id in ids {
            self.ask_transport_hosts(&id);
        }
    }

    pub(super) fn transport_hosts_answered(&mut self, plugin: &str, hosts: Vec<FoundHost>) {
        lock(&self.remote.directory)
            .found
            .insert(plugin.to_owned(), hosts);
    }

    /// Put `path`, a path on this machine, to the transport whose
    /// `local_roots` it is under (a WSL distribution's files as Explorer
    /// names them). `false` when no transport claims it; else the answer
    /// opens it, there or here.
    pub(crate) fn locate_on_host(&mut self, path: &str) -> bool {
        let folded = path.to_lowercase();
        let claimed = lock(&self.remote.directory)
            .transports
            .iter()
            .find(|(_, transport)| {
                transport.manifest.transport.as_ref().is_some_and(|claim| {
                    claim
                        .local_roots
                        .iter()
                        .any(|root| folded.starts_with(&root.to_lowercase()))
                })
            })
            .map(|(id, transport)| (id.clone(), transport.plugin.clone()));
        let Some((plugin, running)) = claimed else {
            return false;
        };
        let (to, path) = (self.plugins.to_app.clone(), path.to_owned());
        running.call::<Option<Located>>(
            PluginCall::Transport {
                call: TransportCall::Locate { path: path.clone() },
            },
            move |result| {
                let located = result.unwrap_or_else(|error| {
                    tracing::warn!(plugin, "transport locate() failed: {error}");
                    None
                });
                let _ = to.unbounded_send(FromPlugin::Located {
                    plugin,
                    path,
                    located,
                });
            },
        );
        true
    }

    pub(super) fn located(
        &mut self,
        plugin: &str,
        path: String,
        located: Option<Located>,
        cx: &mut Cx,
    ) {
        match located {
            Some(located) => {
                lock(&self.remote.directory).note_found(
                    plugin,
                    FoundHost {
                        name: located.host.clone(),
                        options: located.options,
                    },
                );
                self.open_remote(
                    diffui_core::remote::RemoteTarget {
                        host: diffui_core::remote::HostId(located.host),
                        path: located.path,
                    },
                    cx,
                );
            }
            // Not on a host after all: it is the path it looks like.
            None => self.open_local_repository(&path, cx),
        }
    }
}
