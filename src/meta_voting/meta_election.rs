// Copyright 2018 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use super::meta_event::{MetaEvent, MetaEventBuilder};
use super::meta_vote::MetaVote;
use crate::gossip::{EventIndex, Graph};
use crate::id::PublicId;
use crate::observation::{ObservationHash, ObservationKey};
use crate::peer_list::{PeerIndex, PeerIndexMap, PeerIndexSet, PeerListChange};
use crate::round_hash::RoundHash;
use fnv::FnvHashMap;
use std::collections::BTreeSet;
use std::usize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MetaElection {
    pub(crate) meta_events: FnvHashMap<EventIndex, MetaEvent>,
    // The "round hash" for each set of meta votes.  They are held in sequence in the `Vec`, i.e.
    // the one for round `x` is held at index `x`.
    pub(crate) round_hashes: PeerIndexMap<Vec<RoundHash>>,
    // Set of peers participating in this meta-election, i.e. all voters at the time the
    // current meta-election was started.
    pub(crate) voters: PeerIndexSet,
    // The indices of events for each peer that have a non-empty set of `interesting_content`.
    pub(crate) interesting_events: PeerIndexMap<Vec<EventIndex>>,
    // Set of all events that carry a payload that hasn't yet been consensused.
    pub(crate) unconsensused_events: BTreeSet<EventIndex>,
    // Keys of the consensused blocks' payloads in the order they were consensused.
    pub(crate) consensus_history: Vec<ObservationKey>,
}

impl MetaElection {
    pub fn new(voters: PeerIndexSet) -> Self {
        MetaElection {
            meta_events: FnvHashMap::default(),
            round_hashes: PeerIndexMap::default(),
            voters,
            interesting_events: PeerIndexMap::default(),
            unconsensused_events: BTreeSet::new(),
            consensus_history: Vec::new(),
        }
    }

    pub fn add_meta_event<P: PublicId>(&mut self, builder: MetaEventBuilder<P>) {
        let event_index = builder.event().event_index();
        let creator = builder.event().creator();
        let meta_event = builder.finish();

        // Update round hashes.
        for (peer_index, event_votes) in &meta_event.meta_votes {
            let hashes = if let Some(hashes) = self.round_hashes.get_mut(peer_index) {
                hashes
            } else {
                continue;
            };

            for meta_vote in event_votes {
                while hashes.len() < meta_vote.round + 1 {
                    let next_round_hash = hashes[hashes.len() - 1].increment_round();
                    hashes.push(next_round_hash);
                }
            }
        }

        // Update interesting events
        if !meta_event.interesting_content.is_empty() {
            self.interesting_events
                .entry(creator)
                .or_insert_with(Vec::new)
                .push(event_index);
        }

        // Insert the meta-event itself.
        let _ = self.meta_events.insert(event_index, meta_event);
    }

    pub fn remove_meta_event(&mut self, event_index: EventIndex) -> Option<MetaEvent> {
        self.meta_events.remove(&event_index)
    }

    pub fn meta_event(&self, event_index: EventIndex) -> Option<&MetaEvent> {
        self.meta_events.get(&event_index)
    }

    /// The event meta votes if available
    pub fn populated_meta_votes(
        &self,
        event_index: EventIndex,
    ) -> Option<&PeerIndexMap<Vec<MetaVote>>> {
        self.meta_events
            .get(&event_index)
            .map(|meta_event| &meta_event.meta_votes)
            .filter(|meta_votes| !meta_votes.is_empty())
    }

    pub fn round_hashes(&self, peer_index: PeerIndex) -> Option<&Vec<RoundHash>> {
        self.round_hashes.get(peer_index)
    }

    /// List of voters participating in the current meta-election.
    pub fn voters(&self) -> &PeerIndexSet {
        &self.voters
    }

    pub fn consensus_history(&self) -> &[ObservationKey] {
        &self.consensus_history
    }

    pub fn interesting_events(&self) -> impl Iterator<Item = (PeerIndex, &[EventIndex])> {
        self.interesting_events
            .iter()
            .map(|(peer_index, event_indices)| (peer_index, &event_indices[..]))
    }

    pub fn first_interesting_content_by(&self, creator: PeerIndex) -> Option<&ObservationKey> {
        let event_index = self
            .interesting_events
            .get(creator)
            .and_then(|indices| indices.first())?;
        let meta_event = self.meta_events.get(event_index)?;

        meta_event.interesting_content.first()
    }

    pub fn is_already_interesting_content(
        &self,
        creator: PeerIndex,
        payload_key: &ObservationKey,
    ) -> bool {
        self.interesting_events
            .get(creator)
            .map_or(false, |indices| {
                indices.iter().any(|index| {
                    if let Some(meta_event) = self.meta_events.get(index) {
                        meta_event.interesting_content.contains(payload_key)
                    } else {
                        false
                    }
                })
            })
    }

    /// Topological index of the first unconsensused payload-carrying event.
    pub fn start_index(&self) -> Option<usize> {
        // `unconsensused_events` are already sorted topologically, so just return the first one.
        self.unconsensused_events
            .iter()
            .next()
            .map(|event_index| event_index.topological_index())
    }

    /// Starts new election.
    pub fn new_election<P: PublicId>(
        &mut self,
        graph: &Graph<P>,
        decided_key: ObservationKey,
        peer_list_change: Option<PeerListChange>,
    ) {
        self.update_unconsensused_events(graph, &decided_key);
        self.round_hashes.clear();
        self.interesting_events.clear();

        match peer_list_change {
            Some(PeerListChange::Add(peer_index)) => {
                let _ = self.voters.insert(peer_index);
            }
            Some(PeerListChange::Remove(peer_index)) => {
                let _ = self.voters.remove(peer_index);
            }
            None => (),
        }

        // If peer list didn't change, we can reuse existing meta-events to some extent.
        let start_index = if peer_list_change.is_none() {
            self.start_index()
        } else {
            None
        };

        if let Some(start_index) = start_index {
            self.meta_events
                .retain(|event_index, _| event_index.topological_index() >= start_index);
            self.update_interesting_content(&decided_key);
        } else {
            self.meta_events.clear();
        }

        self.consensus_history.push(decided_key);
    }

    pub fn initialise_round_hashes<'a, I, P>(&mut self, peer_ids: I)
    where
        I: IntoIterator<Item = (PeerIndex, &'a P)>,
        P: PublicId + 'a,
    {
        let hash = self
            .consensus_history
            .last()
            .map(|key| *key.hash())
            .unwrap_or(ObservationHash::ZERO);

        self.round_hashes = peer_ids
            .into_iter()
            .map(|(index, id)| {
                let round_hash = RoundHash::new(id, hash);
                (index, vec![round_hash])
            })
            .collect();
    }

    #[cfg(feature = "dump-graphs")]
    pub fn meta_events(&self) -> &FnvHashMap<EventIndex, MetaEvent> {
        &self.meta_events
    }

    pub fn add_unconsensused_event(&mut self, event_index: EventIndex) {
        let _ = self.unconsensused_events.insert(event_index);
    }

    pub fn unconsensused_events<'a>(&'a self) -> impl Iterator<Item = EventIndex> + 'a {
        self.unconsensused_events.iter().cloned()
    }

    // Updates unconsensused_events by removing those that became consensused.
    fn update_unconsensused_events<P: PublicId>(
        &mut self,
        graph: &Graph<P>,
        decided_key: &ObservationKey,
    ) {
        let remove: Vec<_> = self
            .unconsensused_events
            .iter()
            .filter(|event_index| {
                graph
                    .get(**event_index)
                    .map(|event| event.payload_key() == Some(decided_key))
                    .unwrap_or(false)
            })
            .cloned()
            .collect();

        for event_index in remove {
            let _ = self.unconsensused_events.remove(&event_index);
        }
    }

    fn update_interesting_content(&mut self, decided_key: &ObservationKey) {
        for meta_event in self.meta_events.values_mut() {
            meta_event
                .interesting_content
                .retain(|payload_key| payload_key != decided_key);
        }
    }
}

#[cfg(any(all(test, feature = "mock"), feature = "dump-graphs"))]
pub(crate) mod snapshot {
    use super::super::meta_event::snapshot::MetaEventSnapshot;
    use super::*;
    use crate::gossip::{EventHash, Graph};
    use crate::id::SecretId;
    use crate::peer_list::PeerList;
    use std::collections::BTreeMap;

    #[serde(bound = "")]
    #[derive(Eq, PartialEq, Debug, Serialize, Deserialize)]
    pub(crate) struct MetaElectionSnapshot<P: PublicId> {
        meta_events: BTreeMap<EventHash, MetaEventSnapshot<P>>,
        round_hashes: BTreeMap<P, Vec<RoundHash>>,
        voters: BTreeSet<P>,
        interesting_events: BTreeMap<P, Vec<EventHash>>,
    }

    impl<P: PublicId> MetaElectionSnapshot<P> {
        pub fn new<S>(
            meta_election: &MetaElection,
            graph: &Graph<P>,
            peer_list: &PeerList<S>,
        ) -> Self
        where
            S: SecretId<PublicId = P>,
        {
            let meta_events = meta_election
                .meta_events
                .iter()
                .filter_map(|(index, meta_event)| {
                    graph
                        .get(*index)
                        .map(|event| *event.hash())
                        .map(|hash| (hash, MetaEventSnapshot::new(meta_event, peer_list)))
                })
                .collect();

            let interesting_events = meta_election
                .interesting_events
                .iter()
                .filter_map(|(peer_index, event_indices)| {
                    peer_list
                        .get(peer_index)
                        .map(|peer| (peer.id(), event_indices))
                })
                .map(|(peer_id, indices)| {
                    let hashes = indices
                        .iter()
                        .filter_map(|index| graph.get(*index).map(|event| *event.hash()))
                        .collect();
                    (peer_id.clone(), hashes)
                })
                .collect();

            MetaElectionSnapshot {
                meta_events,
                round_hashes: meta_election
                    .round_hashes
                    .iter()
                    .filter_map(|(peer_index, hashes)| {
                        peer_list
                            .get(peer_index)
                            .map(|peer| (peer.id().clone(), hashes.clone()))
                    })
                    .collect(),
                voters: meta_election
                    .voters
                    .iter()
                    .filter_map(|index| peer_list.get(index).map(|peer| peer.id().clone()))
                    .collect(),
                interesting_events,
            }
        }
    }
}
