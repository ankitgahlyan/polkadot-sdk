// Copyright 2019-2020 Parity Technologies (UK) Ltd.
// This file is part of Parity Bridges Common.

// Parity Bridges Common is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity Bridges Common is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity Bridges Common.  If not, see <http://www.gnu.org/licenses/>.

//! Runtime module that allows sending and receiving messages using lane concept:
//!
//! 1) the message is sent using `send_message()` call;
//! 2) every outbound message is assigned nonce;
//! 3) the messages are stored in the storage;
//! 4) external component (relay) delivers messages to bridged chain;
//! 5) messages are processed in order (ordered by assigned nonce);
//! 6) relay may send proof-of-delivery back to this chain.
//!
//! Once message is sent, its progress can be tracked by looking at module events.
//! The assigned nonce is reported using `MessageAccepted` event. When message is
//! delivered to the the bridged chain, it is reported using `MessagesDelivered` event.

#![cfg_attr(not(feature = "std"), no_std)]

use crate::inbound_lane::{InboundLane, InboundLaneStorage};
use crate::outbound_lane::{OutboundLane, OutboundLaneStorage};

use bp_message_lane::{
	source_chain::{LaneMessageVerifier, MessageDeliveryAndDispatchPayment, TargetHeaderChain},
	target_chain::{DispatchMessage, MessageDispatch, ProvedLaneMessages, ProvedMessages, SourceHeaderChain},
	InboundLaneData, LaneId, MessageData, MessageKey, MessageNonce, OutboundLaneData,
};
use codec::{Decode, Encode};
use frame_support::{
	decl_error, decl_event, decl_module, decl_storage, sp_runtime::DispatchResult, traits::Get, weights::Weight,
	Parameter, StorageMap,
};
use frame_system::ensure_signed;
use sp_std::{cell::RefCell, marker::PhantomData, prelude::*};

mod inbound_lane;
mod outbound_lane;

pub mod instant_payments;

#[cfg(test)]
mod mock;

// TODO: update me (https://github.com/paritytech/parity-bridges-common/issues/78)
/// Upper bound of delivery transaction weight.
const DELIVERY_BASE_WEIGHT: Weight = 0;

/// The module configuration trait
pub trait Trait<I = DefaultInstance>: frame_system::Trait {
	// General types

	/// They overarching event type.
	type Event: From<Event<Self, I>> + Into<<Self as frame_system::Trait>::Event>;
	/// Maximal number of messages that may be pruned during maintenance. Maintenance occurs
	/// whenever outbound lane is updated - i.e. when new message is sent, or receival is
	/// confirmed. The reason is that if you want to use lane, you should be ready to pay
	/// for it.
	type MaxMessagesToPruneAtOnce: Get<MessageNonce>;
	/// Maximal number of "messages" (see note below) in the 'unconfirmed' state at inbound lane.
	/// Unconfirmed message at inbound lane is the message that has been: sent, delivered and
	/// dispatched. Its delivery confirmation is still pending. This limit is introduced to bound
	/// maximal number of relayers-ids in the inbound lane state.
	///
	/// "Message" in this context does not necessarily mean an individual message, but instead
	/// continuous range of individual messages, that are delivered by single relayer. So if relayer#1
	/// has submitted delivery transaction#1 with individual messages [1; 2] and then delivery
	/// transaction#2 with individual messages [3; 4], this would be treated as single "Message" and
	/// would occupy single unit of `MaxUnconfirmedMessagesAtInboundLane` limit.
	type MaxUnconfirmedMessagesAtInboundLane: Get<MessageNonce>;

	/// Payload type of outbound messages. This payload is dispatched on the bridged chain.
	type OutboundPayload: Parameter;
	/// Message fee type of outbound messages. This fee is paid on this chain.
	type OutboundMessageFee: Parameter;

	/// Payload type of inbound messages. This payload is dispatched on this chain.
	type InboundPayload: Decode;
	/// Message fee type of inbound messages. This fee is paid on the bridged chain.
	type InboundMessageFee: Decode;
	/// Identifier of relayer that deliver messages to this chain. Relayer reward is paid on the bridged chain.
	type InboundRelayer: Parameter;

	// Types that are used by outbound_lane (on source chain).

	/// Target header chain.
	type TargetHeaderChain: TargetHeaderChain<Self::OutboundPayload, Self::AccountId>;
	/// Message payload verifier.
	type LaneMessageVerifier: LaneMessageVerifier<Self::AccountId, Self::OutboundPayload, Self::OutboundMessageFee>;
	/// Message delivery payment.
	type MessageDeliveryAndDispatchPayment: MessageDeliveryAndDispatchPayment<Self::AccountId, Self::OutboundMessageFee>;

	// Types that are used by inbound_lane (on target chain).

	/// Source header chain, as it is represented on target chain.
	type SourceHeaderChain: SourceHeaderChain<Self::InboundMessageFee>;
	/// Message dispatch.
	type MessageDispatch: MessageDispatch<Self::InboundMessageFee, DispatchPayload = Self::InboundPayload>;
}

/// Shortcut to messages proof type for Trait.
type MessagesProofOf<T, I> =
	<<T as Trait<I>>::SourceHeaderChain as SourceHeaderChain<<T as Trait<I>>::InboundMessageFee>>::MessagesProof;
/// Shortcut to messages delivery proof type for Trait.
type MessagesDeliveryProofOf<T, I> = <<T as Trait<I>>::TargetHeaderChain as TargetHeaderChain<
	<T as Trait<I>>::OutboundPayload,
	<T as frame_system::Trait>::AccountId,
>>::MessagesDeliveryProof;

decl_error! {
	pub enum Error for Module<T: Trait<I>, I: Instance> {
		/// Message has been treated as invalid by chain verifier.
		MessageRejectedByChainVerifier,
		/// Message has been treated as invalid by lane verifier.
		MessageRejectedByLaneVerifier,
		/// Submitter has failed to pay fee for delivering and dispatching messages.
		FailedToWithdrawMessageFee,
		/// Invalid messages has been submitted.
		InvalidMessagesProof,
		/// Invalid messages dispatch weight has been declared by the relayer.
		InvalidMessagesDispatchWeight,
		/// Invalid messages delivery proof has been submitted.
		InvalidMessagesDeliveryProof,
	}
}

decl_storage! {
	trait Store for Module<T: Trait<I>, I: Instance = DefaultInstance> as MessageLane {
		/// Map of lane id => inbound lane data.
		pub InboundLanes: map hasher(blake2_128_concat) LaneId => InboundLaneData<T::InboundRelayer>;
		/// Map of lane id => outbound lane data.
		pub OutboundLanes: map hasher(blake2_128_concat) LaneId => OutboundLaneData;
		/// All queued outbound messages.
		pub OutboundMessages: map hasher(blake2_128_concat) MessageKey => Option<MessageData<T::OutboundMessageFee>>;
	}
}

decl_event!(
	pub enum Event<T, I = DefaultInstance> where
		<T as frame_system::Trait>::AccountId,
	{
		/// Message has been accepted and is waiting to be delivered.
		MessageAccepted(LaneId, MessageNonce),
		/// Messages in the inclusive range have been delivered and processed by the bridged chain.
		MessagesDelivered(LaneId, MessageNonce, MessageNonce),
		/// Phantom member, never used.
		Dummy(PhantomData<(AccountId, I)>),
	}
);

decl_module! {
	pub struct Module<T: Trait<I>, I: Instance = DefaultInstance> for enum Call where origin: T::Origin {
		/// Deposit one of this module's events by using the default implementation.
		fn deposit_event() = default;

		/// Send message over lane.
		#[weight = 0] // TODO: update me (https://github.com/paritytech/parity-bridges-common/issues/78)
		pub fn send_message(
			origin,
			lane_id: LaneId,
			payload: T::OutboundPayload,
			delivery_and_dispatch_fee: T::OutboundMessageFee,
		) -> DispatchResult {
			let submitter = ensure_signed(origin)?;

			// let's first check if message can be delivered to target chain
			T::TargetHeaderChain::verify_message(&payload).map_err(|err| {
				frame_support::debug::trace!(
					target: "runtime",
					"Message to lane {:?} is rejected by target chain: {:?}",
					lane_id,
					err,
				);

				Error::<T, I>::MessageRejectedByChainVerifier
			})?;

			// now let's enforce any additional lane rules
			T::LaneMessageVerifier::verify_message(
				&submitter,
				&delivery_and_dispatch_fee,
				&lane_id,
				&payload,
			).map_err(|err| {
				frame_support::debug::trace!(
					target: "runtime",
					"Message to lane {:?} is rejected by lane verifier: {:?}",
					lane_id,
					err,
				);

				Error::<T, I>::MessageRejectedByLaneVerifier
			})?;

			// let's withdraw delivery and dispatch fee from submitter
			T::MessageDeliveryAndDispatchPayment::pay_delivery_and_dispatch_fee(
				&submitter,
				&delivery_and_dispatch_fee,
			).map_err(|err| {
				frame_support::debug::trace!(
					target: "runtime",
					"Message to lane {:?} is rejected because submitter {:?} is unable to pay fee {:?}: {:?}",
					lane_id,
					submitter,
					delivery_and_dispatch_fee,
					err,
				);

				Error::<T, I>::FailedToWithdrawMessageFee
			})?;

			// finally, save message in outbound storage and emit event
			let mut lane = outbound_lane::<T, I>(lane_id);
			let nonce = lane.send_message(MessageData {
				payload: payload.encode(),
				fee: delivery_and_dispatch_fee,
			});
			lane.prune_messages(T::MaxMessagesToPruneAtOnce::get());

			frame_support::debug::trace!(
				target: "runtime",
				"Accepted message {} to lane {:?}",
				nonce,
				lane_id,
			);

			Self::deposit_event(RawEvent::MessageAccepted(lane_id, nonce));

			Ok(())
		}

		/// Receive messages proof from bridged chain.
		#[weight = DELIVERY_BASE_WEIGHT + dispatch_weight]
		pub fn receive_messages_proof(
			origin,
			relayer_id: T::InboundRelayer,
			proof: MessagesProofOf<T, I>,
			dispatch_weight: Weight,
		) -> DispatchResult {
			let _ = ensure_signed(origin)?;

			// verify messages proof && convert proof into messages
			let messages = verify_and_decode_messages_proof::<T::SourceHeaderChain, T::InboundMessageFee, T::InboundPayload>(proof)
				.map_err(|err| {
					frame_support::debug::trace!(
						target: "runtime",
						"Rejecting invalid messages proof: {:?}",
						err,
					);

					Error::<T, I>::InvalidMessagesProof
				})?;

			// verify that relayer is paying actual dispatch weight
			let actual_dispatch_weight: Weight = messages
				.values()
				.map(|lane_messages| lane_messages
					.messages
					.iter()
					.map(T::MessageDispatch::dispatch_weight)
					.sum::<Weight>()
				)
				.sum();
			if dispatch_weight < actual_dispatch_weight {
				frame_support::debug::trace!(
					target: "runtime",
					"Rejecting messages proof because of dispatch weight mismatch: declared={}, expected={}",
					dispatch_weight,
					actual_dispatch_weight,
				);

				return Err(Error::<T, I>::InvalidMessagesDispatchWeight.into());
			}

			// dispatch messages and (optionally) update lane(s) state(s)
			let mut total_messages = 0;
			let mut valid_messages = 0;
			for (lane_id, lane_data) in messages {
				let mut lane = inbound_lane::<T, I>(lane_id);

				if let Some(lane_state) = lane_data.lane_state {
					let updated_latest_confirmed_nonce = lane.receive_state_update(lane_state);
					if let Some(updated_latest_confirmed_nonce) = updated_latest_confirmed_nonce {
						frame_support::debug::trace!(
							target: "runtime",
							"Received lane {:?} state update: latest_confirmed_nonce={}",
							lane_id,
							updated_latest_confirmed_nonce,
						);
					}
				}

				for message in lane_data.messages {
					debug_assert_eq!(message.key.lane_id, lane_id);

					total_messages += 1;
					if lane.receive_message::<T::MessageDispatch>(relayer_id.clone(), message.key.nonce, message.data) {
						valid_messages += 1;
					}
				}
			}

			frame_support::debug::trace!(
				target: "runtime",
				"Received messages: total={}, valid={}",
				total_messages,
				valid_messages,
			);

			Ok(())
		}

		/// Receive messages delivery proof from bridged chain.
		#[weight = 0] // TODO: update me (https://github.com/paritytech/parity-bridges-common/issues/78)
		pub fn receive_messages_delivery_proof(origin, proof: MessagesDeliveryProofOf<T, I>) -> DispatchResult {
			let confirmation_relayer = ensure_signed(origin)?;
			let (lane_id, lane_data) = T::TargetHeaderChain::verify_messages_delivery_proof(proof).map_err(|err| {
				frame_support::debug::trace!(
					target: "runtime",
					"Rejecting invalid messages delivery proof: {:?}",
					err,
				);

				Error::<T, I>::InvalidMessagesDeliveryProof
			})?;

			// mark messages as delivered
			let mut lane = outbound_lane::<T, I>(lane_id);
			let received_range = lane.confirm_delivery(lane_data.latest_received_nonce);
			if let Some(received_range) = received_range {
				Self::deposit_event(RawEvent::MessagesDelivered(lane_id, received_range.0, received_range.1));

				// reward relayers that have delivered messages
				// this loop is bounded by `T::MaxUnconfirmedMessagesAtInboundLane` on the bridged chain
				for (nonce_low, nonce_high, relayer) in lane_data.relayers {
					let nonce_begin = sp_std::cmp::max(nonce_low, received_range.0);
					let nonce_end = sp_std::cmp::min(nonce_high, received_range.1);
					// loop won't proceed if current entry is ahead of received range (begin > end).
					for nonce in nonce_begin..nonce_end + 1 {
						let message_data = OutboundMessages::<T, I>::get(MessageKey {
							lane_id,
							nonce,
						}).expect("message was just confirmed; we never prune unconfirmed messages; qed");

						<T as Trait<I>>::MessageDeliveryAndDispatchPayment::pay_relayer_reward(
							&confirmation_relayer,
							&relayer,
							&message_data.fee,
						);
					}
				}
			}

			frame_support::debug::trace!(
				target: "runtime",
				"Received messages delivery proof up to (and including) {} at lane {:?}",
				lane_data.latest_received_nonce,
				lane_id,
			);

			Ok(())
		}
	}
}

/// Creates new inbound lane object, backed by runtime storage.
fn inbound_lane<T: Trait<I>, I: Instance>(lane_id: LaneId) -> InboundLane<RuntimeInboundLaneStorage<T, I>> {
	InboundLane::new(RuntimeInboundLaneStorage {
		lane_id,
		cached_data: RefCell::new(None),
		_phantom: Default::default(),
	})
}

/// Creates new outbound lane object, backed by runtime storage.
fn outbound_lane<T: Trait<I>, I: Instance>(lane_id: LaneId) -> OutboundLane<RuntimeOutboundLaneStorage<T, I>> {
	OutboundLane::new(RuntimeOutboundLaneStorage {
		lane_id,
		_phantom: Default::default(),
	})
}

/// Runtime inbound lane storage.
struct RuntimeInboundLaneStorage<T: Trait<I>, I = DefaultInstance> {
	lane_id: LaneId,
	cached_data: RefCell<Option<InboundLaneData<T::InboundRelayer>>>,
	_phantom: PhantomData<I>,
}

impl<T: Trait<I>, I: Instance> InboundLaneStorage for RuntimeInboundLaneStorage<T, I> {
	type MessageFee = T::InboundMessageFee;
	type Relayer = T::InboundRelayer;

	fn id(&self) -> LaneId {
		self.lane_id
	}

	fn max_unconfirmed_messages(&self) -> MessageNonce {
		T::MaxUnconfirmedMessagesAtInboundLane::get()
	}

	fn data(&self) -> InboundLaneData<T::InboundRelayer> {
		match self.cached_data.clone().into_inner() {
			Some(data) => data,
			None => {
				let data = InboundLanes::<T, I>::get(&self.lane_id);
				*self.cached_data.try_borrow_mut().expect(
					"we're in the single-threaded environment;\
						we have no recursive borrows; qed",
				) = Some(data.clone());
				data
			}
		}
	}

	fn set_data(&mut self, data: InboundLaneData<T::InboundRelayer>) {
		*self.cached_data.try_borrow_mut().expect(
			"we're in the single-threaded environment;\
				we have no recursive borrows; qed",
		) = Some(data.clone());
		InboundLanes::<T, I>::insert(&self.lane_id, data)
	}
}

/// Runtime outbound lane storage.
struct RuntimeOutboundLaneStorage<T, I = DefaultInstance> {
	lane_id: LaneId,
	_phantom: PhantomData<(T, I)>,
}

impl<T: Trait<I>, I: Instance> OutboundLaneStorage for RuntimeOutboundLaneStorage<T, I> {
	type MessageFee = T::OutboundMessageFee;

	fn id(&self) -> LaneId {
		self.lane_id
	}

	fn data(&self) -> OutboundLaneData {
		OutboundLanes::<I>::get(&self.lane_id)
	}

	fn set_data(&mut self, data: OutboundLaneData) {
		OutboundLanes::<I>::insert(&self.lane_id, data)
	}

	#[cfg(test)]
	fn message(&self, nonce: &MessageNonce) -> Option<MessageData<T::OutboundMessageFee>> {
		OutboundMessages::<T, I>::get(MessageKey {
			lane_id: self.lane_id,
			nonce: *nonce,
		})
	}

	fn save_message(&mut self, nonce: MessageNonce, mesage_data: MessageData<T::OutboundMessageFee>) {
		OutboundMessages::<T, I>::insert(
			MessageKey {
				lane_id: self.lane_id,
				nonce,
			},
			mesage_data,
		);
	}

	fn remove_message(&mut self, nonce: &MessageNonce) {
		OutboundMessages::<T, I>::remove(MessageKey {
			lane_id: self.lane_id,
			nonce: *nonce,
		});
	}
}

/// Verify messages proof and return proved messages with decoded payload.
fn verify_and_decode_messages_proof<Chain: SourceHeaderChain<Fee>, Fee, DispatchPayload: Decode>(
	proof: Chain::MessagesProof,
) -> Result<ProvedMessages<DispatchMessage<DispatchPayload, Fee>>, Chain::Error> {
	Chain::verify_messages_proof(proof).map(|messages_by_lane| {
		messages_by_lane
			.into_iter()
			.map(|(lane, lane_data)| {
				(
					lane,
					ProvedLaneMessages {
						lane_state: lane_data.lane_state,
						messages: lane_data.messages.into_iter().map(Into::into).collect(),
					},
				)
			})
			.collect()
	})
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::mock::{
		message, run_test, Origin, TestEvent, TestMessageDeliveryAndDispatchPayment, TestMessagesProof, TestRuntime,
		PAYLOAD_REJECTED_BY_TARGET_CHAIN, REGULAR_PAYLOAD, TEST_LANE_ID, TEST_RELAYER_A, TEST_RELAYER_B,
	};
	use frame_support::{assert_noop, assert_ok};
	use frame_system::{EventRecord, Module as System, Phase};

	fn send_regular_message() {
		System::<TestRuntime>::set_block_number(1);
		System::<TestRuntime>::reset_events();

		assert_ok!(Module::<TestRuntime>::send_message(
			Origin::signed(1),
			TEST_LANE_ID,
			REGULAR_PAYLOAD,
			REGULAR_PAYLOAD.1,
		));

		// check event with assigned nonce
		assert_eq!(
			System::<TestRuntime>::events(),
			vec![EventRecord {
				phase: Phase::Initialization,
				event: TestEvent::message_lane(RawEvent::MessageAccepted(TEST_LANE_ID, 1)),
				topics: vec![],
			}],
		);

		// check that fee has been withdrawn from submitter
		assert!(TestMessageDeliveryAndDispatchPayment::is_fee_paid(1, REGULAR_PAYLOAD.1));
	}

	fn receive_messages_delivery_proof() {
		System::<TestRuntime>::set_block_number(1);
		System::<TestRuntime>::reset_events();

		assert_ok!(Module::<TestRuntime>::receive_messages_delivery_proof(
			Origin::signed(1),
			Ok((
				TEST_LANE_ID,
				InboundLaneData {
					latest_received_nonce: 1,
					..Default::default()
				}
			)),
		));

		assert_eq!(
			System::<TestRuntime>::events(),
			vec![EventRecord {
				phase: Phase::Initialization,
				event: TestEvent::message_lane(RawEvent::MessagesDelivered(TEST_LANE_ID, 1, 1)),
				topics: vec![],
			}],
		);
	}

	#[test]
	fn send_message_works() {
		run_test(|| {
			send_regular_message();
		});
	}

	#[test]
	fn chain_verifier_rejects_invalid_message_in_send_message() {
		run_test(|| {
			// messages with this payload are rejected by target chain verifier
			assert_noop!(
				Module::<TestRuntime>::send_message(
					Origin::signed(1),
					TEST_LANE_ID,
					PAYLOAD_REJECTED_BY_TARGET_CHAIN,
					PAYLOAD_REJECTED_BY_TARGET_CHAIN.1
				),
				Error::<TestRuntime, DefaultInstance>::MessageRejectedByChainVerifier,
			);
		});
	}

	#[test]
	fn lane_verifier_rejects_invalid_message_in_send_message() {
		run_test(|| {
			// messages with zero fee are rejected by lane verifier
			assert_noop!(
				Module::<TestRuntime>::send_message(Origin::signed(1), TEST_LANE_ID, REGULAR_PAYLOAD, 0),
				Error::<TestRuntime, DefaultInstance>::MessageRejectedByLaneVerifier,
			);
		});
	}

	#[test]
	fn message_send_fails_if_submitter_cant_pay_message_fee() {
		run_test(|| {
			TestMessageDeliveryAndDispatchPayment::reject_payments();
			assert_noop!(
				Module::<TestRuntime>::send_message(
					Origin::signed(1),
					TEST_LANE_ID,
					REGULAR_PAYLOAD,
					REGULAR_PAYLOAD.1
				),
				Error::<TestRuntime, DefaultInstance>::FailedToWithdrawMessageFee,
			);
		});
	}

	#[test]
	fn receive_messages_proof_works() {
		run_test(|| {
			assert_ok!(Module::<TestRuntime>::receive_messages_proof(
				Origin::signed(1),
				TEST_RELAYER_A,
				Ok(vec![message(1, REGULAR_PAYLOAD)]).into(),
				REGULAR_PAYLOAD.1,
			));

			assert_eq!(InboundLanes::<TestRuntime>::get(TEST_LANE_ID).latest_received_nonce, 1);
		});
	}

	#[test]
	fn receive_messages_proof_updates_confirmed_message_nonce() {
		run_test(|| {
			// say we have received 10 messages && last confirmed message is 8
			InboundLanes::<TestRuntime, DefaultInstance>::insert(
				TEST_LANE_ID,
				InboundLaneData {
					latest_confirmed_nonce: 8,
					latest_received_nonce: 10,
					relayers: vec![(9, 9, TEST_RELAYER_A), (10, 10, TEST_RELAYER_B)]
						.into_iter()
						.collect(),
				},
			);

			// message proof includes outbound lane state with latest confirmed message updated to 9
			let mut message_proof: TestMessagesProof = Ok(vec![message(11, REGULAR_PAYLOAD)]).into();
			message_proof.result.as_mut().unwrap()[0].1.lane_state = Some(OutboundLaneData {
				latest_received_nonce: 9,
				..Default::default()
			});

			assert_ok!(Module::<TestRuntime>::receive_messages_proof(
				Origin::signed(1),
				TEST_RELAYER_A,
				message_proof,
				REGULAR_PAYLOAD.1,
			));

			assert_eq!(
				InboundLanes::<TestRuntime>::get(TEST_LANE_ID),
				InboundLaneData {
					relayers: vec![(10, 10, TEST_RELAYER_B), (11, 11, TEST_RELAYER_A)]
						.into_iter()
						.collect(),
					latest_received_nonce: 11,
					latest_confirmed_nonce: 9,
				},
			);
		});
	}

	#[test]
	fn receive_messages_proof_rejects_invalid_dispatch_weight() {
		run_test(|| {
			assert_noop!(
				Module::<TestRuntime>::receive_messages_proof(
					Origin::signed(1),
					TEST_RELAYER_A,
					Ok(vec![message(1, REGULAR_PAYLOAD)]).into(),
					REGULAR_PAYLOAD.1 - 1,
				),
				Error::<TestRuntime, DefaultInstance>::InvalidMessagesDispatchWeight,
			);
		});
	}

	#[test]
	fn receive_messages_proof_rejects_invalid_proof() {
		run_test(|| {
			assert_noop!(
				Module::<TestRuntime, DefaultInstance>::receive_messages_proof(
					Origin::signed(1),
					TEST_RELAYER_A,
					Err(()).into(),
					0,
				),
				Error::<TestRuntime, DefaultInstance>::InvalidMessagesProof,
			);
		});
	}

	#[test]
	fn receive_messages_delivery_proof_works() {
		run_test(|| {
			send_regular_message();
			receive_messages_delivery_proof();

			assert_eq!(
				OutboundLanes::<DefaultInstance>::get(&TEST_LANE_ID).latest_received_nonce,
				1,
			);
		});
	}

	#[test]
	fn receive_messages_delivery_proof_rewards_relayers() {
		run_test(|| {
			assert_ok!(Module::<TestRuntime>::send_message(
				Origin::signed(1),
				TEST_LANE_ID,
				REGULAR_PAYLOAD,
				1000,
			));
			assert_ok!(Module::<TestRuntime>::send_message(
				Origin::signed(1),
				TEST_LANE_ID,
				REGULAR_PAYLOAD,
				2000,
			));

			// this reports delivery of message 1 => reward is paid to TEST_RELAYER_A
			assert_ok!(Module::<TestRuntime>::receive_messages_delivery_proof(
				Origin::signed(1),
				Ok((
					TEST_LANE_ID,
					InboundLaneData {
						relayers: vec![(1, 1, TEST_RELAYER_A)].into_iter().collect(),
						latest_received_nonce: 1,
						..Default::default()
					}
				)),
			));
			assert!(TestMessageDeliveryAndDispatchPayment::is_reward_paid(
				TEST_RELAYER_A,
				1000
			));
			assert!(!TestMessageDeliveryAndDispatchPayment::is_reward_paid(
				TEST_RELAYER_B,
				2000
			));

			// this reports delivery of both message 1 and message 2 => reward is paid only to TEST_RELAYER_B
			assert_ok!(Module::<TestRuntime>::receive_messages_delivery_proof(
				Origin::signed(1),
				Ok((
					TEST_LANE_ID,
					InboundLaneData {
						relayers: vec![(1, 1, TEST_RELAYER_A), (2, 2, TEST_RELAYER_B)]
							.into_iter()
							.collect(),
						latest_received_nonce: 2,
						..Default::default()
					}
				)),
			));
			assert!(!TestMessageDeliveryAndDispatchPayment::is_reward_paid(
				TEST_RELAYER_A,
				1000
			));
			assert!(TestMessageDeliveryAndDispatchPayment::is_reward_paid(
				TEST_RELAYER_B,
				2000
			));
		});
	}

	#[test]
	fn receive_messages_delivery_proof_rejects_invalid_proof() {
		run_test(|| {
			assert_noop!(
				Module::<TestRuntime>::receive_messages_delivery_proof(Origin::signed(1), Err(()),),
				Error::<TestRuntime, DefaultInstance>::InvalidMessagesDeliveryProof,
			);
		});
	}

	#[test]
	fn receive_messages_accepts_single_message_with_invalid_payload() {
		run_test(|| {
			let mut invalid_message = message(1, REGULAR_PAYLOAD);
			invalid_message.data.payload = Vec::new();

			assert_ok!(Module::<TestRuntime, DefaultInstance>::receive_messages_proof(
				Origin::signed(1),
				TEST_RELAYER_A,
				Ok(vec![invalid_message]).into(),
				0, // weight may be zero in this case (all messages are improperly encoded)
			),);

			assert_eq!(InboundLanes::<TestRuntime>::get(&TEST_LANE_ID).latest_received_nonce, 1,);
		});
	}

	#[test]
	fn receive_messages_accepts_batch_with_message_with_invalid_payload() {
		run_test(|| {
			let mut invalid_message = message(2, REGULAR_PAYLOAD);
			invalid_message.data.payload = Vec::new();

			assert_ok!(Module::<TestRuntime, DefaultInstance>::receive_messages_proof(
				Origin::signed(1),
				TEST_RELAYER_A,
				Ok(vec![
					message(1, REGULAR_PAYLOAD),
					invalid_message,
					message(3, REGULAR_PAYLOAD),
				])
				.into(),
				REGULAR_PAYLOAD.1 + REGULAR_PAYLOAD.1,
			),);

			assert_eq!(InboundLanes::<TestRuntime>::get(&TEST_LANE_ID).latest_received_nonce, 3,);
		});
	}
}