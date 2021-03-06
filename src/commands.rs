use crate::{Ballot, NodeId, ReplicatedState, Slot, SlottedValue};
use bytes::Bytes;

#[cfg(test)]
use std::iter::Extend;

/// Sends commands to other replicas in addition to applying
/// resolved commands at the current replica
pub trait Sender {
    /// Commander type used to send messages to other instances
    type Commander: Commander;

    /// The state machine used by this replica
    type StateMachine: ReplicatedState;

    /// Send a message to a single node
    fn send_to<F>(&mut self, node: NodeId, command: F)
    where
        F: FnOnce(&mut Self::Commander) -> ();

    /// Resolves the state machine to apply values.
    fn state_machine(&mut self) -> &mut Self::StateMachine;
}

/// Receiver of Paxos commands.
pub trait Commander {
    /// Receive a proposal
    fn proposal(&mut self, val: Bytes);

    /// Receive a Phase 1a PREPARE message containing the proposed ballot
    fn prepare(&mut self, bal: Ballot);

    /// Receive a Phase 1b PROMISE message containing the node
    /// that generated the promise, the ballot promised and all accepted
    /// values within the open window.
    fn promise(&mut self, node: NodeId, bal: Ballot, accepted: Vec<SlottedValue>);

    /// Receive a Phase 2a ACCEPT message that contains the the slot, proposed
    /// ballot and value of the proposal. The ballot contains the node of
    /// the leader of the slot.
    fn accept(&mut self, slot: Slot, bal: Ballot, val: Bytes);

    /// Receives a REJECT message from a peer containing a higher ballot that
    /// preempts either a Phase 1a (PREPARE) for Phase 2a (ACCEPT) message.
    fn reject(&mut self, node: NodeId, proposed: Ballot, preempted: Ballot);

    /// Receives a Phase 2b ACCEPTED message containing the acceptor that has
    /// accepted the slot's proposal along with the ballot that generated
    /// the slot.
    fn accepted(&mut self, node: NodeId, slot: Slot, bal: Ballot);

    /// Receives a final resolution of a slot that has been accepted by a
    /// majority of acceptors.
    ///
    /// NOTE: Resolutions may arrive out-of-order. No guarantees are made on
    /// slot order.
    fn resolution(&mut self, slot: Slot, bal: Ballot, val: Bytes);
}

#[derive(PartialEq, Eq, Debug)]
#[cfg(test)]
pub enum Command {
    Proposal(Bytes),
    Prepare(Ballot),
    Promise(NodeId, Ballot, Vec<(Slot, Ballot, Bytes)>),
    Accept(Slot, Ballot, Bytes),
    Reject(NodeId, Ballot, Ballot),
    Accepted(NodeId, Slot, Ballot),
    Resolution(Slot, Ballot, Bytes),
}

#[cfg(test)]
impl<T> Commander for T
where
    T: Extend<Command>,
{
    fn proposal(&mut self, bytes: Bytes) {
        self.extend(Some(Command::Proposal(bytes)));
    }

    fn prepare(&mut self, bal: Ballot) {
        self.extend(Some(Command::Prepare(bal)));
    }

    fn promise(&mut self, node: NodeId, bal: Ballot, accepted: Vec<SlottedValue>) {
        self.extend(Some(Command::Promise(node, bal, accepted)));
    }

    fn accept(&mut self, slot: Slot, bal: Ballot, val: Bytes) {
        self.extend(Some(Command::Accept(slot, bal, val)));
    }

    fn reject(&mut self, node: NodeId, proposed: Ballot, promised: Ballot) {
        self.extend(Some(Command::Reject(node, proposed, promised)));
    }

    fn accepted(&mut self, node: NodeId, slot: Slot, bal: Ballot) {
        self.extend(Some(Command::Accepted(node, slot, bal)));
    }

    fn resolution(&mut self, slot: Slot, bal: Ballot, val: Bytes) {
        self.extend(Some(Command::Resolution(slot, bal, val)));
    }
}
