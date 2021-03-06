// Copyright 2018 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use super::dot_parser::{parse_dot_file, ParsedContents};
use crate::gossip::IndexedEventRef;
use crate::gossip::{Event, Request, Response};
use crate::hash::Hash;
use crate::mock::{PeerId, Transaction};
use crate::observation::{ConsensusMode, Observation, ObservationKey, ObservationStore};
use crate::parsec::Parsec;
use crate::peer_list::PeerIndex;
use std::collections::BTreeSet;
use std::io;
use std::path::Path;

/// Record of a Parsec session which consist of sequence of operations (`vote_for`, `handle_request`
/// and `handle_response`). Can be produced from a previously dumped DOT file and after replaying,
/// produces the same gossip graph. Useful for benchmarking.
#[derive(Clone)]
pub struct Record {
    our_id: PeerId,
    genesis_group: BTreeSet<PeerId>,
    actions: Vec<Action>,

    // Keys of the consensused blocks' payloads in the order they were consensused.
    consensus_history: Vec<ObservationKey>,

    // Consensus mode to play
    consensus_mode: ConsensusMode,
}

impl Record {
    pub fn parse<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let contents = parse_dot_file(path.as_ref())?;
        Ok(Self::from(contents))
    }

    pub fn play(self) -> Parsec<Transaction, PeerId> {
        let mut parsec =
            Parsec::from_genesis(self.our_id, &self.genesis_group, self.consensus_mode);

        for action in self.actions {
            action.run(&mut parsec)
        }

        parsec
    }

    pub fn consensus_history(&self) -> Vec<Hash> {
        self.consensus_history
            .iter()
            .map(|observation| observation.hash().0)
            .collect()
    }

    pub fn set_consensus_mode(&mut self, consensus_mode: ConsensusMode) {
        self.consensus_mode = consensus_mode;
    }
}

impl From<ParsedContents> for Record {
    fn from(contents: ParsedContents) -> Self {
        // Find the genesis group
        let genesis_group = unwrap!(
            contents
                .graph
                .iter()
                .filter_map(|event| extract_genesis_group(event.inner(), &contents.observations))
                .next()
                .cloned(),
            "No event carrying Observation::Genesis found"
        );

        assert!(
            genesis_group.contains(&contents.our_id),
            "Records currently supported only for the members of the genesis group"
        );

        let mut actions = Vec::new();
        let mut skip_our_accusations = false;
        let mut known = vec![false; contents.graph.len()];

        let collect_event_to_gossip = |other_parent: IndexedEventRef<_>, known: &mut Vec<bool>| {
            let mut events_to_gossip = Vec::new();
            let other_parent_tindex = other_parent.topological_index();
            for event in contents.graph.ancestors(other_parent) {
                let event_tindex = event.topological_index();
                if known[event_tindex] && other_parent_tindex != event_tindex {
                    continue;
                } else {
                    known[event_tindex] = true;
                }

                events_to_gossip.push(unwrap!(event.pack(contents.event_context())));
            }
            events_to_gossip.reverse();
            events_to_gossip
        };

        let collect_remaining_event_to_gossip = |known: &mut Vec<bool>| {
            let mut events_to_gossip = Vec::new();
            let mut last_event = None;
            for event in contents.graph.iter() {
                let event_tindex = event.topological_index();
                if known[event_tindex] {
                    continue;
                } else {
                    known[event_tindex] = true;
                }

                events_to_gossip.push(unwrap!(event.pack(contents.event_context())));
                last_event = Some(event);
            }
            (events_to_gossip, last_event)
        };

        for event in &contents.graph {
            if event.topological_index() == 0 {
                // Skip the initial event
                assert!(event.is_initial());
                assert_eq!(event.creator(), PeerIndex::OUR);
                continue;
            }

            if event.topological_index() == 1 {
                // Skip the genesis event
                assert!(extract_genesis_group(&*event, &contents.observations).is_some());
                assert_eq!(event.creator(), PeerIndex::OUR);
                continue;
            }

            if event.creator() == PeerIndex::OUR {
                if let Some(observation) = event
                    .payload_key()
                    .and_then(|key| contents.observations.get(key))
                    .map(|info| &info.observation)
                {
                    known[event.topological_index()] = true;

                    if let Observation::Accusation { .. } = *observation {
                        if skip_our_accusations {
                            continue;
                        } else {
                            // Accusations by us must follow our sync event.
                            panic!("Unexpected accusation {:?}", *event);
                        }
                    }

                    actions.push(Action::Vote(observation.clone()));
                } else if event.is_request() || event.is_response() {
                    known[event.topological_index()] = true;

                    let other_parent = unwrap!(
                        event
                            .other_parent()
                            .and_then(|hash| contents.graph.get(hash)),
                        "Sync event without other-parent: {:?}",
                        *event
                    );

                    let src = unwrap!(contents
                        .peer_list
                        .get(other_parent.creator())
                        .map(|peer| peer.id().clone()));

                    let events_to_gossip = collect_event_to_gossip(other_parent, &mut known);

                    if event.is_request() {
                        actions.push(Action::Request(src, Request::new(events_to_gossip)))
                    } else {
                        actions.push(Action::Response(src, Response::new(events_to_gossip)))
                    }

                    // Skip all accusations directly following our sync event, as they will be
                    // created during replay.
                    skip_our_accusations = true;
                } else {
                    panic!("Unexpected event {:?}", *event);
                }
            } else {
                skip_our_accusations = false;
            }
        }

        // Need a request to our peer with the remaining events
        let (events_to_gossip, other_parent) = collect_remaining_event_to_gossip(&mut known);
        if let Some(other_parent) = other_parent {
            let src = unwrap!(contents
                .peer_list
                .get(other_parent.creator())
                .map(|peer| peer.id().clone()));
            actions.push(Action::Request(src, Request::new(events_to_gossip)));
        }

        Record {
            our_id: contents.our_id,
            genesis_group,
            actions,
            consensus_history: contents.meta_election.consensus_history,
            consensus_mode: ConsensusMode::Supermajority,
        }
    }
}

#[derive(Clone)]
enum Action {
    Vote(Observation<Transaction, PeerId>),
    Request(PeerId, Request<Transaction, PeerId>),
    Response(PeerId, Response<Transaction, PeerId>),
}

impl Action {
    fn run(self, parsec: &mut Parsec<Transaction, PeerId>) {
        match self {
            Action::Vote(observation) => unwrap!(parsec.vote_for(observation)),
            Action::Request(src, request) => {
                let _ = unwrap!(parsec.handle_request(&src, request));
            }
            Action::Response(src, response) => unwrap!(parsec.handle_response(&src, response)),
        }
    }
}

fn extract_genesis_group<'a>(
    event: &Event<PeerId>,
    observations: &'a ObservationStore<Transaction, PeerId>,
) -> Option<&'a BTreeSet<PeerId>> {
    event
        .payload_key()
        .and_then(|key| observations.get(key))
        .map(|info| &info.observation)
        .and_then(|observation| {
            if let Observation::Genesis(ref genesis_group) = *observation {
                Some(genesis_group)
            } else {
                None
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parsec::get_graph_snapshot;
    use std::cmp;

    #[derive(PartialEq, Eq, Debug)]
    struct TrucatedHashes<'th> {
        actual_len: usize,
        hashes: &'th [Hash],
    }

    impl<'th> TrucatedHashes<'th> {
        fn new_with_one_missing_element(hashes: &'th [Hash]) -> Self {
            Self {
                actual_len: hashes.len() + 1,
                hashes,
            }
        }

        fn new_with_one_trucated_element(hashes: &'th [Hash]) -> Self {
            Self {
                actual_len: hashes.len(),
                hashes: &hashes[0..cmp::max(hashes.len(), 1) - 1],
            }
        }
    }

    /// Run smoke test using given dot file
    /// Use ignore_last_events to skip fake request that replay generates to send remaining gossip events.
    fn smoke(path: &str, ignore_last_events: usize) {
        let contents = unwrap!(parse_dot_file(path));
        let expected = Parsec::from_parsed_contents(contents);
        let expected_events = {
            let ignore_last_events = 0;
            get_graph_snapshot(&expected, ignore_last_events)
        };

        let contents = unwrap!(parse_dot_file(path));
        let replay = Record::from(contents);
        let actual = replay.play();
        let actual_events = get_graph_snapshot(&actual, ignore_last_events);

        assert_eq!(expected_events, actual_events);
    }

    /// Run smoke test using given dot file checking the consensus history
    fn smoke_consensus_history(path: &str) {
        let contents = unwrap!(parse_dot_file(path));
        let replay = Record::from(contents);
        let expected_history = replay.consensus_history();

        let actual = replay.play();
        let actual_history = actual.meta_election_consensus_history_hash();

        assert_eq!(
            TrucatedHashes::new_with_one_missing_element(&expected_history),
            TrucatedHashes::new_with_one_trucated_element(&actual_history)
        );
    }

    #[test]
    fn smoke_parsec() {
        let ignore_last_events = 0;
        smoke("input_graphs/benches/minimal.dot", ignore_last_events)
    }

    #[test]
    fn smoke_other_peer_names() {
        let ignore_last_events = 1;
        smoke(
            "input_graphs/dev_utils_record_tests_smoke_other_peer_names/annie.dot",
            ignore_last_events,
        )
    }

    #[test]
    fn smoke_routing() {
        let ignore_last_events = 1;
        smoke(
            "input_graphs/dev_utils_record_tests_smoke_routing/minimal.dot",
            ignore_last_events,
        )
    }

    #[test]
    fn smoke_consensus_history_parsec() {
        smoke_consensus_history("input_graphs/benches/minimal.dot")
    }

    #[test]
    fn smoke_consensus_history_other_peer_names() {
        smoke_consensus_history(
            "input_graphs/dev_utils_record_tests_smoke_other_peer_names/annie.dot",
        )
    }
}
