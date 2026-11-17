//! A repository on another machine, as an actor here.
//!
//! The real actor runs in `diffui-serve` over there; this one relays. It
//! opens a channel on the host's link, turns the server's answer into the
//! `Ready` a local actor sends at once, then passes commands up and events
//! down unchanged.
//!
//! When the link goes it fails every job still out, says why once
//! ([`Payload::Disconnected`]) and fails whatever is sent after. It doesn't
//! reconnect: the frontend opens the repository again, which gets a new
//! link.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;

use super::{Envelope, RemoteRepository, RepoHandle};
use crate::remote::link::ChannelEvent;
use crate::remote::wire::Up;
use crate::remote::{ConnectError, HostLink, LinkState};
use crate::repo::{Command, Event, JobId, Payload, RepoError, RepoId};
use crate::repository::Repository;

pub(super) fn spawn(
    id: RepoId,
    link: HostLink,
    path: String,
    mut commands: mpsc::UnboundedReceiver<Envelope>,
    tx: mpsc::UnboundedSender<Envelope>,
    events: mpsc::UnboundedSender<Event>,
) {
    tokio::spawn(async move {
        let emit = |payload| {
            let _ = events.send(Event::new(id.clone(), payload));
        };
        let (channel, mut incoming) = link.open(path);

        // Until the server answers, the link's progress is the tab's.
        let mut state = link.state();
        let mut watching = true;
        let answer = loop {
            tokio::select! {
                event = incoming.recv() => match event {
                    Some(ChannelEvent::Opened(opened)) => break Ok(opened),
                    Some(ChannelEvent::Refused(message)) => {
                        break Err(ConnectError::Refused(message));
                    }
                    Some(ChannelEvent::Event(_)) => {}
                    None => break Err(link.down_reason()),
                },
                changed = state.changed(), if watching => {
                    watching = changed.is_ok();
                    if let LinkState::Connecting { detail } = &*state.borrow_and_update() {
                        emit(Payload::Connecting { detail: detail.clone() });
                    }
                }
                envelope = commands.recv() => {
                    if matches!(envelope, None | Some(Envelope::Shutdown)) {
                        link.close(channel);
                        return;
                    }
                }
            }
        };
        let answer = match answer {
            Ok(opened) => link.ready().await.map(|info| (opened, info)),
            Err(error) => Err(error),
        };
        let (opened, info) = match answer {
            Ok(answer) => answer,
            Err(error) => {
                emit(Payload::Disconnected { error });
                fail_the_rest(commands, &emit).await;
                return;
            }
        };

        let capabilities = opened.capabilities;
        let handle = RepoHandle {
            id: id.clone(),
            repository: Repository {
                root: PathBuf::from(&opened.root),
                vcs: opened.vcs,
                scope: PathBuf::from(&opened.scope),
            },
            capabilities,
            tx,
            remote: Some(Arc::new(RemoteRepository {
                host: link.host().clone(),
                info,
                opened,
                link: link.clone(),
            })),
        };
        emit(Payload::Ready {
            handle,
            capabilities,
        });

        let mut out: BTreeSet<JobId> = BTreeSet::new();
        loop {
            tokio::select! {
                envelope = commands.recv() => match envelope {
                    None | Some(Envelope::Shutdown) => {
                        link.close(channel);
                        return;
                    }
                    // The watch runs over there, with the real actor.
                    Some(Envelope::Watch(_)) => {}
                    Some(Envelope::Run { repository, command }) => {
                        if let Some(job) = command.job() {
                            out.insert(job);
                        }
                        link.send(Up::Run {
                            channel,
                            scope: wire_path(&repository.scope),
                            command,
                        });
                    }
                },
                event = incoming.recv() => match event {
                    Some(ChannelEvent::Event(payload)) => {
                        if payload.is_terminal()
                            && let Some(job) = payload.job()
                        {
                            out.remove(&job);
                        }
                        emit(*payload);
                    }
                    Some(ChannelEvent::Opened(_) | ChannelEvent::Refused(_)) => {}
                    None => break,
                },
            }
        }

        for job in out {
            emit(Payload::Failed {
                job,
                error: RepoError::Disconnected,
            });
        }
        emit(Payload::Disconnected {
            error: link.down_reason(),
        });
        // The link is no use now; let its program go while the tab decides.
        drop(link);
        fail_the_rest(commands, &emit).await;
    });
}

/// Answer every command sent to a repository that can't be reached, until
/// its tab lets go.
async fn fail_the_rest(mut commands: mpsc::UnboundedReceiver<Envelope>, emit: &impl Fn(Payload)) {
    while let Some(envelope) = commands.recv().await {
        match envelope {
            Envelope::Shutdown => return,
            Envelope::Watch(_) => {}
            Envelope::Run { command, .. } => {
                if let Command::Cancel { .. } = *command {
                    continue;
                }
                if let Some(job) = command.job() {
                    emit(Payload::Failed {
                        job,
                        error: RepoError::Disconnected,
                    });
                }
            }
        }
    }
}

/// A path under the repository's root as the server reads it. A Windows app
/// builds one with backslashes; the server's side of the wire is `/`.
fn wire_path(path: &std::path::Path) -> String {
    let path = path.to_string_lossy();
    if cfg!(windows) {
        path.replace('\\', "/")
    } else {
        path.into_owned()
    }
}
