use crate::{
    acceptor::{AcceptResponse, PrepareResponse},
    commands::*,
    proposer::{Proposer, ProposerStatus},
    window::{SlotMutRef, SlotWindow},
    Ballot, Configuration, NodeId, ReplicatedState, Slot, SlottedValue,
};
use bytes::Bytes;
use std::mem;

/// State manager for multi-paxos group
pub struct Replica<S> {
    sender: S,
    config: Configuration,
    proposer: Proposer,
    window: SlotWindow,

    // TODO: bound the proposal queue
    proposal_queue: Vec<Bytes>,
}

impl<S: Sender> Replica<S> {
    /// Replica creation from a sender and starting configuration
    pub fn new(sender: S, config: Configuration) -> Replica<S> {
        let (p1_quorum, p2_quorum) = config.quorum_size();
        let node = config.current();
        Replica {
            sender,
            config,
            proposer: Proposer::new(node, p1_quorum),
            proposal_queue: Vec::new(),
            window: SlotWindow::new(p2_quorum),
        }
    }

    /// Replace the sender with an alertnate implementation
    pub fn with_sender<A>(self, sender: A) -> Replica<A> {
        Replica {
            sender: sender,
            config: self.config,
            proposer: self.proposer,
            proposal_queue: self.proposal_queue,
            window: self.window,
        }
    }

    /// Mutable reference to the sender
    pub fn sender_mut(&mut self) -> &mut S {
        &mut self.sender
    }

    /// Reference to the sender
    pub fn sender(&self) -> &S {
        &self.sender
    }

    /// Broadcast ACCEPT messages once the proposer has phase 1 quorum
    fn drive_accept(&mut self) {
        if self.proposer.status() != ProposerStatus::Leader {
            return;
        }

        let bal = self.proposer.highest_observed_ballot().unwrap();
        assert!(bal.1 == self.config.current());

        // add queued proposals to new slots
        for value in self.proposal_queue.drain(..) {
            let mut slot = self.window.next_slot();
            slot.acceptor().notice_value(bal, value.clone());
        }

        // queue up all accepts
        let accepts = self
            .window
            .open_range()
            .filter_map(|slot| {
                match self.window.slot_mut(slot) {
                    SlotMutRef::Open(ref mut open_slot) => {
                        if let Some((_, val)) = open_slot.acceptor().highest_value() {
                            // have the acceptor update the highest ballot to this one
                            open_slot.acceptor().notice_value(bal, val.clone());
                            Some((slot, bal, val))
                        } else {
                            open_slot.acceptor().notice_value(bal, Bytes::default());
                            Some((slot, bal, Bytes::default()))
                        }
                    }
                    SlotMutRef::Empty(empty_slot) => {
                        // fill the hole with an empty slot
                        let mut slot = empty_slot.fill();
                        slot.acceptor().notice_value(bal, Bytes::default());
                        Some((slot.slot(), bal, Bytes::default()))
                    }
                    _ => None,
                }
            })
            .collect::<Vec<SlottedValue>>();

        // send out the accepts
        for (slot, bal, val) in accepts {
            self.broadcast(|c| c.accept(slot, bal, val.clone()));
        }
    }

    /// Forwards pending proposals to the new leader
    fn forward(&mut self) {
        if self.proposer.status() != ProposerStatus::Follower || self.proposal_queue.is_empty() {
            return;
        }

        if let Some(Ballot(_, node)) = self.proposer.highest_observed_ballot() {
            let mut proposals = Vec::new();
            mem::swap(&mut self.proposal_queue, &mut proposals);
            self.sender.send_to(node, move |c| {
                for proposal in proposals.into_iter() {
                    c.proposal(proposal);
                }
            });
        }
    }

    /// Executes commands that have been decided.
    fn execute_decisions(&mut self) {
        for (slot, val) in self.window.drain_decisions() {
            if val.len() > 0 {
                self.sender.state_machine().execute(slot, val);
            }
        }
    }

    fn broadcast<F>(&mut self, f: F)
    where
        F: Fn(&mut S::Commander) -> (),
    {
        // TODO: thrifty option
        for node in self.config.peers().into_iter() {
            self.sender.send_to(node, &f);
        }
    }
}

impl<S: Sender> Commander for Replica<S> {
    fn proposal(&mut self, val: Bytes) {
        // redirect to the distinguished proposer or start PREPARE
        match self.proposer.status() {
            ProposerStatus::Follower if self.proposer.highest_observed_ballot().is_none() => {
                // no known proposers, go through prepare cycle
                self.proposal_queue.push(val);
                let bal = self.proposer.prepare();
                self.broadcast(|c| c.prepare(bal));
            }
            ProposerStatus::Follower => {
                self.sender.send_to(self.proposer.highest_observed_ballot().unwrap().1, |c| {
                    c.proposal(val)
                });
            }
            ProposerStatus::Candidate => {
                // still waiting for promises, queue up the value
                // TODO: should this re-send some PREPARE messages?
                self.proposal_queue.push(val);
            }
            ProposerStatus::Leader => {
                // node is the distinguished proposer
                let bal = self.proposer.highest_observed_ballot().unwrap();
                let slot = {
                    let mut slot_ref = self.window.next_slot();
                    slot_ref.acceptor().notice_value(bal, val.clone());
                    slot_ref.slot()
                };
                self.broadcast(|c| c.accept(slot, bal, val.clone()));
            }
        }
    }

    fn prepare(&mut self, bal: Ballot) {
        self.proposer.observe_ballot(bal);

        let node_id = self.config.current();

        let mut accepted = Vec::new();
        for slot in self.window.open_range() {
            match self.window.slot_mut(slot) {
                SlotMutRef::Open(ref mut open_ref) => {
                    match open_ref.acceptor().receive_prepare(bal) {
                        PrepareResponse::Promise { value: Some((bal, val)), .. } => {
                            accepted.push((slot, bal, val));
                        }
                        PrepareResponse::Reject { proposed, preempted } => {
                            // found a slot that accepted a higher ballot, send the reject
                            self.sender.send_to(bal.1, |c| c.reject(node_id, proposed, preempted));
                            return;
                        }
                        _ => {}
                    }
                }
                SlotMutRef::Resolved(bal, val) => {
                    // TODO: is this the right thing to do here?????
                    accepted.push((slot, bal, val));
                }

                SlotMutRef::Empty(_) => {
                    warn!("Empty slot {} detected in the middle of the open range", slot);
                }
                SlotMutRef::ResolutionTruncated => {
                    unreachable!("Cannot be resolved in the middle of the open range")
                }
            }
        }
        self.sender.send_to(bal.1, move |c| c.promise(node_id, bal, accepted));
    }

    fn promise(&mut self, node: NodeId, bal: Ballot, accepted: Vec<SlottedValue>) {
        if self.proposer.status() != ProposerStatus::Candidate {
            return;
        }

        self.proposer.receive_promise(node, bal);

        // track highest proposals
        for (slot, bal, val) in accepted.into_iter() {
            match self.window.slot_mut(slot) {
                SlotMutRef::Open(ref mut open_slot) => {
                    open_slot.acceptor().notice_value(bal, val);
                }
                SlotMutRef::Empty(empty_slot) => {
                    empty_slot.fill().acceptor().notice_value(bal, val);
                }
                _ => {}
            }
        }

        // if we have phase 1 quorum, we can send out ACCEPT messages
        self.drive_accept();
    }

    fn accept(&mut self, slot: Slot, bal: Ballot, val: Bytes) {
        self.proposer.observe_ballot(bal);

        let current_node = self.config.current();
        let acceptor_res = match self.window.slot_mut(slot) {
            SlotMutRef::Empty(empty_slot) => {
                let mut open_slot = empty_slot.fill();
                open_slot.acceptor().receive_accept(bal, val)
            }
            SlotMutRef::Open(ref mut open_slot) => open_slot.acceptor().receive_accept(bal, val),
            _ => return,
        };

        match acceptor_res {
            AcceptResponse::Accepted { .. } => {
                // TODO: what do we do w/ the preempted proposal
                self.sender.send_to(bal.1, |c| c.accepted(current_node, slot, bal));
            }
            AcceptResponse::Reject { proposed, preempted } => {
                self.sender.send_to(bal.1, |c| c.reject(current_node, proposed, preempted));
            }
            _ => {}
        }
    }

    fn reject(&mut self, node: NodeId, proposed: Ballot, promised: Ballot) {
        // reject it within the proposer
        self.proposer.receive_reject(node, proposed, promised);
        self.forward();
    }

    fn accepted(&mut self, node: NodeId, slot: Slot, bal: Ballot) {
        self.proposer.observe_ballot(bal);

        let resolution = match self.window.slot_mut(slot) {
            SlotMutRef::Open(ref mut open_ref) => {
                open_ref.acceptor().receive_accepted(node, bal);
                open_ref.acceptor().resolution()
            }
            SlotMutRef::Empty(_) => {
                warn!("Received accepted() for slot {} which is unknown", slot);
                return;
            }
            _ => return,
        };

        if let Some((bal, val)) = resolution {
            self.broadcast(|c| c.resolution(slot, bal, val.clone()));
        }

        // execute resolved decisions
        self.execute_decisions();
    }

    fn resolution(&mut self, slot: Slot, bal: Ballot, val: Bytes) {
        self.proposer.observe_ballot(bal);

        // resolve the slot
        match self.window.slot_mut(slot) {
            SlotMutRef::Empty(empty_slot) => empty_slot.fill().acceptor().resolve(bal, val),
            SlotMutRef::Open(ref mut open) => open.acceptor().resolve(bal, val),
            _ => {}
        }

        // execute resolved decisions
        self.execute_decisions();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ReplicatedState;
    use lazy_static::lazy_static;
    use std::ops::Index;

    lazy_static! {
        static ref CONFIG: Configuration = Configuration::new(
            4u32,
            vec![
                (0, "127.0.0.1:4000".parse().unwrap()),
                (1, "127.0.0.1:4001".parse().unwrap()),
                (2, "127.0.0.1:4002".parse().unwrap()),
                (3, "127.0.0.1:4003".parse().unwrap()),
            ]
            .into_iter(),
        );
    }

    #[test]
    fn replica_proposal() {
        let mut replica = Replica::new(VecSender::default(), CONFIG.clone());

        // sent with no existing proposal, kickstarts phase 1
        replica.proposal("123".into());
        assert_eq!(Some(Ballot(0, 4)), replica.proposer.highest_observed_ballot());
        assert_eq!(&[Command::Prepare(Ballot(0, 4))], &replica.sender[0]);
        assert_eq!(&[Command::Prepare(Ballot(0, 4))], &replica.sender[1]);
        assert_eq!(&[Command::Prepare(Ballot(0, 4))], &replica.sender[2]);
        assert_eq!(&[Command::Prepare(Ballot(0, 4))], &replica.sender[3]);
        replica.sender.clear();

        replica.proposal("456".into());
        assert_eq!(Some(Ballot(0, 4)), replica.proposer.highest_observed_ballot());
        assert!(replica.sender[0].is_empty());
        assert!(replica.sender[1].is_empty());
        assert!(replica.sender[2].is_empty());
        assert!(replica.sender[3].is_empty());

        assert!(replica.sender.resolutions().is_empty());
    }

    #[test]
    fn replica_proposal_redirection() {
        let mut replica = Replica::new(VecSender::default(), CONFIG.clone());
        replica.prepare(Ballot(0, 3));
        assert_eq!(Some(Ballot(0, 3)), replica.proposer.highest_observed_ballot());
        replica.sender.clear();

        replica.proposal("123".into());
        assert!(replica.sender[0].is_empty());
        assert!(replica.sender[1].is_empty());
        assert!(replica.sender[2].is_empty());
        assert_eq!(&[Command::Proposal("123".into())], &replica.sender[3]);

        assert!(replica.sender.resolutions().is_empty());
    }

    #[test]
    fn replica_prepare() {
        let mut replica = Replica::new(VecSender::default(), CONFIG.clone());

        replica.prepare(Ballot(1, 0));
        assert_eq!(Some(Ballot(1, 0)), replica.proposer.highest_observed_ballot());
        assert_eq!(&[Command::Promise(4, Ballot(1, 0), Vec::new())], &replica.sender[0]);
        assert!(&replica.sender[1].is_empty());
        assert!(&replica.sender[2].is_empty());
        assert!(&replica.sender[3].is_empty());
        replica.sender.clear();

        replica.prepare(Ballot(0, 2));
        assert_eq!(Some(Ballot(1, 0)), replica.proposer.highest_observed_ballot());
        assert!(&replica.sender[0].is_empty());
        assert!(&replica.sender[1].is_empty());
        assert_eq!(&[Command::Reject(4, Ballot(0, 2), Ballot(1, 0))], &replica.sender[2]);
        assert!(&replica.sender[3].is_empty());

        assert!(replica.sender.resolutions().is_empty());
    }

    #[test]
    fn replica_promise_without_existing_accepted_value() {
        let mut replica = Replica::new(VecSender::default(), CONFIG.clone());
        replica.proposal("123".into());
        assert_eq!(Some(Ballot(0, 4)), replica.proposer.highest_observed_ballot());
        replica.sender.clear();

        // replica needs 2 more promises to achieve Phase 1 Quorum
        replica.promise(0, Ballot(0, 4), Vec::new());
        (0..4).for_each(|i| assert!(replica.sender[i].is_empty()));

        replica.promise(2, Ballot(0, 4), Vec::new());

        (0..4).for_each(|i| {
            assert_eq!(&[Command::Accept(0, Ballot(0, 4), "123".into())], &replica.sender[i])
        });

        assert!(replica.sender.resolutions().is_empty());
    }

    #[test]
    fn replica_promise_with_existing_accepted_value() {
        let mut replica = Replica::new(VecSender::default(), CONFIG.clone());
        replica.proposal("123".into());
        assert_eq!(Some(Ballot(0, 4)), replica.proposer.highest_observed_ballot());
        replica.sender.clear();

        // replica needs 2 more promises to achieve Phase 1 Quorum
        replica.promise(1, Ballot(0, 4), vec![(0, Ballot(0, 0), "456".into())]);
        (0..4).for_each(|i| assert!(replica.sender[i].is_empty()));

        replica.promise(2, Ballot(0, 4), vec![]);

        (0..4).for_each(|i| {
            assert_eq!(
                &[
                    Command::Accept(0, Ballot(0, 4), "456".into()),
                    Command::Accept(1, Ballot(0, 4), "123".into())
                ],
                &replica.sender[i]
            )
        });

        assert!(replica.sender.resolutions().is_empty());
    }

    #[test]
    fn replica_promise_with_slot_holes() {
        let mut replica = Replica::new(VecSender::default(), CONFIG.clone());
        replica.proposal("123".into());
        assert_eq!(Some(Ballot(0, 4)), replica.proposer.highest_observed_ballot());
        replica.sender.clear();

        // replica needs 2 more promises to achieve Phase 1 Quorum
        replica.promise(1, Ballot(0, 4), vec![(2, Ballot(0, 0), "456".into())]);
        (0..4).for_each(|i| assert!(replica.sender[i].is_empty()));

        replica.promise(2, Ballot(0, 4), vec![]);

        (0..4).for_each(|i| {
            assert_eq!(
                &[
                    Command::Accept(0, Ballot(0, 4), Bytes::default()),
                    Command::Accept(1, Ballot(0, 4), Bytes::default()),
                    Command::Accept(2, Ballot(0, 4), "456".into()),
                    Command::Accept(3, Ballot(0, 4), "123".into())
                ],
                &replica.sender[i]
            );
        });

        assert!(replica.sender.resolutions().is_empty());
    }

    #[test]
    fn replica_accept() {
        let mut replica = Replica::new(VecSender::default(), CONFIG.clone());
        replica.prepare(Ballot(8, 2));
        assert_eq!(Some(Ballot(8, 2)), replica.proposer.highest_observed_ballot());
        replica.sender.clear();

        // test rejection first for bal < proposer.highest_observed_ballot
        replica.accept(0, Ballot(1, 1), "123".into());
        assert_eq!(&[Command::Reject(4, Ballot(1, 1), Ballot(8, 2))], &replica.sender[1]);
        replica.sender.clear();

        // test replying with accepted message when bal =
        // proposer.highest_observed_ballot
        replica.accept(0, Ballot(8, 2), "456".into());
        assert_eq!(Some(Ballot(8, 2)), replica.proposer.highest_observed_ballot());
        assert_eq!(&[Command::Accepted(4, 0, Ballot(8, 2))], &replica.sender[2]);
        replica.sender.clear();

        // test replying with accepted message when bal >
        // proposer.highest_observed_ballot
        replica.accept(0, Ballot(9, 2), "789".into());
        assert_eq!(Some(Ballot(9, 2)), replica.proposer.highest_observed_ballot());
        assert_eq!(&[Command::Accepted(4, 0, Ballot(9, 2))], &replica.sender[2]);

        assert!(replica.sender.resolutions().is_empty());
    }

    #[test]
    fn replica_reject() {
        let mut replica = Replica::new(VecSender::default(), CONFIG.clone());
        replica.proposal("123".into());
        assert_eq!(Some(Ballot(0, 4)), replica.proposer.highest_observed_ballot());
        replica.sender.clear();

        replica.reject(2, Ballot(0, 4), Ballot(5, 3));
        assert_eq!(Some(Ballot(5, 3)), replica.proposer.highest_observed_ballot());
        assert_eq!(ProposerStatus::Follower, replica.proposer.status());
        assert_eq!(&[Command::Proposal("123".into())], &replica.sender[3]);
        (0..3).for_each(|i| assert!(replica.sender[i].is_empty()));

        assert!(replica.sender.resolutions().is_empty());
    }

    #[test]
    fn replica_accepted() {
        let mut replica = Replica::new(VecSender::default(), CONFIG.clone());
        replica.proposal("123".into());
        assert_eq!(Some(Ballot(0, 4)), replica.proposer.highest_observed_ballot());
        replica.promise(1, Ballot(0, 4), vec![]);
        replica.promise(0, Ballot(0, 4), vec![]);
        replica.promise(2, Ballot(0, 4), vec![]);
        replica.sender.clear();

        // wait for phase 2 quorum (accepted) before sending resolution
        replica.accepted(0, 0, Ballot(0, 4));
        (0..4).for_each(|i| assert!(replica.sender[i].is_empty()));

        replica.accepted(2, 0, Ballot(0, 4));
        (0..4).for_each(|i| {
            assert_eq!(&[Command::Resolution(0, Ballot(0, 4), "123".into())], &replica.sender[i])
        });

        assert_eq!(&[(0, "123".into())], replica.sender.resolutions());
    }

    #[test]
    fn replica_resolution() {
        let mut replica = Replica::new(VecSender::default(), CONFIG.clone());

        replica.resolution(4, Ballot(1, 2), "123".into());
        assert_eq!((0..5), replica.window.open_range());
        assert!(match replica.window.slot_mut(4) {
            SlotMutRef::Resolved(Ballot(1, 2), val) if val == "123" => true,
            _ => false,
        });

        replica.resolution(1, Ballot(1, 2), Bytes::default());
        replica.resolution(0, Ballot(1, 2), "000".into());
        assert_eq!(&[(0, "000".into())], replica.sender.resolutions());

        // fill hole 1,2
        replica.resolution(1, Ballot(1, 2), Bytes::default());
        replica.resolution(2, Ballot(1, 2), Bytes::default());
        replica.resolution(3, Ballot(1, 2), "3".into());

        assert_eq!(
            &[(0, "000".into()), (3, "3".into()), (4, "123".into())],
            replica.sender.resolutions()
        );
    }

    #[derive(Default)]
    struct VecSender([Vec<Command>; 4], StateMachine);

    impl VecSender {
        fn clear(&mut self) {
            for i in 0usize..4 {
                self.0[i].clear();
            }
        }

        fn resolutions(&self) -> &[(Slot, Bytes)] {
            &(&self.1).0
        }
    }

    impl Index<usize> for VecSender {
        type Output = [Command];
        fn index(&self, n: usize) -> &[Command] {
            assert!(n < 4);
            &self.0[n]
        }
    }

    impl Sender for VecSender {
        type Commander = Vec<Command>;
        type StateMachine = StateMachine;

        fn send_to<F>(&mut self, node: NodeId, f: F)
        where
            F: FnOnce(&mut Self::Commander) -> (),
        {
            assert!(node < 4);
            f(&mut self.0[node as usize]);
        }

        fn state_machine(&mut self) -> &mut Self::StateMachine {
            &mut self.1
        }
    }

    #[derive(Default)]
    struct StateMachine(Vec<(Slot, Bytes)>);

    impl ReplicatedState for StateMachine {
        fn execute(&mut self, slot: Slot, command: Bytes) {
            self.0.push((slot, command));
        }
    }
}
