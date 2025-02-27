// Copyright 2019 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate.  If not, see <http://www.gnu.org/licenses/>.

//! Consensus extension module for BABE consensus. Collects on-chain randomness
//! from VRF outputs and manages epoch transitions.

#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unused_must_use, unsafe_code, unused_variables, unused_must_use)]
#![deny(unused_imports)]
pub use timestamp;

use rstd::{result, prelude::*};
use support::{decl_storage, decl_module, traits::FindAuthor, traits::Get};
use timestamp::OnTimestampSet;
use sr_primitives::{generic::DigestItem, ConsensusEngineId, Perbill};
use sr_primitives::traits::{IsMember, SaturatedConversion, Saturating, RandomnessBeacon};
use sr_staking_primitives::{
	SessionIndex,
	offence::{Offence, Kind},
};
#[cfg(feature = "std")]
use timestamp::TimestampInherentData;
use codec::{Encode, Decode};
use inherents::{RuntimeString, InherentIdentifier, InherentData, ProvideInherent, MakeFatalError};
#[cfg(feature = "std")]
use inherents::{InherentDataProviders, ProvideInherentData};
use babe_primitives::{
	BABE_ENGINE_ID, ConsensusLog, BabeAuthorityWeight, NextEpochDescriptor, RawBabePreDigest,
	SlotNumber,
};
pub use babe_primitives::{AuthorityId, VRF_OUTPUT_LENGTH, PUBLIC_KEY_LENGTH};

#[cfg(all(feature = "std", test))]
mod tests;

#[cfg(all(feature = "std", test))]
mod mock;

/// The BABE inherent identifier.
pub const INHERENT_IDENTIFIER: InherentIdentifier = *b"babeslot";

/// The type of the BABE inherent.
pub type InherentType = u64;
/// Auxiliary trait to extract BABE inherent data.
pub trait BabeInherentData {
	/// Get BABE inherent data.
	fn babe_inherent_data(&self) -> result::Result<InherentType, RuntimeString>;
	/// Replace BABE inherent data.
	fn babe_replace_inherent_data(&mut self, new: InherentType);
}

impl BabeInherentData for InherentData {
	fn babe_inherent_data(&self) -> result::Result<InherentType, RuntimeString> {
		self.get_data(&INHERENT_IDENTIFIER)
			.and_then(|r| r.ok_or_else(|| "BABE inherent data not found".into()))
	}

	fn babe_replace_inherent_data(&mut self, new: InherentType) {
		self.replace_data(INHERENT_IDENTIFIER, &new);
	}
}

/// Provides the slot duration inherent data for BABE.
#[cfg(feature = "std")]
pub struct InherentDataProvider {
	slot_duration: u64,
}

#[cfg(feature = "std")]
impl InherentDataProvider {
	/// Constructs `Self`
	pub fn new(slot_duration: u64) -> Self {
		Self {
			slot_duration
		}
	}
}

#[cfg(feature = "std")]
impl ProvideInherentData for InherentDataProvider {
	fn on_register(
		&self,
		providers: &InherentDataProviders,
	) -> result::Result<(), RuntimeString> {
		if !providers.has_provider(&timestamp::INHERENT_IDENTIFIER) {
			// Add the timestamp inherent data provider, as we require it.
			providers.register_provider(timestamp::InherentDataProvider)
		} else {
			Ok(())
		}
	}

	fn inherent_identifier(&self) -> &'static inherents::InherentIdentifier {
		&INHERENT_IDENTIFIER
	}

	fn provide_inherent_data(
		&self,
		inherent_data: &mut InherentData,
	) -> result::Result<(), RuntimeString> {
		let timestamp = inherent_data.timestamp_inherent_data()?;
		let slot_number = timestamp / self.slot_duration;
		inherent_data.put_data(INHERENT_IDENTIFIER, &slot_number)
	}

	fn error_to_string(&self, error: &[u8]) -> Option<String> {
		RuntimeString::decode(&mut &error[..]).map(Into::into).ok()
	}
}

pub trait Trait: timestamp::Trait {
	/// The amount of time, in slots, that each epoch should last.
	type EpochDuration: Get<SlotNumber>;

	/// The expected average block time at which BABE should be creating
	/// blocks. Since BABE is probabilistic it is not trivial to figure out
	/// what the expected average block time should be based on the slot
	/// duration and the security parameter `c` (where `1 - c` represents
	/// the probability of a slot being empty).
	type ExpectedBlockTime: Get<Self::Moment>;

	/// BABE requires some logic to be triggered on every block to query for whether an epoch
	/// has ended and to perform the transition to the next epoch.
	///
	/// Typically, the `ExternalTrigger` type should be used. An internal trigger should only be used
	/// when no other module is responsible for changing authority set.
	type EpochChangeTrigger: EpochChangeTrigger;
}

/// Trigger an epoch change, if any should take place.
pub trait EpochChangeTrigger {
	/// Trigger an epoch change, if any should take place. This should be called
	/// during every block, after initialization is done.
	fn trigger<T: Trait>(now: T::BlockNumber);
}

/// A type signifying to BABE that an external trigger
/// for epoch changes (e.g. srml-session) is used.
pub struct ExternalTrigger;

impl EpochChangeTrigger for ExternalTrigger {
	fn trigger<T: Trait>(_: T::BlockNumber) { } // nothing - trigger is external.
}

/// A type signifying to BABE that it should perform epoch changes
/// with an internal trigger, recycling the same authorities forever.
pub struct SameAuthoritiesForever;

impl EpochChangeTrigger for SameAuthoritiesForever {
	fn trigger<T: Trait>(now: T::BlockNumber) {
		if <Module<T>>::should_epoch_change(now) {
			let authorities = <Module<T>>::authorities();
			let next_authorities = authorities.clone();

			<Module<T>>::enact_epoch_change(authorities, next_authorities);
		}
	}
}

/// The length of the BABE randomness
pub const RANDOMNESS_LENGTH: usize = 32;

const UNDER_CONSTRUCTION_SEGMENT_LENGTH: usize = 256;

type MaybeVrf = Option<[u8; 32 /* VRF_OUTPUT_LENGTH */]>;

decl_storage! {
	trait Store for Module<T: Trait> as Babe {
		/// Current epoch index.
		pub EpochIndex get(fn epoch_index): u64;

		/// Current epoch authorities.
		pub Authorities get(fn authorities): Vec<(AuthorityId, BabeAuthorityWeight)>;

		/// The slot at which the first epoch actually started. This is 0
		/// until the first block of the chain.
		pub GenesisSlot get(fn genesis_slot): u64;

		/// Current slot number.
		pub CurrentSlot get(fn current_slot): u64;

		/// The epoch randomness for the *current* epoch.
		///
		/// # Security
		///
		/// This MUST NOT be used for gambling, as it can be influenced by a
		/// malicious validator in the short term. It MAY be used in many
		/// cryptographic protocols, however, so long as one remembers that this
		/// (like everything else on-chain) it is public. For example, it can be
		/// used where a number is needed that cannot have been chosen by an
		/// adversary, for purposes such as public-coin zero-knowledge proofs.
		// NOTE: the following fields don't use the constants to define the
		// array size because the metadata API currently doesn't resolve the
		// variable to its underlying value.
		pub Randomness get(fn randomness): [u8; 32 /* RANDOMNESS_LENGTH */];

		/// Next epoch randomness.
		NextRandomness: [u8; 32 /* RANDOMNESS_LENGTH */];

		/// Randomness under construction.
		///
		/// We make a tradeoff between storage accesses and list length.
		/// We store the under-construction randomness in segments of up to
		/// `UNDER_CONSTRUCTION_SEGMENT_LENGTH`.
		///
		/// Once a segment reaches this length, we begin the next one.
		/// We reset all segments and return to `0` at the beginning of every
		/// epoch.
		SegmentIndex build(|_| 0): u32;
		UnderConstruction: map u32 => Vec<[u8; 32 /* VRF_OUTPUT_LENGTH */]>;

		/// Temporary value (cleared at block finalization) which is `Some`
		/// if per-block initialization has already been called for current block.
		Initialized get(fn initialized): Option<MaybeVrf>;
	}
	add_extra_genesis {
		config(authorities): Vec<(AuthorityId, BabeAuthorityWeight)>;
		build(|config| Module::<T>::initialize_authorities(&config.authorities))
	}
}

decl_module! {
	/// The BABE SRML module
	pub struct Module<T: Trait> for enum Call where origin: T::Origin {
		/// The number of **slots** that an epoch takes. We couple sessions to
		/// epochs, i.e. we start a new session once the new epoch begins.
		const EpochDuration: u64 = T::EpochDuration::get();

		/// The expected average block time at which BABE should be creating
		/// blocks. Since BABE is probabilistic it is not trivial to figure out
		/// what the expected average block time should be based on the slot
		/// duration and the security parameter `c` (where `1 - c` represents
		/// the probability of a slot being empty).
		const ExpectedBlockTime: T::Moment = T::ExpectedBlockTime::get();

		/// Initialization
		fn on_initialize(now: T::BlockNumber) {
			Self::do_initialize(now);
		}

		/// Block finalization
		fn on_finalize() {
			// at the end of the block, we can safely include the new VRF output
			// from this block into the under-construction randomness. If we've determined
			// that this block was the first in a new epoch, the changeover logic has
			// already occurred at this point, so the under-construction randomness
			// will only contain outputs from the right epoch.
			if let Some(Some(vrf_output)) = Initialized::take() {
				Self::deposit_vrf_output(&vrf_output);
			}
		}
	}
}

impl<T: Trait> RandomnessBeacon for Module<T> {
	fn random() -> [u8; VRF_OUTPUT_LENGTH] {
		Self::randomness()
	}
}

/// A BABE public key
pub type BabeKey = [u8; PUBLIC_KEY_LENGTH];

impl<T: Trait> FindAuthor<u32> for Module<T> {
	fn find_author<'a, I>(digests: I) -> Option<u32> where
		I: 'a + IntoIterator<Item=(ConsensusEngineId, &'a [u8])>
	{
		for (id, mut data) in digests.into_iter() {
			if id == BABE_ENGINE_ID {
				let pre_digest = RawBabePreDigest::decode(&mut data).ok()?;
				return Some(match pre_digest {
					RawBabePreDigest::Primary { authority_index, .. } =>
						authority_index,
					RawBabePreDigest::Secondary { authority_index, .. } =>
						authority_index,
				});
			}
		}

		return None;
	}
}

impl<T: Trait> IsMember<AuthorityId> for Module<T> {
	fn is_member(authority_id: &AuthorityId) -> bool {
		<Module<T>>::authorities()
			.iter()
			.any(|id| &id.0 == authority_id)
	}
}

impl<T: Trait> session::ShouldEndSession<T::BlockNumber> for Module<T> {
	fn should_end_session(now: T::BlockNumber) -> bool {
		// it might be (and it is in current implementation) that session module is calling
		// should_end_session() from it's own on_initialize() handler
		// => because session on_initialize() is called earlier than ours, let's ensure
		// that we have synced with digest before checking if session should be ended.
		Self::do_initialize(now);

		Self::should_epoch_change(now)
	}
}

// TODO [slashing]: @marcio use this, remove the dead_code annotation.
/// A BABE equivocation offence report.
///
/// When a validator released two or more blocks at the same slot.
#[allow(dead_code)]
struct BabeEquivocationOffence<FullIdentification> {
	/// A babe slot number in which this incident happened.
	slot: u64,
	/// The session index in which the incident happened.
	session_index: SessionIndex,
	/// The size of the validator set at the time of the offence.
	validator_set_count: u32,
	/// The authority that produced the equivocation.
	offender: FullIdentification,
}

impl<FullIdentification: Clone> Offence<FullIdentification> for BabeEquivocationOffence<FullIdentification> {
	const ID: Kind = *b"babe:equivocatio";
	type TimeSlot = u64;

	fn offenders(&self) -> Vec<FullIdentification> {
		vec![self.offender.clone()]
	}

	fn session_index(&self) -> SessionIndex {
		self.session_index
	}

	fn validator_set_count(&self) -> u32 {
		self.validator_set_count
	}

	fn time_slot(&self) -> Self::TimeSlot {
		self.slot
	}

	fn slash_fraction(
		offenders_count: u32,
		validator_set_count: u32,
	) -> Perbill {
		// the formula is min((3k / n)^2, 1)
		let x = Perbill::from_rational_approximation(3 * offenders_count, validator_set_count);
		// _ ^ 2
		x.square()
	}
}

impl<T: Trait> Module<T> {
	/// Determine the BABE slot duration based on the Timestamp module configuration.
	pub fn slot_duration() -> T::Moment {
		// we double the minimum block-period so each author can always propose within
		// the majority of their slot.
		<T as timestamp::Trait>::MinimumPeriod::get().saturating_mul(2.into())
	}

	/// Determine whether an epoch change should take place at this block.
	/// Assumes that initialization has already taken place.
	pub fn should_epoch_change(now: T::BlockNumber) -> bool {
		// The epoch has technically ended during the passage of time
		// between this block and the last, but we have to "end" the epoch now,
		// since there is no earlier possible block we could have done it.
		//
		// The exception is for block 1: the genesis has slot 0, so we treat
		// epoch 0 as having started at the slot of block 1. We want to use
		// the same randomness and validator set as signalled in the genesis,
		// so we don't rotate the epoch.
		now != sr_primitives::traits::One::one() && {
			let diff = CurrentSlot::get().saturating_sub(Self::current_epoch_start());
			diff >= T::EpochDuration::get()
		}
	}

	/// DANGEROUS: Enact an epoch change. Should be done on every block where `should_epoch_change` has returned `true`,
	/// and the caller is the only caller of this function.
	///
	/// Typically, this is not handled directly by the user, but by higher-level validator-set manager logic like
	/// `srml-session`.
	pub fn enact_epoch_change(
		authorities: Vec<(AuthorityId, BabeAuthorityWeight)>,
		next_authorities: Vec<(AuthorityId, BabeAuthorityWeight)>,
	) {
		// PRECONDITION: caller has done initialization and is guaranteed
		// by the session module to be called before this.
		#[cfg(debug_assertions)]
		{
			assert!(Self::initialized().is_some())
		}

		// Update epoch index
		let epoch_index = EpochIndex::get()
			.checked_add(1)
			.expect("epoch indices will never reach 2^64 before the death of the universe; qed");

		EpochIndex::put(epoch_index);
		Authorities::put(authorities);

		// Update epoch randomness.
		let next_epoch_index = epoch_index
			.checked_add(1)
			.expect("epoch indices will never reach 2^64 before the death of the universe; qed");

		// Returns randomness for the current epoch and computes the *next*
		// epoch randomness.
		let randomness = Self::randomness_change_epoch(next_epoch_index);
		Randomness::put(randomness);

		// After we update the current epoch, we signal the *next* epoch change
		// so that nodes can track changes.
		let next_randomness = NextRandomness::get();

		let next = NextEpochDescriptor {
			authorities: next_authorities,
			randomness: next_randomness,
		};

		Self::deposit_consensus(ConsensusLog::NextEpochData(next))
	}

	// finds the start slot of the current epoch. only guaranteed to
	// give correct results after `do_initialize` of the first block
	// in the chain (as its result is based off of `GenesisSlot`).
	fn current_epoch_start() -> SlotNumber {
		(EpochIndex::get() * T::EpochDuration::get()) + GenesisSlot::get()
	}

	fn deposit_consensus<U: Encode>(new: U) {
		let log: DigestItem<T::Hash> = DigestItem::Consensus(BABE_ENGINE_ID, new.encode());
		<system::Module<T>>::deposit_log(log.into())
	}

	fn deposit_vrf_output(vrf_output: &[u8; VRF_OUTPUT_LENGTH]) {
		let segment_idx = <SegmentIndex>::get();
		let mut segment = <UnderConstruction>::get(&segment_idx);
		if segment.len() < UNDER_CONSTRUCTION_SEGMENT_LENGTH {
			// push onto current segment: not full.
			segment.push(*vrf_output);
			<UnderConstruction>::insert(&segment_idx, &segment);
		} else {
			// move onto the next segment and update the index.
			let segment_idx = segment_idx + 1;
			<UnderConstruction>::insert(&segment_idx, &vec![*vrf_output]);
			<SegmentIndex>::put(&segment_idx);
		}
	}

	fn do_initialize(now: T::BlockNumber) {
		// since do_initialize can be called twice (if session module is present)
		// => let's ensure that we only modify the storage once per block
		let initialized = Self::initialized().is_some();
		if initialized {
			return;
		}

		let maybe_pre_digest = <system::Module<T>>::digest()
			.logs
			.iter()
			.filter_map(|s| s.as_pre_runtime())
			.filter_map(|(id, mut data)| if id == BABE_ENGINE_ID {
				RawBabePreDigest::decode(&mut data).ok()
			} else {
				None
			})
			.next();

		let maybe_vrf = maybe_pre_digest.and_then(|digest| {
			// on the first non-zero block (i.e. block #1)
			// this is where the first epoch (epoch #0) actually starts.
			// we need to adjust internal storage accordingly.
			if GenesisSlot::get() == 0 {
				GenesisSlot::put(digest.slot_number());
				debug_assert_ne!(GenesisSlot::get(), 0);

				// deposit a log because this is the first block in epoch #0
				// we use the same values as genesis because we haven't collected any
				// randomness yet.
				let next = NextEpochDescriptor {
					authorities: Self::authorities(),
					randomness: Self::randomness(),
				};

				Self::deposit_consensus(ConsensusLog::NextEpochData(next))
			}

			CurrentSlot::put(digest.slot_number());

			if let RawBabePreDigest::Primary { vrf_output, .. } = digest {
				// place the VRF output into the `Initialized` storage item
				// and it'll be put onto the under-construction randomness
				// later, once we've decided which epoch this block is in.
				Some(vrf_output)
			} else {
				None
			}
		});

		Initialized::put(maybe_vrf);

		// enact epoch change, if necessary.
		T::EpochChangeTrigger::trigger::<T>(now)
	}

	/// Call this function exactly once when an epoch changes, to update the
	/// randomness. Returns the new randomness.
	fn randomness_change_epoch(next_epoch_index: u64) -> [u8; RANDOMNESS_LENGTH] {
		let this_randomness = NextRandomness::get();
		let segment_idx: u32 = <SegmentIndex>::mutate(|s| rstd::mem::replace(s, 0));

		// overestimate to the segment being full.
		let rho_size = segment_idx.saturating_add(1) as usize * UNDER_CONSTRUCTION_SEGMENT_LENGTH;

		let next_randomness = compute_randomness(
			this_randomness,
			next_epoch_index,
			(0..segment_idx).flat_map(|i| <UnderConstruction>::take(&i)),
			Some(rho_size),
		);
		NextRandomness::put(&next_randomness);
		this_randomness
	}

	fn initialize_authorities(authorities: &[(AuthorityId, BabeAuthorityWeight)]) {
		if !authorities.is_empty() {
			assert!(Authorities::get().is_empty(), "Authorities are already initialized!");
			Authorities::put(authorities);
		}
	}
}

impl<T: Trait> OnTimestampSet<T::Moment> for Module<T> {
	fn on_timestamp_set(_moment: T::Moment) { }
}

impl<T: Trait> session::OneSessionHandler<T::AccountId> for Module<T> {
	type Key = AuthorityId;

	fn on_genesis_session<'a, I: 'a>(validators: I)
		where I: Iterator<Item=(&'a T::AccountId, AuthorityId)>
	{
		let authorities = validators.map(|(_, k)| (k, 1)).collect::<Vec<_>>();
		Self::initialize_authorities(&authorities);
	}

	fn on_new_session<'a, I: 'a>(_changed: bool, validators: I, queued_validators: I)
		where I: Iterator<Item=(&'a T::AccountId, AuthorityId)>
	{
		let authorities = validators.map(|(_account, k)| {
			(k, 1)
		}).collect::<Vec<_>>();

		let next_authorities = queued_validators.map(|(_account, k)| {
			(k, 1)
		}).collect::<Vec<_>>();

		Self::enact_epoch_change(authorities, next_authorities)
	}

	fn on_disabled(i: usize) {
		Self::deposit_consensus(ConsensusLog::OnDisabled(i as u32))
	}
}

// compute randomness for a new epoch. rho is the concatenation of all
// VRF outputs in the prior epoch.
//
// an optional size hint as to how many VRF outputs there were may be provided.
fn compute_randomness(
	last_epoch_randomness: [u8; RANDOMNESS_LENGTH],
	epoch_index: u64,
	rho: impl Iterator<Item=[u8; VRF_OUTPUT_LENGTH]>,
	rho_size_hint: Option<usize>,
) -> [u8; RANDOMNESS_LENGTH] {
	let mut s = Vec::with_capacity(40 + rho_size_hint.unwrap_or(0) * VRF_OUTPUT_LENGTH);
	s.extend_from_slice(&last_epoch_randomness);
	s.extend_from_slice(&epoch_index.to_le_bytes());

	for vrf_output in rho {
		s.extend_from_slice(&vrf_output[..]);
	}

	runtime_io::blake2_256(&s)
}

impl<T: Trait> ProvideInherent for Module<T> {
	type Call = timestamp::Call<T>;
	type Error = MakeFatalError<RuntimeString>;
	const INHERENT_IDENTIFIER: InherentIdentifier = INHERENT_IDENTIFIER;

	fn create_inherent(_: &InherentData) -> Option<Self::Call> {
		None
	}

	fn check_inherent(call: &Self::Call, data: &InherentData) -> result::Result<(), Self::Error> {
		let timestamp = match call {
			timestamp::Call::set(ref timestamp) => timestamp.clone(),
			_ => return Ok(()),
		};

		let timestamp_based_slot = (timestamp / Self::slot_duration()).saturated_into::<u64>();
		let seal_slot = data.babe_inherent_data()?;

		if timestamp_based_slot == seal_slot {
			Ok(())
		} else {
			Err(RuntimeString::from("timestamp set in block doesn't match slot in seal").into())
		}
	}
}
