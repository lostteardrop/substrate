// This file is part of Substrate.

// Copyright (C) 2019-2021 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! This module contains functions to meter the storage usage.

use crate::{storage::ContractInfo, BalanceOf, Config, Error};
use frame_support::{
	dispatch::{DispatchError, DispatchResult},
	traits::{tokens::BalanceStatus, Get, ReservableCurrency},
	DefaultNoBound,
};
use pallet_contracts_primitives::StorageDeposit as Deposit;
use sp_core::crypto::UncheckedFrom;
use sp_runtime::traits::{Saturating, Zero};
use sp_std::marker::PhantomData;

pub type Meter<T> = RawMeter<T, DefaultExt, Root>;
pub type NestedMeter<T> = RawMeter<T, DefaultExt, Nested>;
pub type GenericMeter<T, S> = RawMeter<T, DefaultExt, S>;

pub trait Ext<T: Config> {
	fn reserve_limit(origin: &T::AccountId, limit: &BalanceOf<T>) -> DispatchResult;
	fn unreserve_limit(origin: &T::AccountId, limit: &BalanceOf<T>, usage: &Usage<T>);
	fn charge(origin: &T::AccountId, contract: &T::AccountId, amount: &Deposit<BalanceOf<T>>);
}

pub enum DefaultExt {}

pub trait State: private::Sealed {}

pub enum Root {}
pub enum Nested {}

impl State for Root {}
impl State for Nested {}

#[derive(DefaultNoBound)]
pub struct RawMeter<T: Config, E: Ext<T>, S: State> {
	origin: Option<T::AccountId>,
	limit: BalanceOf<T>,
	total_usage: Usage<T>,
	own_usage: Usage<T>,
	_phantom: PhantomData<(E, S)>,
}

#[derive(Default)]
pub struct Diff {
	pub bytes_added: u32,
	pub bytes_removed: u32,
	pub items_added: u32,
	pub items_removed: u32,
}

#[derive(DefaultNoBound, Clone)]
pub struct Usage<T: Config> {
	charge: BalanceOf<T>,
	refund: BalanceOf<T>,
}

impl<T: Config> Usage<T> {
	pub fn charge(amount: BalanceOf<T>) -> Self {
		Self { charge: amount, refund: Zero::zero() }
	}
}

impl Diff {
	pub fn to_usage<T: Config>(&self) -> Usage<T> {
		let mut usage: Usage<T> = Usage::default();
		let per_byte = T::DepositPerByte::get();
		let per_item = T::DepositPerItem::get();

		if self.bytes_added > self.bytes_removed {
			usage.charge = per_byte.saturating_mul((self.bytes_added - self.bytes_removed).into());
		} else if self.bytes_removed > self.bytes_added {
			usage.refund = per_byte.saturating_mul((self.bytes_removed - self.bytes_added).into());
		}

		if self.items_added > self.items_removed {
			usage.charge.saturating_add(
				per_item.saturating_mul((self.items_added - self.items_removed).into()),
			);
		} else if self.bytes_removed > self.bytes_added {
			usage.refund.saturating_add(
				per_item.saturating_mul((self.items_removed - self.items_added).into()),
			);
		}

		usage
	}
}

impl<T: Config> Copy for Usage<T> {}

impl<T: Config> Usage<T> {
	pub fn to_deposit(&self) -> Deposit<BalanceOf<T>> {
		if self.charge >= self.refund {
			Deposit::Charge(self.charge.saturating_sub(self.refund))
		} else {
			Deposit::Refund(self.refund.saturating_sub(self.charge))
		}
	}
}

impl<T: Config> Saturating for Usage<T> {
	fn saturating_add(self, rhs: Self) -> Self {
		Self {
			charge: self.charge.saturating_add(rhs.charge),
			refund: self.refund.saturating_add(rhs.refund),
		}
	}

	fn saturating_sub(self, rhs: Self) -> Self {
		Self {
			charge: self.charge.saturating_sub(rhs.charge),
			refund: self.refund.saturating_sub(rhs.refund),
		}
	}

	fn saturating_mul(self, rhs: Self) -> Self {
		Self {
			charge: self.charge.saturating_mul(rhs.charge),
			refund: self.refund.saturating_mul(rhs.refund),
		}
	}

	fn saturating_pow(self, exp: usize) -> Self {
		Self { charge: self.charge.saturating_pow(exp), refund: self.refund.saturating_pow(exp) }
	}
}

impl<T, E, S> Drop for RawMeter<T, E, S>
where
	T: Config,
	E: Ext<T>,
	S: State,
{
	fn drop(&mut self) {
		// Drop cannot be specialized: We need to do a runtime check.
		// An origin exists if and only if this is a root meter.
		if let Some(origin) = self.origin.as_ref() {
			// you cannot charge to the root meter
			debug_assert_eq!(self.own_usage.charge, <BalanceOf<T>>::zero());
			debug_assert_eq!(self.own_usage.refund, <BalanceOf<T>>::zero());
			E::unreserve_limit(origin, &self.limit, &self.total_usage);
		}
	}
}

impl<T, E, S> RawMeter<T, E, S>
where
	T: Config,
	T::AccountId: UncheckedFrom<T::Hash> + AsRef<[u8]>,
	E: Ext<T>,
	S: State,
{
	pub fn nested(&mut self) -> RawMeter<T, E, Nested> {
		RawMeter {
			origin: None,
			limit: self.available(),
			total_usage: Default::default(),
			own_usage: Default::default(),
			_phantom: PhantomData,
		}
	}

	pub fn absorb(
		&mut self,
		absorbed: RawMeter<T, E, Nested>,
		origin: &T::AccountId,
		contract: &T::AccountId,
		info: Option<&mut ContractInfo<T>>,
	) {
		let mut deposit = absorbed.own_usage.to_deposit();

		// Absorbing from an exisiting (non terminated) contract.
		if let Some(info) = info {
			match &mut deposit {
				Deposit::Charge(amount) =>
					info.storage_deposit = info.storage_deposit.saturating_add(*amount),
				Deposit::Refund(amount) => {
					// We need to make sure to never refund more than what was deposited.
					// This is relevant on runtime upgrades.
					*amount = (*amount).min(info.storage_deposit);
					info.storage_deposit = info.storage_deposit.saturating_sub(*amount);
				},
			}
		}

		self.total_usage = self.total_usage.saturating_add(absorbed.total_usage);
		E::charge(origin, &contract, &deposit);
	}

	pub fn total_usage(&self) -> Usage<T> {
		self.total_usage
	}

	fn available(&self) -> BalanceOf<T> {
		self.limit
			.saturating_add(self.total_usage.refund)
			.saturating_sub(self.total_usage.charge)
	}
}

impl<T, E> RawMeter<T, E, Root>
where
	T: Config,
	T::AccountId: UncheckedFrom<T::Hash> + AsRef<[u8]>,
	E: Ext<T>,
{
	pub fn new(origin: T::AccountId, limit: BalanceOf<T>) -> Result<Self, DispatchError> {
		E::reserve_limit(&origin, &limit)?;
		Ok(Self {
			origin: Some(origin),
			limit,
			total_usage: Default::default(),
			own_usage: Default::default(),
			_phantom: PhantomData,
		})
	}
}

impl<T, E> RawMeter<T, E, Nested>
where
	T: Config,
	T::AccountId: UncheckedFrom<T::Hash> + AsRef<[u8]>,
	E: Ext<T>,
{
	pub fn charge(&mut self, diff: &Diff) -> Result<Deposit<BalanceOf<T>>, DispatchError> {
		let usage = diff.to_usage();
		self.total_usage = self.total_usage.saturating_add(usage);
		self.own_usage = self.own_usage.saturating_add(usage);
		if let Deposit::Charge(amount) = self.total_usage.to_deposit() {
			if amount > self.limit {
				return Err(<Error<T>>::StorageExhausted.into())
			}
		}
		Ok(usage.to_deposit())
	}

	pub fn terminate(&mut self, contract_info: &ContractInfo<T>) {
		let usage = Usage { charge: 0u32.into(), refund: contract_info.storage_deposit };

		// The usage for `own_usage` isn't persisted into the contract info until the current
		// frame is dropped. This means that whatever changes were introduced during the
		// current frame are dicarded when terminating.
		self.total_usage = self.total_usage.saturating_add(usage).saturating_sub(self.own_usage);
		self.own_usage = usage;
	}
}

impl<T: Config> Ext<T> for DefaultExt {
	fn reserve_limit(origin: &T::AccountId, limit: &BalanceOf<T>) -> DispatchResult {
		T::Currency::reserve(origin, *limit)
	}

	fn unreserve_limit(origin: &T::AccountId, limit: &BalanceOf<T>, usage: &Usage<T>) {
		T::Currency::unreserve(
			origin,
			limit.saturating_add(usage.refund).saturating_sub(usage.charge),
		);
	}

	fn charge(origin: &T::AccountId, contract: &T::AccountId, amount: &Deposit<BalanceOf<T>>) {
		let (slashed, beneficiary, amount) = match amount {
			Deposit::Charge(amount) => (origin, contract, amount),
			Deposit::Refund(amount) => (contract, origin, amount),
		};

		// For charge `Err` can never happen as a contract's account is required to exist
		// at all times. The pallet enforces this invariant. Chain extensions or dispatchables
		// that allow the removal of the contract's account are defunct.
		//
		// For refund `Err` can't happen because the initial value transfer from the
		// origin to the contract has a keep alive existence requirement.
		//
		// There is nothing we can do when either `Err` or `Ok(> 0)` happens as this constitutes
		// a bug in the runtime: Either the runtime does not hold up the invariant of never
		// deleting a contract's account or it does not honor reserved balances.
		//
		// There is one exception:
		//
		// If a contract is terminated its account's free balance is completely removed and
		// sent to the beneficiary. This could lead to the removal of the contract's account if
		// the amount of reserved balance is below the existential deposit.
		let _ = T::Currency::repatriate_reserved(
			slashed,
			beneficiary,
			*amount,
			BalanceStatus::Reserved,
		);
	}
}

mod private {
	pub trait Sealed {}
	impl Sealed for super::Root {}
	impl Sealed for super::Nested {}
}
