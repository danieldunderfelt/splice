use crate::net::TsApi;
use serde::{Deserialize, Serialize};
use splice_proto::MachineId;
use splice_update::{control::Action, Host, UpdateState};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiUpdate {
    pub state: Option<UpdateState>,
    pub error: Option<String>,
    pub expected_version: Option<String>,
    pub requested_ms: Option<u64>,
}

pub(crate) struct Updates {
    host: Host,
    self_id: MachineId,
    local: IpAddr,
    peers: HashMap<MachineId, IpAddr>,
    state: BTreeMap<MachineId, UiUpdate>,
    pending: HashSet<MachineId>,
    queued: HashMap<MachineId, Action>,
    local_error: Option<String>,
    jobs: tokio::task::JoinSet<(MachineId, anyhow::Result<UpdateState>)>,
    server: tokio::task::JoinHandle<()>,
    limit: Arc<tokio::sync::Semaphore>,
}

impl Updates {
    pub async fn new(host: Host, self_id: MachineId, local: IpAddr, ts: Arc<dyn TsApi>) -> Self {
        let listener =
            tokio::net::TcpListener::bind(SocketAddr::new(local, splice_update::control::PORT))
                .await;
        let server_host = host.clone();
        let machine = self_id.clone();
        let server = tokio::spawn(async move {
            let listener = match listener {
                Ok(listener) => listener,
                Err(error) => {
                    server_host.fail(format!(
                        "Cannot listen for remote updates on port 41718: {error}"
                    ));
                    return;
                }
            };
            if let Err(error) = server_host.confirm_running() {
                server_host.fail(format!("Update startup confirmation failed: {error:#}"));
            }
            let authorize: splice_update::control::Authorize = Arc::new(move |address| {
                let ts = ts.clone();
                Box::pin(async move {
                    let (Ok(status), Ok(who)) = tokio::join!(ts.status(), ts.whois(address)) else {
                        return false;
                    };
                    matches!(
                        splice_tailscale::authorize(&status, &who),
                        splice_tailscale::AuthDecision::Peer(_)
                    )
                })
            });
            if let Err(error) =
                splice_update::control::serve(listener, machine, server_host.clone(), authorize)
                    .await
            {
                server_host.fail(format!("Remote update listener failed: {error:#}"));
            }
        });
        Self {
            host,
            self_id,
            local,
            peers: HashMap::new(),
            state: BTreeMap::new(),
            pending: HashSet::new(),
            queued: HashMap::new(),
            local_error: None,
            jobs: tokio::task::JoinSet::new(),
            server,
            limit: Arc::new(tokio::sync::Semaphore::new(8)),
        }
    }

    pub fn discover(&mut self, peers: &[(MachineId, IpAddr)]) {
        self.peers = peers.iter().cloned().collect();
        self.state.retain(|id, _| self.peers.contains_key(id));
        self.queued.retain(|id, _| self.peers.contains_key(id));
        for (id, _) in peers {
            self.request(id.clone(), Action::Status);
        }
    }

    pub fn request(&mut self, id: MachineId, action: Action) {
        if id == self.self_id {
            self.local_error = self
                .host
                .request(action)
                .err()
                .map(|error| format!("{error:#}"));
            return;
        }
        let entry = self.state.entry(id.clone()).or_default();
        let Some(ip) = self.peers.get(&id).copied() else {
            entry.error = Some("This computer is not an online authorized Tailnet peer".into());
            return;
        };
        if self.pending.contains(&id) {
            if action != Action::Status {
                if let std::collections::hash_map::Entry::Vacant(queued) = self.queued.entry(id) {
                    queued.insert(action);
                } else {
                    entry.error = Some("An update request is already queued".into());
                }
            }
            return;
        }
        if let Action::Install { version } = &action {
            entry.expected_version = Some(version.clone());
            entry.requested_ms = Some(crate::diagnostics::unix_ms());
        }
        self.pending.insert(id.clone());
        let local = self.local;
        let limit = self.limit.clone();
        self.jobs.spawn(async move {
            let _permit = limit
                .acquire_owned()
                .await
                .expect("update request semaphore stays open");
            let result = splice_update::control::request(
                local,
                SocketAddr::new(ip, splice_update::control::PORT),
                &id,
                action,
            )
            .await;
            (id, result)
        });
    }

    pub fn poll(&mut self) {
        self.host.refresh_result();
        while let Some(result) = self.jobs.try_join_next() {
            match result {
                Ok((id, result)) => {
                    self.pending.remove(&id);
                    if !self.peers.contains_key(&id) {
                        self.queued.remove(&id);
                        continue;
                    }
                    let entry = self.state.entry(id.clone()).or_default();
                    match result {
                        Ok(state) => {
                            if state.phase == splice_update::Phase::Failed
                                || entry
                                    .expected_version
                                    .as_ref()
                                    .is_some_and(|v| *v == state.build.version)
                            {
                                entry.expected_version = None;
                                entry.requested_ms = None;
                            }
                            entry.state = Some(state);
                            entry.error = None;
                        }
                        Err(error) => {
                            entry.error = Some(format!("Update connection failed: {error:#}"))
                        }
                    }
                    if let Some(action) = self.queued.remove(&id) {
                        self.request(id, action);
                    }
                }
                Err(error) => self
                    .host
                    .fail(format!("Update request task failed: {error}")),
            }
        }
        let active: Vec<_> = self
            .state
            .iter()
            .filter(|(_, entry)| {
                entry.expected_version.is_some()
                    || entry.state.as_ref().is_some_and(|s| {
                        matches!(
                            s.phase,
                            splice_update::Phase::Checking
                                | splice_update::Phase::Downloading
                                | splice_update::Phase::Restarting
                        )
                    })
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in active {
            self.request(id, Action::Status);
        }
        for entry in self.state.values_mut() {
            if entry
                .requested_ms
                .is_some_and(|start| crate::diagnostics::unix_ms().saturating_sub(start) > 90_000)
            {
                entry.error = Some(format!(
                    "Could not confirm that Splice {} started on this computer",
                    entry
                        .expected_version
                        .as_deref()
                        .unwrap_or("requested version")
                ));
                entry.expected_version = None;
                entry.requested_ms = None;
            }
        }
    }

    pub fn snapshot(&self) -> BTreeMap<MachineId, UiUpdate> {
        let mut state = self.state.clone();
        state.insert(
            self.self_id.clone(),
            UiUpdate {
                state: Some(self.host.state().borrow().clone()),
                error: self.local_error.clone(),
                ..Default::default()
            },
        );
        state
    }

    pub fn restart_requested(&self) -> bool {
        *self.host.restart().borrow()
    }
}

impl Drop for Updates {
    fn drop(&mut self) {
        self.server.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_queued_install_is_discarded_when_the_peer_disappears() {
        let directory = tempfile::tempdir().unwrap();
        let peer = MachineId("peer".into());
        let ip = "127.0.0.1".parse().unwrap();
        let mut updates = Updates {
            host: Host::new(directory.path()).unwrap(),
            self_id: MachineId("self".into()),
            local: ip,
            peers: HashMap::from([(peer.clone(), ip)]),
            state: BTreeMap::new(),
            pending: HashSet::from([peer.clone()]),
            queued: HashMap::new(),
            local_error: None,
            jobs: tokio::task::JoinSet::new(),
            server: tokio::spawn(std::future::pending()),
            limit: Arc::new(tokio::sync::Semaphore::new(8)),
        };
        updates.request(
            peer.clone(),
            Action::Install {
                version: "9.0.0".into(),
            },
        );
        assert!(updates.queued.contains_key(&peer));
        updates.discover(&[]);
        assert!(!updates.queued.contains_key(&peer));
        updates.discover(&[(peer.clone(), ip)]);
        let result_id = peer.clone();
        let state = updates.host.state().borrow().clone();
        updates.jobs.spawn(async move { (result_id, Ok(state)) });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while updates.pending.contains(&peer) {
                tokio::task::yield_now().await;
                updates.poll();
            }
        })
        .await
        .unwrap();
        assert!(updates.queued.is_empty());
        assert!(updates.jobs.is_empty());
        assert!(updates.state[&peer].expected_version.is_none());
    }
}
