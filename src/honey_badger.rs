//! # Honey Badger
//!
//! Honey Badger allows a network of _N_ nodes with at most _f_ faulty ones,
//! where _3 f < N_, to input "contributions" - any kind of data -, and to agree on a sequence of
//! _batches_ of contributions. The protocol proceeds in _epochs_, starting at number 0, and outputs
//! one batch in each epoch. It never terminates: It handles a continuous stream of incoming
//! contributions and keeps producing new batches from them. All correct nodes will output the same
//! batch for each epoch. Each validator proposes one contribution per epoch, and every batch will
//! contain the contributions of at least _N - f_ validators.
//!
//! ## How it works
//!
//! In every epoch, every validator encrypts their contribution and proposes it to the others.
//! A `CommonSubset` instance determines which proposals are accepted and will be part of the new
//! batch. Using threshold encryption, the nodes collaboratively decrypt all accepted
//! contributions. Invalid contributions (that e.g. cannot be deserialized) are discarded - their
//! proposers must be faulty -, and the remaining ones are output as the new batch. The next epoch
//! begins as soon as the validators propose new contributions again.
//!
//! So it is essentially an endlessly repeating `CommonSubset`, but with the proposed values
//! encrypted. The encryption makes it harder for an attacker to try and censor a particular value
//! by influencing the set of proposals that make it into the common subset, because they don't
//! know the decrypted values before the subset is determined.

use rand::Rand;
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Debug;
use std::hash::Hash;
use std::marker::PhantomData;
use std::sync::Arc;

use bincode;
use itertools::Itertools;
use serde::{Deserialize, Serialize};

use common_subset::{self, CommonSubset, CommonSubsetStep};
use crypto::{Ciphertext, DecryptionShare};
use fault_log::{FaultKind, FaultLog};
use messaging::{DistAlgorithm, NetworkInfo, Step, Target, TargetedMessage};

error_chain!{
    types {
        Error, ErrorKind, ResultExt, HoneyBadgerResult;
    }

    links {
        CommonSubset(common_subset::Error, common_subset::ErrorKind);
    }

    foreign_links {
        Bincode(Box<bincode::ErrorKind>);
    }

    errors {
        UnknownSender
    }
}

/// A Honey Badger builder, to configure the parameters and create new instances of `HoneyBadger`.
pub struct HoneyBadgerBuilder<C, NodeUid> {
    /// Shared network data.
    netinfo: Arc<NetworkInfo<NodeUid>>,
    /// The maximum number of future epochs for which we handle messages simultaneously.
    max_future_epochs: usize,
    _phantom: PhantomData<C>,
}

impl<C, NodeUid> HoneyBadgerBuilder<C, NodeUid>
where
    C: Serialize + for<'r> Deserialize<'r> + Debug + Hash + Eq,
    NodeUid: Ord + Clone + Debug + Rand,
{
    /// Returns a new `HoneyBadgerBuilder` configured to use the node IDs and cryptographic keys
    /// specified by `netinfo`.
    pub fn new(netinfo: Arc<NetworkInfo<NodeUid>>) -> Self {
        HoneyBadgerBuilder {
            netinfo,
            max_future_epochs: 3,
            _phantom: PhantomData,
        }
    }

    /// Sets the maximum number of future epochs for which we handle messages simultaneously.
    pub fn max_future_epochs(&mut self, max_future_epochs: usize) -> &mut Self {
        self.max_future_epochs = max_future_epochs;
        self
    }

    /// Creates a new Honey Badger instance.
    pub fn build(&self) -> HoneyBadger<C, NodeUid> {
        HoneyBadger {
            netinfo: self.netinfo.clone(),
            epoch: 0,
            has_input: false,
            common_subsets: BTreeMap::new(),
            max_future_epochs: self.max_future_epochs as u64,
            messages: MessageQueue(VecDeque::new()),
            output: Vec::new(),
            incoming_queue: BTreeMap::new(),
            received_shares: BTreeMap::new(),
            decrypted_contributions: BTreeMap::new(),
            ciphertexts: BTreeMap::new(),
        }
    }
}

/// An instance of the Honey Badger Byzantine fault tolerant consensus algorithm.
pub struct HoneyBadger<C, NodeUid: Rand> {
    /// Shared network data.
    netinfo: Arc<NetworkInfo<NodeUid>>,
    /// The earliest epoch from which we have not yet received output.
    epoch: u64,
    /// Whether we have already submitted a proposal for the current epoch.
    has_input: bool,
    /// The Asynchronous Common Subset instance that decides which nodes' transactions to include,
    /// indexed by epoch.
    common_subsets: BTreeMap<u64, CommonSubset<NodeUid>>,
    /// The maximum number of `CommonSubset` instances that we run simultaneously.
    max_future_epochs: u64,
    /// The messages that need to be sent to other nodes.
    messages: MessageQueue<NodeUid>,
    /// The outputs from completed epochs.
    output: Vec<Batch<C, NodeUid>>,
    /// Messages for future epochs that couldn't be handled yet.
    incoming_queue: BTreeMap<u64, Vec<(NodeUid, MessageContent<NodeUid>)>>,
    /// Received decryption shares for an epoch. Each decryption share has a sender and a
    /// proposer. The outer `BTreeMap` has epochs as its key. The next `BTreeMap` has proposers as
    /// its key. The inner `BTreeMap` has the sender as its key.
    received_shares: BTreeMap<u64, BTreeMap<NodeUid, BTreeMap<NodeUid, DecryptionShare>>>,
    /// Decoded accepted proposals.
    decrypted_contributions: BTreeMap<NodeUid, Vec<u8>>,
    /// Ciphertexts output by Common Subset in an epoch.
    ciphertexts: BTreeMap<u64, BTreeMap<NodeUid, Ciphertext>>,
}

pub type HoneyBadgerStep<C, NodeUid> = Step<NodeUid, Batch<C, NodeUid>, Message<NodeUid>>;

impl<C, NodeUid> DistAlgorithm for HoneyBadger<C, NodeUid>
where
    C: Serialize + for<'r> Deserialize<'r> + Debug + Hash + Eq,
    NodeUid: Ord + Clone + Debug + Rand,
{
    type NodeUid = NodeUid;
    type Input = C;
    type Output = Batch<C, NodeUid>;
    type Message = Message<NodeUid>;
    type Error = Error;

    fn input(&mut self, input: Self::Input) -> HoneyBadgerResult<HoneyBadgerStep<C, NodeUid>> {
        let fault_log = self.propose(&input)?;
        self.step(fault_log)
    }

    fn handle_message(
        &mut self,
        sender_id: &NodeUid,
        message: Self::Message,
    ) -> HoneyBadgerResult<HoneyBadgerStep<C, NodeUid>> {
        if !self.netinfo.is_node_validator(sender_id) {
            return Err(ErrorKind::UnknownSender.into());
        }
        let Message { epoch, content } = message;
        let mut fault_log = FaultLog::new();
        if epoch > self.epoch + self.max_future_epochs {
            // Postpone handling this message.
            self.incoming_queue
                .entry(epoch)
                .or_insert_with(Vec::new)
                .push((sender_id.clone(), content));
        } else if epoch == self.epoch {
            fault_log.extend(self.handle_message_content(sender_id, epoch, content)?);
        } // And ignore all messages from past epochs.
        self.step(fault_log)
    }

    fn terminated(&self) -> bool {
        false
    }

    fn our_id(&self) -> &NodeUid {
        self.netinfo.our_uid()
    }
}

impl<C, NodeUid> HoneyBadger<C, NodeUid>
where
    C: Serialize + for<'r> Deserialize<'r> + Debug + Hash + Eq,
    NodeUid: Ord + Clone + Debug + Rand,
{
    /// Returns a new `HoneyBadgerBuilder` configured to use the node IDs and cryptographic keys
    /// specified by `netinfo`.
    pub fn builder(netinfo: Arc<NetworkInfo<NodeUid>>) -> HoneyBadgerBuilder<C, NodeUid> {
        HoneyBadgerBuilder::new(netinfo)
    }

    fn step(
        &mut self,
        fault_log: FaultLog<NodeUid>,
    ) -> HoneyBadgerResult<HoneyBadgerStep<C, NodeUid>> {
        Ok(Step::new(
            self.output.drain(..).collect(),
            fault_log,
            self.messages.drain(..).collect(),
        ))
    }

    /// Proposes a new item in the current epoch.
    pub fn propose(&mut self, proposal: &C) -> HoneyBadgerResult<FaultLog<NodeUid>> {
        if !self.netinfo.is_validator() {
            return Ok(FaultLog::new());
        }
        let step = {
            let cs = match self.common_subsets.entry(self.epoch) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => {
                    entry.insert(CommonSubset::new(self.netinfo.clone(), self.epoch)?)
                }
            };
            let ser_prop = bincode::serialize(&proposal)?;
            let ciphertext = self.netinfo.public_key_set().public_key().encrypt(ser_prop);
            self.has_input = true;
            cs.input(bincode::serialize(&ciphertext).unwrap())?
        };
        Ok(self.process_output(step, None)?)
    }

    /// Returns `true` if input for the current epoch has already been provided.
    pub fn has_input(&self) -> bool {
        !self.netinfo.is_validator() || self.has_input
    }

    /// Handles a message for the given epoch.
    fn handle_message_content(
        &mut self,
        sender_id: &NodeUid,
        epoch: u64,
        content: MessageContent<NodeUid>,
    ) -> HoneyBadgerResult<FaultLog<NodeUid>> {
        match content {
            MessageContent::CommonSubset(cs_msg) => {
                self.handle_common_subset_message(sender_id, epoch, cs_msg)
            }
            MessageContent::DecryptionShare { proposer_id, share } => {
                self.handle_decryption_share_message(sender_id, epoch, proposer_id, share)
            }
        }
    }

    /// Handles a message for the common subset sub-algorithm.
    fn handle_common_subset_message(
        &mut self,
        sender_id: &NodeUid,
        epoch: u64,
        message: common_subset::Message<NodeUid>,
    ) -> HoneyBadgerResult<FaultLog<NodeUid>> {
        let mut fault_log = FaultLog::new();
        let step = {
            // Borrow the instance for `epoch`, or create it.
            let cs = match self.common_subsets.entry(epoch) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => {
                    if epoch < self.epoch {
                        // Epoch has already terminated. Message is obsolete.
                        return Ok(fault_log);
                    } else {
                        entry.insert(CommonSubset::new(self.netinfo.clone(), epoch)?)
                    }
                }
            };
            cs.handle_message(sender_id, message)?
        };
        fault_log.extend(self.process_output(step, Some(epoch))?);
        self.remove_terminated(epoch);
        Ok(fault_log)
    }

    /// Handles decryption shares sent by `HoneyBadger` instances.
    fn handle_decryption_share_message(
        &mut self,
        sender_id: &NodeUid,
        epoch: u64,
        proposer_id: NodeUid,
        share: DecryptionShare,
    ) -> HoneyBadgerResult<FaultLog<NodeUid>> {
        let mut fault_log = FaultLog::new();

        if let Some(ciphertext) = self
            .ciphertexts
            .get(&epoch)
            .and_then(|cts| cts.get(&proposer_id))
        {
            if !self.verify_decryption_share(sender_id, &share, ciphertext) {
                let fault_kind = FaultKind::UnverifiedDecryptionShareSender;
                fault_log.append(sender_id.clone(), fault_kind);
                return Ok(fault_log);
            }
        }

        // Insert the share.
        self.received_shares
            .entry(epoch)
            .or_insert_with(BTreeMap::new)
            .entry(proposer_id.clone())
            .or_insert_with(BTreeMap::new)
            .insert(sender_id.clone(), share);

        if epoch == self.epoch {
            self.try_decrypt_proposer_contribution(proposer_id);
            fault_log.extend(self.try_decrypt_and_output_batch()?);
        }

        Ok(fault_log)
    }

    /// Verifies a given decryption share using the sender's public key and the proposer's
    /// ciphertext. Returns `true` if verification has been successful and `false` if verification
    /// has failed.
    fn verify_decryption_share(
        &self,
        sender_id: &NodeUid,
        share: &DecryptionShare,
        ciphertext: &Ciphertext,
    ) -> bool {
        if let Some(pk) = self.netinfo.public_key_share(sender_id) {
            pk.verify_decryption_share(&share, ciphertext)
        } else {
            false
        }
    }

    /// When contributions of transactions have been decrypted for all valid proposers in this
    /// epoch, moves those contributions into a batch, outputs the batch and updates the epoch.
    fn try_output_batch(&mut self) -> HoneyBadgerResult<FaultLog<NodeUid>> {
        // Wait until contributions have been successfully decoded for all proposer nodes with correct
        // ciphertext outputs.
        if !self.all_contributions_decrypted() {
            return Ok(FaultLog::new());
        }

        // Deserialize the output.
        let mut fault_log = FaultLog::new();
        let contributions: BTreeMap<NodeUid, C> = self
            .decrypted_contributions
            .iter()
            .flat_map(|(proposer_id, ser_contrib)| {
                // If deserialization fails, the proposer of that item is faulty. Ignore it.
                if let Ok(contrib) = bincode::deserialize::<C>(&ser_contrib) {
                    Some((proposer_id.clone(), contrib))
                } else {
                    let fault_kind = FaultKind::BatchDeserializationFailed;
                    fault_log.append(proposer_id.clone(), fault_kind);
                    None
                }
            })
            .collect();
        let batch = Batch {
            epoch: self.epoch,
            contributions,
        };
        debug!(
            "{:?} Epoch {} output {:?}",
            self.netinfo.our_uid(),
            self.epoch,
            batch.contributions.keys().collect::<Vec<_>>()
        );
        // Queue the output and advance the epoch.
        self.output.push(batch);
        fault_log.extend(self.update_epoch()?);
        Ok(fault_log)
    }

    /// Increments the epoch number and clears any state that is local to the finished epoch.
    fn update_epoch(&mut self) -> HoneyBadgerResult<FaultLog<NodeUid>> {
        // Clear the state of the old epoch.
        self.ciphertexts.remove(&self.epoch);
        self.decrypted_contributions.clear();
        self.received_shares.remove(&self.epoch);
        self.epoch += 1;
        self.has_input = false;
        let max_epoch = self.epoch + self.max_future_epochs;
        let mut fault_log = FaultLog::new();
        // TODO: Once stable, use `Iterator::flatten`.
        for (sender_id, content) in
            Itertools::flatten(self.incoming_queue.remove(&max_epoch).into_iter())
        {
            self.handle_message_content(&sender_id, max_epoch, content)?
                .merge_into(&mut fault_log);
        }
        // Handle any decryption shares received for the new epoch.
        self.try_decrypt_and_output_batch()?
            .merge_into(&mut fault_log);
        Ok(fault_log)
    }

    /// Tries to decrypt contributions from all proposers and output those in a batch.
    fn try_decrypt_and_output_batch(&mut self) -> HoneyBadgerResult<FaultLog<NodeUid>> {
        // Return if we don't have ciphertexts yet.
        let proposer_ids: Vec<_> = match self.ciphertexts.get(&self.epoch) {
            Some(cts) => cts.keys().cloned().collect(),
            None => {
                return Ok(FaultLog::new());
            }
        };

        // Try to output a batch if all contributions have been decrypted.
        for proposer_id in proposer_ids {
            self.try_decrypt_proposer_contribution(proposer_id);
        }
        self.try_output_batch()
    }

    /// Returns true if and only if contributions have been decrypted for all selected proposers in
    /// this epoch.
    fn all_contributions_decrypted(&mut self) -> bool {
        match self.ciphertexts.get(&self.epoch) {
            None => false, // No ciphertexts yet.
            Some(ciphertexts) => ciphertexts.keys().eq(self.decrypted_contributions.keys()),
        }
    }

    /// Tries to decrypt the contribution from a given proposer.
    fn try_decrypt_proposer_contribution(&mut self, proposer_id: NodeUid) {
        if self.decrypted_contributions.contains_key(&proposer_id) {
            return; // Already decrypted.
        }
        let shares = if let Some(shares) = self
            .received_shares
            .get(&self.epoch)
            .and_then(|sh| sh.get(&proposer_id))
        {
            shares
        } else {
            return;
        };
        if shares.len() <= self.netinfo.num_faulty() {
            return;
        }

        if let Some(ciphertext) = self
            .ciphertexts
            .get(&self.epoch)
            .and_then(|cts| cts.get(&proposer_id))
        {
            let ids_u64: BTreeMap<&NodeUid, u64> = shares
                .keys()
                .map(|id| (id, self.netinfo.node_index(id).unwrap() as u64))
                .collect();
            let indexed_shares: BTreeMap<&u64, _> = shares
                .into_iter()
                .map(|(id, share)| (&ids_u64[id], share))
                .collect();
            match self
                .netinfo
                .public_key_set()
                .decrypt(indexed_shares, ciphertext)
            {
                Ok(contrib) => {
                    self.decrypted_contributions.insert(proposer_id, contrib);
                }
                Err(err) => error!("{:?} Decryption failed: {:?}.", self.our_id(), err),
            }
        }
    }

    fn send_decryption_shares(
        &mut self,
        cs_output: BTreeMap<NodeUid, Vec<u8>>,
    ) -> HoneyBadgerResult<FaultLog<NodeUid>> {
        let mut fault_log = FaultLog::new();
        let mut ciphertexts = BTreeMap::new();
        for (proposer_id, v) in cs_output {
            let mut ciphertext: Ciphertext;
            if let Ok(ct) = bincode::deserialize(&v) {
                ciphertext = ct;
            } else {
                warn!("Invalid ciphertext from proposer {:?} ignored", proposer_id);
                let fault_kind = FaultKind::InvalidCiphertext;
                fault_log.append(proposer_id.clone(), fault_kind);
                continue;
            }
            let (incorrect_senders, faults) =
                self.verify_pending_decryption_shares(&proposer_id, &ciphertext);
            self.remove_incorrect_decryption_shares(&proposer_id, incorrect_senders);
            fault_log.extend(faults);
            let (valid, dec_fl) = self.send_decryption_share(&proposer_id, &ciphertext)?;
            fault_log.extend(dec_fl);
            if valid {
                ciphertexts.insert(proposer_id.clone(), ciphertext);
                self.try_decrypt_proposer_contribution(proposer_id);
            } else {
                warn!("Share decryption failed for proposer {:?}", proposer_id);
                let fault_kind = FaultKind::ShareDecryptionFailed;
                fault_log.append(proposer_id.clone(), fault_kind);
            }
        }
        self.ciphertexts.insert(self.epoch, ciphertexts);
        fault_log.extend(self.try_decrypt_and_output_batch()?);
        Ok(fault_log)
    }

    /// Verifies the ciphertext and sends decryption shares. Returns whether it is valid.
    fn send_decryption_share(
        &mut self,
        proposer_id: &NodeUid,
        ciphertext: &Ciphertext,
    ) -> HoneyBadgerResult<(bool, FaultLog<NodeUid>)> {
        if !self.netinfo.is_validator() {
            return Ok((ciphertext.verify(), FaultLog::new()));
        }
        let share = match self.netinfo.secret_key_share().decrypt_share(&ciphertext) {
            None => return Ok((false, FaultLog::new())),
            Some(share) => share,
        };
        // Send the share to remote nodes.
        let content = MessageContent::DecryptionShare {
            proposer_id: proposer_id.clone(),
            share: share.clone(),
        };
        let message = Target::All.message(content.with_epoch(self.epoch));
        self.messages.0.push_back(message);
        let epoch = self.epoch;
        let our_id = self.netinfo.our_uid().clone();
        // Receive the share locally.
        let fault_log =
            self.handle_decryption_share_message(&our_id, epoch, proposer_id.clone(), share)?;
        Ok((true, fault_log))
    }

    /// Verifies the shares of the current epoch that are pending verification. Returned are the
    /// senders with incorrect pending shares.
    fn verify_pending_decryption_shares(
        &self,
        proposer_id: &NodeUid,
        ciphertext: &Ciphertext,
    ) -> (BTreeSet<NodeUid>, FaultLog<NodeUid>) {
        let mut incorrect_senders = BTreeSet::new();
        let mut fault_log = FaultLog::new();
        if let Some(sender_shares) = self
            .received_shares
            .get(&self.epoch)
            .and_then(|e| e.get(proposer_id))
        {
            for (sender_id, share) in sender_shares {
                if !self.verify_decryption_share(sender_id, share, ciphertext) {
                    let fault_kind = FaultKind::UnverifiedDecryptionShareSender;
                    fault_log.append(sender_id.clone(), fault_kind);
                    incorrect_senders.insert(sender_id.clone());
                }
            }
        }
        (incorrect_senders, fault_log)
    }

    fn remove_incorrect_decryption_shares(
        &mut self,
        proposer_id: &NodeUid,
        incorrect_senders: BTreeSet<NodeUid>,
    ) {
        if let Some(sender_shares) = self
            .received_shares
            .get_mut(&self.epoch)
            .and_then(|e| e.get_mut(proposer_id))
        {
            for sender_id in incorrect_senders {
                sender_shares.remove(&sender_id);
            }
        }
    }

    /// Checks whether the current epoch has output, and if it does, sends out our decryption
    /// shares.  The `epoch` argument allows to differentiate between calls which produce output in
    /// all conditions, `epoch == None`, and calls which only produce output in a given epoch,
    /// `epoch == Some(given_epoch)`.
    fn process_output(
        &mut self,
        step: CommonSubsetStep<NodeUid>,
        epoch: Option<u64>,
    ) -> HoneyBadgerResult<FaultLog<NodeUid>> {
        let Step {
            output,
            mut fault_log,
            mut messages,
        } = step;
        self.messages.extend_with_epoch(self.epoch, &mut messages);
        // If this is the current epoch, the message could cause a new output.
        if epoch.is_none() || epoch == Some(self.epoch) {
            for cs_output in output {
                fault_log.extend(self.send_decryption_shares(cs_output)?);
                // TODO: May also check that there is no further output from Common Subset.
            }
        }
        Ok(fault_log)
    }

    /// Removes all `CommonSubset` instances from _past_ epochs that have terminated.
    fn remove_terminated(&mut self, from_epoch: u64) {
        for epoch in from_epoch..self.epoch {
            if self
                .common_subsets
                .get(&epoch)
                .map_or(false, CommonSubset::terminated)
            {
                debug!(
                    "{:?} Epoch {} has terminated.",
                    self.netinfo.our_uid(),
                    epoch
                );
                self.common_subsets.remove(&epoch);
            }
        }
    }
}

/// A batch of contributions the algorithm has output.
#[derive(Clone, Debug)]
pub struct Batch<C, NodeUid> {
    pub epoch: u64,
    pub contributions: BTreeMap<NodeUid, C>,
}

impl<C, NodeUid: Ord> Batch<C, NodeUid> {
    /// Returns an iterator over references to all transactions included in the batch.
    pub fn iter<'a>(&'a self) -> impl Iterator<Item = <&'a C as IntoIterator>::Item>
    where
        &'a C: IntoIterator,
    {
        self.contributions.values().flat_map(|item| item)
    }

    /// Returns an iterator over all transactions included in the batch. Consumes the batch.
    pub fn into_tx_iter(self) -> impl Iterator<Item = <C as IntoIterator>::Item>
    where
        C: IntoIterator,
    {
        self.contributions.into_iter().flat_map(|(_, vec)| vec)
    }

    /// Returns the number of transactions in the batch (without detecting duplicates).
    pub fn len<Tx>(&self) -> usize
    where
        C: AsRef<[Tx]>,
    {
        self.contributions
            .values()
            .map(C::as_ref)
            .map(<[Tx]>::len)
            .sum()
    }

    /// Returns `true` if the batch contains no transactions.
    pub fn is_empty<Tx>(&self) -> bool
    where
        C: AsRef<[Tx]>,
    {
        self.contributions
            .values()
            .map(C::as_ref)
            .all(<[Tx]>::is_empty)
    }
}

/// The content of a `HoneyBadger` message. It should be further annotated with an epoch.
#[derive(Clone, Debug, Deserialize, Rand, Serialize)]
pub enum MessageContent<NodeUid: Rand> {
    /// A message belonging to the common subset algorithm in the given epoch.
    CommonSubset(common_subset::Message<NodeUid>),
    /// A decrypted share of the output of `proposer_id`.
    DecryptionShare {
        proposer_id: NodeUid,
        share: DecryptionShare,
    },
}

impl<NodeUid: Rand> MessageContent<NodeUid> {
    pub fn with_epoch(self, epoch: u64) -> Message<NodeUid> {
        Message {
            epoch,
            content: self,
        }
    }
}

/// A message sent to or received from another node's Honey Badger instance.
#[derive(Clone, Debug, Deserialize, Rand, Serialize)]
pub struct Message<NodeUid: Rand> {
    epoch: u64,
    content: MessageContent<NodeUid>,
}

impl<NodeUid: Rand> Message<NodeUid> {
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
}

/// The queue of outgoing messages in a `HoneyBadger` instance.
#[derive(Deref, DerefMut)]
struct MessageQueue<NodeUid: Rand>(VecDeque<TargetedMessage<Message<NodeUid>, NodeUid>>);

impl<NodeUid: Clone + Debug + Ord + Rand> MessageQueue<NodeUid> {
    /// Appends to the queue the messages from `cs`, wrapped with `epoch`.
    fn extend_with_epoch(
        &mut self,
        epoch: u64,
        msgs: &mut VecDeque<TargetedMessage<common_subset::Message<NodeUid>, NodeUid>>,
    ) {
        let convert = |msg: TargetedMessage<common_subset::Message<NodeUid>, NodeUid>| {
            msg.map(|cs_msg| MessageContent::CommonSubset(cs_msg).with_epoch(epoch))
        };
        self.extend(msgs.drain(..).map(convert));
    }
}
