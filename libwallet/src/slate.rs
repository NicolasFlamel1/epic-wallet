// Copyright 2018 The Grin Developers
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Functions for building partial transactions to be passed
//! around during an interactive wallet exchange

use crate::blake2::blake2b::blake2b;
use crate::keychain::{BlindSum, BlindingFactor, Keychain};
use crate::error::{Error, ErrorKind};
use crate::slate_versions::v0::SlateV0;
use crate::util::secp;
use crate::util::secp::key::{PublicKey, SecretKey};
use crate::util::secp::Signature;
use crate::util::RwLock;
use grin_core::core::amount_to_hr_string;
use grin_core::core::committed::Committed;
use grin_core::core::transaction::{kernel_features, kernel_sig_msg, Transaction, Weighting};
use grin_core::core::verifier_cache::LruVerifierCache;
use grin_core::libtx::{aggsig, build, secp_ser, tx_fee};
use rand::thread_rng;
use std::sync::Arc;
use uuid::Uuid;

const CURRENT_SLATE_VERSION: u64 = 1;

/// A wrapper around slates the enables support for versioning
#[derive(Serialize, Deserialize, Clone)]
#[serde(untagged)]
pub enum VersionedSlate {
	/// Pre versioning version
	V0(SlateV0),
	/// Version 1 with versioning and hex serialization - current
	V1(Slate),
}

impl From<VersionedSlate> for Slate {
	fn from(ver: VersionedSlate) -> Self {
		match ver {
			VersionedSlate::V0(slate_v0) => Slate::from(slate_v0),
			VersionedSlate::V1(slate) => slate,
		}
	}
}

/// Public data for each participant in the slate

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ParticipantData {
	/// Id of participant in the transaction. (For now, 0=sender, 1=rec)
	pub id: u64,
	/// Public key corresponding to private blinding factor
	#[serde(with = "secp_ser::pubkey_serde")]
	pub public_blind_excess: PublicKey,
	/// Public key corresponding to private nonce
	#[serde(with = "secp_ser::pubkey_serde")]
	pub public_nonce: PublicKey,
	/// Public partial signature
	#[serde(with = "secp_ser::option_sig_serde")]
	pub part_sig: Option<Signature>,
	/// A message for other participants
	pub message: Option<String>,
	/// Signature, created with private key corresponding to 'public_blind_excess'
	#[serde(with = "secp_ser::option_sig_serde")]
	pub message_sig: Option<Signature>,
}

impl ParticipantData {
	/// A helper to return whether this participant
	/// has completed round 1 and round 2;
	/// Round 1 has to be completed before instantiation of this struct
	/// anyhow, and for each participant consists of:
	/// -Inputs added to transaction
	/// -Outputs added to transaction
	/// -Public signature nonce chosen and added
	/// -Public contribution to blinding factor chosen and added
	/// Round 2 can only be completed after all participants have
	/// performed round 1, and adds:
	/// -Part sig is filled out
	pub fn is_complete(&self) -> bool {
		self.part_sig.is_some()
	}
}

/// Public message data (for serialising and storage)
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ParticipantMessageData {
	/// id of the particpant in the tx
	pub id: u64,
	/// Public key
	#[serde(with = "secp_ser::pubkey_serde")]
	pub public_key: PublicKey,
	/// Message,
	pub message: Option<String>,
	/// Signature
	#[serde(with = "secp_ser::option_sig_serde")]
	pub message_sig: Option<Signature>,
}

impl ParticipantMessageData {
	/// extract relevant message data from participant data
	pub fn from_participant_data(p: &ParticipantData) -> ParticipantMessageData {
		ParticipantMessageData {
			id: p.id,
			public_key: p.public_blind_excess,
			message: p.message.clone(),
			message_sig: p.message_sig.clone(),
		}
	}
}

/// A 'Slate' is passed around to all parties to build up all of the public
/// transaction data needed to create a finalized transaction. Callers can pass
/// the slate around by whatever means they choose, (but we can provide some
/// binary or JSON serialization helpers here).

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Slate {
	/// The number of participants intended to take part in this transaction
	pub num_participants: usize,
	/// Unique transaction ID, selected by sender
	pub id: Uuid,
	/// The core transaction data:
	/// inputs, outputs, kernels, kernel offset
	pub tx: Transaction,
	/// base amount (excluding fee)
	pub amount: u64,
	/// fee amount
	pub fee: u64,
	/// Block height for the transaction
	pub height: u64,
	/// Lock height
	pub lock_height: u64,
	/// Participant data, each participant in the transaction will
	/// insert their public data here. For now, 0 is sender and 1
	/// is receiver, though this will change for multi-party
	pub participant_data: Vec<ParticipantData>,
	/// Slate format version
	#[serde(default = "no_version")]
	pub version: u64,
}

fn no_version() -> u64 {
	0
}

/// Helper just to facilitate serialization
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ParticipantMessages {
	/// included messages
	pub messages: Vec<ParticipantMessageData>,
}

impl Slate {
	/// Create a new slate
	pub fn blank(num_participants: usize) -> Slate {
		Slate {
			num_participants: num_participants,
			id: Uuid::new_v4(),
			tx: Transaction::empty(),
			amount: 0,
			fee: 0,
			height: 0,
			lock_height: 0,
			participant_data: vec![],
			version: CURRENT_SLATE_VERSION,
		}
	}

	/// Adds selected inputs and outputs to the slate's transaction
	/// Returns blinding factor
	pub fn add_transaction_elements<K>(
		&mut self,
		keychain: &K,
		mut elems: Vec<Box<build::Append<K>>>,
	) -> Result<BlindingFactor, Error>
	where
		K: Keychain,
	{
		// Append to the exiting transaction
		if self.tx.kernels().len() != 0 {
			elems.insert(0, build::initial_tx(self.tx.clone()));
		}
		let (tx, blind) = build::partial_transaction(elems, keychain)?;
		self.tx = tx;
		Ok(blind)
	}

	/// Completes callers part of round 1, adding public key info
	/// to the slate
	pub fn fill_round_1<K>(
		&mut self,
		keychain: &K,
		sec_key: &mut SecretKey,
		sec_nonce: &SecretKey,
		participant_id: usize,
		message: Option<String>,
	) -> Result<(), Error>
	where
		K: Keychain,
	{
		// Whoever does this first generates the offset
		if self.tx.offset == BlindingFactor::zero() {
			self.generate_offset(keychain, sec_key)?;
		}
		self.add_participant_info(
			keychain,
			&sec_key,
			&sec_nonce,
			participant_id,
			None,
			message,
		)?;
		Ok(())
	}

	// This is the msg that we will sign as part of the tx kernel.
	// Currently includes the fee and the lock_height.
	fn msg_to_sign(&self) -> Result<secp::Message, Error> {
		// Currently we only support interactively creating a tx with a "default" kernel.
		let features = kernel_features(self.lock_height);
		let msg = kernel_sig_msg(self.fee, self.lock_height, features)?;
		Ok(msg)
	}

	/// Completes caller's part of round 2, completing signatures
	pub fn fill_round_2<K>(
		&mut self,
		keychain: &K,
		sec_key: &SecretKey,
		sec_nonce: &SecretKey,
		participant_id: usize,
	) -> Result<(), Error>
	where
		K: Keychain,
	{
		self.check_fees()?;

		self.verify_part_sigs(keychain.secp())?;
		let sig_part = aggsig::calculate_partial_sig(
			keychain.secp(),
			sec_key,
			sec_nonce,
			&self.pub_nonce_sum(keychain.secp())?,
			Some(&self.pub_blind_sum(keychain.secp())?),
			&self.msg_to_sign()?,
		)?;
		self.participant_data[participant_id].part_sig = Some(sig_part);
		Ok(())
	}

	/// Creates the final signature, callable by either the sender or recipient
	/// (after phase 3: sender confirmation)
	/// TODO: Only callable by receiver at the moment
	pub fn finalize<K>(&mut self, keychain: &K) -> Result<(), Error>
	where
		K: Keychain,
	{
		let final_sig = self.finalize_signature(keychain)?;
		self.finalize_transaction(keychain, &final_sig)
	}

	/// Return the sum of public nonces
	fn pub_nonce_sum(&self, secp: &secp::Secp256k1) -> Result<PublicKey, Error> {
		let pub_nonces = self
			.participant_data
			.iter()
			.map(|p| &p.public_nonce)
			.collect();
		match PublicKey::from_combination(secp, pub_nonces) {
			Ok(k) => Ok(k),
			Err(e) => Err(ErrorKind::Secp(e))?,
		}
	}

	/// Return the sum of public blinding factors
	fn pub_blind_sum(&self, secp: &secp::Secp256k1) -> Result<PublicKey, Error> {
		let pub_blinds = self
			.participant_data
			.iter()
			.map(|p| &p.public_blind_excess)
			.collect();
		match PublicKey::from_combination(secp, pub_blinds) {
			Ok(k) => Ok(k),
			Err(e) => Err(ErrorKind::Secp(e))?,
		}
	}

	/// Return vector of all partial sigs
	fn part_sigs(&self) -> Vec<&Signature> {
		self.participant_data
			.iter()
			.map(|p| p.part_sig.as_ref().unwrap())
			.collect()
	}

	/// Adds participants public keys to the slate data
	/// and saves participant's transaction context
	/// sec_key can be overridden to replace the blinding
	/// factor (by whoever split the offset)
	fn add_participant_info<K>(
		&mut self,
		keychain: &K,
		sec_key: &SecretKey,
		sec_nonce: &SecretKey,
		id: usize,
		part_sig: Option<Signature>,
		message: Option<String>,
	) -> Result<(), Error>
	where
		K: Keychain,
	{
		// Add our public key and nonce to the slate
		let pub_key = PublicKey::from_secret_key(keychain.secp(), &sec_key)?;
		let pub_nonce = PublicKey::from_secret_key(keychain.secp(), &sec_nonce)?;
		// Sign the provided message
		let message_sig = {
			if let Some(m) = message.clone() {
				let hashed = blake2b(secp::constants::MESSAGE_SIZE, &[], &m.as_bytes()[..]);
				let m = secp::Message::from_slice(&hashed.as_bytes())?;
				let res = aggsig::sign_single(&keychain.secp(), &m, &sec_key, Some(&pub_key))?;
				Some(res)
			} else {
				None
			}
		};
		self.participant_data.push(ParticipantData {
			id: id as u64,
			public_blind_excess: pub_key,
			public_nonce: pub_nonce,
			part_sig: part_sig,
			message: message,
			message_sig: message_sig,
		});
		Ok(())
	}

	/// helper to return all participant messages
	pub fn participant_messages(&self) -> ParticipantMessages {
		let mut ret = ParticipantMessages { messages: vec![] };
		for ref m in self.participant_data.iter() {
			ret.messages
				.push(ParticipantMessageData::from_participant_data(m));
		}
		ret
	}

	/// Somebody involved needs to generate an offset with their private key
	/// For now, we'll have the transaction initiator be responsible for it
	/// Return offset private key for the participant to use later in the
	/// transaction
	fn generate_offset<K>(&mut self, keychain: &K, sec_key: &mut SecretKey) -> Result<(), Error>
	where
		K: Keychain,
	{
		// Generate a random kernel offset here
		// and subtract it from the blind_sum so we create
		// the aggsig context with the "split" key
		self.tx.offset =
			BlindingFactor::from_secret_key(SecretKey::new(&keychain.secp(), &mut thread_rng()));
		let blind_offset = keychain.blind_sum(
			&BlindSum::new()
				.add_blinding_factor(BlindingFactor::from_secret_key(sec_key.clone()))
				.sub_blinding_factor(self.tx.offset),
		)?;
		*sec_key = blind_offset.secret_key(&keychain.secp())?;
		Ok(())
	}

	/// Checks the fees in the transaction in the given slate are valid
	fn check_fees(&self) -> Result<(), Error> {
		// double check the fee amount included in the partial tx
		// we don't necessarily want to just trust the sender
		// we could just overwrite the fee here (but we won't) due to the sig
		let fee = tx_fee(
			self.tx.inputs().len(),
			self.tx.outputs().len(),
			self.tx.kernels().len(),
			None,
		);
		if fee > self.tx.fee() {
			return Err(ErrorKind::Fee(
				format!("Fee Dispute Error: {}, {}", self.tx.fee(), fee,).to_string(),
			))?;
		}

		if fee > self.amount + self.fee {
			let reason = format!(
				"Rejected the transfer because transaction fee ({}) exceeds received amount ({}).",
				amount_to_hr_string(fee, false),
				amount_to_hr_string(self.amount + self.fee, false)
			);
			info!("{}", reason);
			return Err(ErrorKind::Fee(reason.to_string()))?;
		}

		Ok(())
	}

	/// Verifies all of the partial signatures in the Slate are valid
	fn verify_part_sigs(&self, secp: &secp::Secp256k1) -> Result<(), Error> {
		// collect public nonces
		for p in self.participant_data.iter() {
			if p.is_complete() {
				aggsig::verify_partial_sig(
					secp,
					p.part_sig.as_ref().unwrap(),
					&self.pub_nonce_sum(secp)?,
					&p.public_blind_excess,
					Some(&self.pub_blind_sum(secp)?),
					&self.msg_to_sign()?,
				)?;
			}
		}
		Ok(())
	}

	/// Verifies any messages in the slate's participant data match their signatures
	pub fn verify_messages(&self, secp: &secp::Secp256k1) -> Result<(), Error> {
		for p in self.participant_data.iter() {
			if let Some(msg) = &p.message {
				let hashed = blake2b(secp::constants::MESSAGE_SIZE, &[], &msg.as_bytes()[..]);
				let m = secp::Message::from_slice(&hashed.as_bytes())?;
				let signature = match p.message_sig {
					None => {
						error!("verify_messages - participant message doesn't have signature. Message: \"{}\"",
						   String::from_utf8_lossy(&msg.as_bytes()[..]));
						return Err(ErrorKind::Signature(
							"Optional participant messages doesn't have signature".to_owned(),
						))?;
					}
					Some(s) => s,
				};
				if !aggsig::verify_single(
					secp,
					&signature,
					&m,
					None,
					&p.public_blind_excess,
					Some(&p.public_blind_excess),
					false,
				) {
					error!("verify_messages - participant message doesn't match signature. Message: \"{}\"",
						   String::from_utf8_lossy(&msg.as_bytes()[..]));
					return Err(ErrorKind::Signature(
						"Optional participant messages do not match signatures".to_owned(),
					))?;
				} else {
					info!(
						"verify_messages - signature verified ok. Participant message: \"{}\"",
						String::from_utf8_lossy(&msg.as_bytes()[..])
					);
				}
			}
		}
		Ok(())
	}

	/// This should be callable by either the sender or receiver
	/// once phase 3 is done
	///
	/// Receive Part 3 of interactive transactions from sender, Sender
	/// Confirmation Return Ok/Error
	/// -Receiver receives sS
	/// -Receiver verifies sender's sig, by verifying that
	/// kS * G + e *xS * G = sS* G
	/// -Receiver calculates final sig as s=(sS+sR, kS * G+kR * G)
	/// -Receiver puts into TX kernel:
	///
	/// Signature S
	/// pubkey xR * G+xS * G
	/// fee (= M)
	///
	/// Returns completed transaction ready for posting to the chain

	fn finalize_signature<K>(&mut self, keychain: &K) -> Result<Signature, Error>
	where
		K: Keychain,
	{
		self.verify_part_sigs(keychain.secp())?;

		let part_sigs = self.part_sigs();
		let pub_nonce_sum = self.pub_nonce_sum(keychain.secp())?;
		let final_pubkey = self.pub_blind_sum(keychain.secp())?;
		// get the final signature
		let final_sig = aggsig::add_signatures(&keychain.secp(), part_sigs, &pub_nonce_sum)?;

		// Calculate the final public key (for our own sanity check)

		// Check our final sig verifies
		aggsig::verify_completed_sig(
			&keychain.secp(),
			&final_sig,
			&final_pubkey,
			Some(&final_pubkey),
			&self.msg_to_sign()?,
		)?;

		Ok(final_sig)
	}

	/// builds a final transaction after the aggregated sig exchange
	fn finalize_transaction<K>(
		&mut self,
		keychain: &K,
		final_sig: &secp::Signature,
	) -> Result<(), Error>
	where
		K: Keychain,
	{
		let kernel_offset = self.tx.offset;

		self.check_fees()?;

		let mut final_tx = self.tx.clone();

		// build the final excess based on final tx and offset
		let final_excess = {
			// sum the input/output commitments on the final tx
			let overage = final_tx.fee() as i64;
			let tx_excess = final_tx.sum_commitments(overage)?;

			// subtract the kernel_excess (built from kernel_offset)
			let offset_excess = keychain
				.secp()
				.commit(0, kernel_offset.secret_key(&keychain.secp())?)?;
			keychain
				.secp()
				.commit_sum(vec![tx_excess], vec![offset_excess])?
		};

		// update the tx kernel to reflect the offset excess and sig
		assert_eq!(final_tx.kernels().len(), 1);
		final_tx.kernels_mut()[0].excess = final_excess.clone();
		final_tx.kernels_mut()[0].excess_sig = final_sig.clone();

		// confirm the kernel verifies successfully before proceeding
		debug!("Validating final transaction");
		final_tx.kernels()[0].verify()?;

		// confirm the overall transaction is valid (including the updated kernel)
		// accounting for tx weight limits
		let verifier_cache = Arc::new(RwLock::new(LruVerifierCache::new()));
		let _ = final_tx.validate(Weighting::AsTransaction, verifier_cache)?;

		self.tx = final_tx;
		Ok(())
	}
}