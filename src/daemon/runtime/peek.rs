use std::collections::{BTreeMap, BTreeSet};

use crate::pane_state::{ClientWitness, PaneInstance};

use super::CanonicalCoordinatorState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PeekLease {
    Pending {
        operation_seq: u64,
        source_pane: PaneInstance,
        previous_target: Option<PaneInstance>,
        candidates: BTreeSet<PaneInstance>,
    },
    Active {
        target: PaneInstance,
        last_observation_seq: u64,
    },
}

impl PeekLease {
    fn protects(&self, pane: &PaneInstance) -> bool {
        match self {
            Self::Pending {
                previous_target,
                candidates,
                ..
            } => previous_target.as_ref() == Some(pane) || candidates.contains(pane),
            Self::Active { target, .. } => target == pane,
        }
    }
}

impl CanonicalCoordinatorState {
    pub fn active_peek_target(&self, client_pid: u32) -> Option<&PaneInstance> {
        match self.peek_leases.get(&client_pid) {
            Some(PeekLease::Active { target, .. }) => Some(target),
            _ => None,
        }
    }

    pub fn begin_peek(
        &mut self,
        client_pid: u32,
        source_pane: PaneInstance,
        candidates: impl IntoIterator<Item = PaneInstance>,
        operation_seq: u64,
    ) -> bool {
        if matches!(
            self.peek_leases.get(&client_pid),
            Some(PeekLease::Pending { .. })
        ) {
            return false;
        }
        let previous_target = match self.peek_leases.get(&client_pid) {
            Some(PeekLease::Active { target, .. }) => Some(target.clone()),
            Some(PeekLease::Pending { .. }) => unreachable!("pending peek was rejected above"),
            None => None,
        };
        self.peek_leases.insert(
            client_pid,
            PeekLease::Pending {
                operation_seq,
                source_pane,
                previous_target: previous_target.clone(),
                candidates: candidates.into_iter().collect(),
            },
        );
        true
    }

    pub fn activate_peek(
        &mut self,
        client_pid: u32,
        operation_seq: u64,
        target: PaneInstance,
        witness_observation_floor: u64,
    ) {
        let matches_pending = match self.peek_leases.get(&client_pid) {
            Some(PeekLease::Pending {
                operation_seq: pending,
                ..
            }) => *pending == operation_seq,
            _ => false,
        };
        if matches_pending && self.contains_pane(&target) {
            self.peek_leases.insert(
                client_pid,
                PeekLease::Active {
                    target,
                    last_observation_seq: witness_observation_floor,
                },
            );
        }
    }

    pub fn renew_active_peek(
        &mut self,
        client_pid: u32,
        target: &PaneInstance,
        witness_observation_floor: u64,
    ) -> bool {
        let matches_active = matches!(
            self.peek_leases.get(&client_pid),
            Some(PeekLease::Active { target: active, .. }) if active == target
        );
        if matches_active && self.contains_pane(target) {
            self.peek_leases.insert(
                client_pid,
                PeekLease::Active {
                    target: target.clone(),
                    last_observation_seq: witness_observation_floor,
                },
            );
            true
        } else {
            false
        }
    }

    pub fn restore_peek_after_failure(
        &mut self,
        client_pid: u32,
        operation_seq: u64,
        witnesses: &[ClientWitness],
        observation_seq: u64,
    ) {
        let previous = match self.peek_leases.get(&client_pid) {
            Some(PeekLease::Pending {
                operation_seq: pending,
                previous_target,
                ..
            }) if *pending == operation_seq => previous_target.clone(),
            _ => return,
        };
        let fresh_active = witnesses
            .iter()
            .find(|witness| witness.client_pid == client_pid && witness.is_eligible())
            .map(|witness| &witness.active_pane);
        match previous.filter(|pane| {
            self.contains_pane(pane) && fresh_active.is_some_and(|active| active == pane)
        }) {
            Some(target) => {
                self.peek_leases.insert(
                    client_pid,
                    PeekLease::Active {
                        target,
                        last_observation_seq: observation_seq,
                    },
                );
            }
            None => {
                self.peek_leases.remove(&client_pid);
            }
        }
    }

    pub fn clear_peek(&mut self, client_pid: u32) {
        self.peek_leases.remove(&client_pid);
    }

    pub fn clear_peeks_for_read_panes(&mut self, panes: &BTreeSet<PaneInstance>) {
        self.clear_peeks_for_read_panes_except(panes, None);
    }

    pub fn clear_peeks_for_read_panes_except(
        &mut self,
        panes: &BTreeSet<PaneInstance>,
        excluded_client_pid: Option<u32>,
    ) {
        self.peek_leases.retain(|client_pid, lease| {
            excluded_client_pid == Some(*client_pid)
                || !panes.iter().any(|pane| lease.protects(pane))
        });
    }

    pub fn reconcile_peek_leases(&mut self, witnesses: &[ClientWitness], observation_seq: u64) {
        let eligible = witnesses
            .iter()
            .filter(|witness| witness.is_eligible())
            .map(|witness| (witness.client_pid, &witness.active_pane))
            .collect::<BTreeMap<_, _>>();
        let present = self
            .topology
            .panes
            .iter()
            .map(|pane| pane.pane_instance.clone())
            .collect::<BTreeSet<_>>();
        self.peek_leases.retain(|client_pid, lease| {
            let Some(active) = eligible.get(client_pid) else {
                return false;
            };
            match lease {
                PeekLease::Active {
                    target,
                    last_observation_seq,
                } => {
                    if !present.contains(target) {
                        return false;
                    }
                    if observation_seq <= *last_observation_seq {
                        // Queries can complete out of order. An observation that began no
                        // later than the latest decisive one cannot change this lease.
                        true
                    } else if *active == target {
                        *last_observation_seq = observation_seq;
                        true
                    } else {
                        false
                    }
                }
                PeekLease::Pending {
                    previous_target,
                    candidates,
                    ..
                } => {
                    // tmux can expose a transient source/target mismatch while a queued
                    // select-pane or switch-client operation is still in flight. The
                    // worker completion, not that intermediate witness, resolves Pending.
                    candidates.retain(|candidate| present.contains(candidate));
                    let previous_is_present = previous_target
                        .as_ref()
                        .is_some_and(|target| present.contains(target));
                    if !previous_is_present {
                        *previous_target = None;
                    }
                    !candidates.is_empty() || previous_target.is_some()
                }
            }
        });
    }

    pub fn read_authorized_panes(&self, witnesses: &[ClientWitness]) -> BTreeSet<PaneInstance> {
        let mut authorized = BTreeSet::new();
        for witness in witnesses.iter().filter(|witness| witness.is_eligible()) {
            let mut targets = BTreeSet::from([witness.active_pane.clone()]);
            if let Some(target) = self.topology.focus_proxy_target(&witness.active_pane)
                && self
                    .leased
                    .runtime
                    .record(target)
                    .is_some_and(|state| state.agent_present)
            {
                targets.insert(target.clone());
            }
            for target in targets {
                if !self
                    .peek_leases
                    .get(&witness.client_pid)
                    .is_some_and(|lease| lease.protects(&target))
                {
                    authorized.insert(target);
                }
            }
        }
        authorized
    }

    pub fn has_read_authority_for(
        &self,
        witnesses: &[ClientWitness],
        panes: &BTreeSet<PaneInstance>,
    ) -> bool {
        self.read_authorized_panes(witnesses)
            .iter()
            .any(|pane| panes.contains(pane))
    }
}
