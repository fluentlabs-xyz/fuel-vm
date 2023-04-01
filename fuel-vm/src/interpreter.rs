//! [`Interpreter`] implementation

use crate::call::CallFrame;
use crate::constraints::reg_key::*;
use crate::consts::*;
use crate::context::Context;
use crate::gas::GasCosts;
use crate::state::Debugger;
use std::io::Read;
use std::ops::Index;
use std::{io, mem};

use fuel_asm::{Flags, PanicReason};
use fuel_tx::{
    field, Chargeable, CheckError, ConsensusParameters, Create, Executable, Output, Receipt, Script, Transaction,
    TransactionFee, TransactionRepr, UniqueIdentifier,
};
use fuel_types::bytes::{SerializableVec, SizedBytes};
use fuel_types::{AssetId, ContractId, Word};

mod alu;
mod balances;
mod blockchain;
mod constructors;
mod contract;
mod crypto;
pub mod diff;
mod executors;
mod flow;
mod gas;
mod initialization;
mod internal;
mod log;
mod memory;
mod metadata;
mod post_execution;
mod receipts;

#[cfg(feature = "debug")]
mod debug;

use crate::profiler::Profiler;

#[cfg(feature = "profile-gas")]
use crate::profiler::InstructionLocation;

pub use balances::RuntimeBalances;
pub use memory::MemoryRange;

use crate::checked_transaction::{
    CreateCheckedMetadata, IntoChecked, NonRetryableFreeBalances, RetryableAmount, ScriptCheckedMetadata,
};
use crate::estimated_transaction::IntoEstimated;

use self::memory::Memory;
use self::receipts::ReceiptsCtx;

/// VM interpreter.
///
/// The internal state of the VM isn't expose because the intended usage is to
/// either inspect the resulting receipts after a transaction execution, or the
/// resulting mutated transaction.
///
/// These can be obtained with the help of a [`crate::transactor::Transactor`]
/// or a client implementation.
#[derive(Debug, Clone)]
pub struct Interpreter<S, Tx = ()> {
    registers: [Word; VM_REGISTER_COUNT],
    memory: Memory<MEM_SIZE>,
    frames: Vec<CallFrame>,
    receipts: ReceiptsCtx,
    tx: Tx,
    initial_balances: InitialBalances,
    storage: S,
    debugger: Debugger,
    context: Context,
    balances: RuntimeBalances,
    gas_costs: GasCosts,
    profiler: Profiler,
    params: ConsensusParameters,
    /// `PanicContext` after the latest execution. It is consumed by `append_panic_receipt`
    /// and is `PanicContext::None` after consumption.
    panic_context: PanicContext,
}

/// Sometimes it is possible to add some additional context information
/// regarding panic reasons to simplify debugging.
// TODO: Move this enum into `fuel-tx` and use it inside of the `Receipt::Panic` as meta
//  information. Maybe better to have `Vec<PanicContext>` to provide more information.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PanicContext {
    /// No additional information.
    None,
    /// `ContractId` retrieved during instruction execution.
    ContractId(ContractId),
}

impl<S, Tx> Interpreter<S, Tx> {
    /// Returns the current state of the VM memory
    pub fn memory(&self) -> &[u8] {
        self.memory.as_slice()
    }

    /// Returns the current state of the registers
    pub const fn registers(&self) -> &[Word] {
        &self.registers
    }

    pub(crate) fn call_stack(&self) -> &[CallFrame] {
        self.frames.as_slice()
    }

    /// Debug handler
    pub const fn debugger(&self) -> &Debugger {
        &self.debugger
    }

    /// The current transaction.
    pub fn transaction(&self) -> &Tx {
        &self.tx
    }

    /// The initial balances.
    pub fn initial_balances(&self) -> &InitialBalances {
        &self.initial_balances
    }

    /// Consensus parameters
    pub const fn params(&self) -> &ConsensusParameters {
        &self.params
    }

    /// Gas costs for opcodes
    pub fn gas_costs(&self) -> &GasCosts {
        &self.gas_costs
    }

    /// Receipts generated by a transaction execution.
    pub fn receipts(&self) -> &[Receipt] {
        self.receipts.as_ref().as_slice()
    }

    pub(crate) fn contract_id(&self) -> Option<ContractId> {
        self.frames.last().map(|frame| *frame.to())
    }

    /// Reference to the underlying profiler
    #[cfg(feature = "profile-any")]
    pub const fn profiler(&self) -> &Profiler {
        &self.profiler
    }
}

pub(crate) fn flags(flag: Reg<FLAG>) -> Flags {
    Flags::from_bits_truncate(*flag)
}

pub(crate) fn is_wrapping(flag: Reg<FLAG>) -> bool {
    flags(flag).contains(Flags::WRAPPING)
}

pub(crate) fn is_unsafe_math(flag: Reg<FLAG>) -> bool {
    flags(flag).contains(Flags::UNSAFEMATH)
}

#[cfg(feature = "profile-gas")]
fn current_location(
    current_contract: Option<ContractId>,
    pc: crate::constraints::reg_key::Reg<{ crate::constraints::reg_key::PC }>,
    is: crate::constraints::reg_key::Reg<{ crate::constraints::reg_key::IS }>,
) -> InstructionLocation {
    InstructionLocation::new(current_contract, *pc - *is)
}

impl<S, Tx> AsRef<S> for Interpreter<S, Tx> {
    fn as_ref(&self) -> &S {
        &self.storage
    }
}

impl<S, Tx> AsMut<S> for Interpreter<S, Tx> {
    fn as_mut(&mut self) -> &mut S {
        &mut self.storage
    }
}

/// The definition of the executable transaction supported by the `Interpreter`.
pub trait ExecutableTransaction:
    Default
    + Clone
    + Chargeable
    + Executable
    + IntoChecked
    + IntoEstimated
    + UniqueIdentifier
    + field::Maturity
    + field::Inputs
    + field::Outputs
    + field::Witnesses
    + Into<Transaction>
    + SizedBytes
    + SerializableVec
{
    /// Casts the `Self` transaction into `&Script` if any.
    fn as_script(&self) -> Option<&Script>;

    /// Casts the `Self` transaction into `&mut Script` if any.
    fn as_script_mut(&mut self) -> Option<&mut Script>;

    /// Casts the `Self` transaction into `&Create` if any.
    fn as_create(&self) -> Option<&Create>;

    /// Casts the `Self` transaction into `&mut Create` if any.
    fn as_create_mut(&mut self) -> Option<&mut Create>;

    /// Returns the type of the transaction like `Transaction::Create` or `Transaction::Script`.
    fn transaction_type() -> Word;

    /// Dumps the `Output` by the `idx` into the `buf` buffer.
    fn output_to_mem(&mut self, idx: usize, buf: &mut [u8]) -> io::Result<usize> {
        self.outputs_mut()
            .get_mut(idx)
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "Invalid output idx"))
            .and_then(|o| o.read(buf))
    }

    /// Replaces the `Output::Variable` with the `output`(should be also `Output::Variable`)
    /// by the `idx` index.
    fn replace_variable_output(&mut self, idx: usize, output: Output) -> Result<(), PanicReason> {
        if !output.is_variable() {
            return Err(PanicReason::ExpectedOutputVariable);
        }

        // TODO increase the error granularity for this case - create a new variant of panic reason
        self.outputs_mut()
            .get_mut(idx)
            .and_then(|o| match o {
                Output::Variable { amount, .. } if amount == &0 => Some(o),
                _ => None,
            })
            .map(|o| mem::replace(o, output))
            .map(|_| ())
            .ok_or(PanicReason::OutputNotFound)
    }

    /// Update change and variable outputs.
    ///
    /// `revert` will signal if the execution was reverted. It will refund the unused gas cost to
    /// the base asset and reset output changes to their `initial_balances`.
    ///
    /// `remaining_gas` expects the raw content of `$ggas`
    ///
    /// `initial_balances` contains the initial state of the free balances
    ///
    /// `balances` will contain the current state of the free balances
    fn update_outputs<I>(
        &mut self,
        params: &ConsensusParameters,
        revert: bool,
        remaining_gas: Word,
        initial_balances: &InitialBalances,
        balances: &I,
    ) -> Result<(), CheckError>
    where
        I: for<'a> Index<&'a AssetId, Output = Word>,
    {
        let gas_refund = TransactionFee::gas_refund_value(params, remaining_gas, self.price())
            .ok_or(CheckError::ArithmeticOverflow)?;

        self.outputs_mut().iter_mut().try_for_each(|o| match o {
            // If revert, set base asset to initial balance and refund unused gas
            //
            // Note: the initial balance deducts the gas limit from base asset
            Output::Change { asset_id, amount, .. } if revert && asset_id == &AssetId::BASE => initial_balances
                .non_retryable[&AssetId::BASE]
                .checked_add(gas_refund)
                .map(|v| *amount = v)
                .ok_or(CheckError::ArithmeticOverflow),

            // If revert, reset any non-base asset to its initial balance
            Output::Change { asset_id, amount, .. } if revert => {
                *amount = initial_balances.non_retryable[asset_id];
                Ok(())
            }

            // The change for the base asset will be the available balance + unused gas
            Output::Change { asset_id, amount, .. } if asset_id == &AssetId::BASE => balances[asset_id]
                .checked_add(gas_refund)
                .map(|v| *amount = v)
                .ok_or(CheckError::ArithmeticOverflow),

            // Set changes to the remainder provided balances
            Output::Change { asset_id, amount, .. } => {
                *amount = balances[asset_id];
                Ok(())
            }

            // If revert, zeroes all variable output values
            Output::Variable { amount, .. } if revert => {
                *amount = 0;
                Ok(())
            }

            // Other outputs are unaffected
            _ => Ok(()),
        })
    }

    /// Finds `Output::Contract` corresponding to the `input` index.
    fn find_output_contract(&self, input: usize) -> Option<(usize, &Output)> {
        self.outputs().iter().enumerate().find(|(_idx, o)| {
            matches!(o, Output::Contract {
                input_index, ..
            } if *input_index as usize == input)
        })
    }
}

impl ExecutableTransaction for Create {
    fn as_script(&self) -> Option<&Script> {
        None
    }

    fn as_script_mut(&mut self) -> Option<&mut Script> {
        None
    }

    fn as_create(&self) -> Option<&Create> {
        Some(self)
    }

    fn as_create_mut(&mut self) -> Option<&mut Create> {
        Some(self)
    }

    fn transaction_type() -> Word {
        TransactionRepr::Create as Word
    }
}

impl ExecutableTransaction for Script {
    fn as_script(&self) -> Option<&Script> {
        Some(self)
    }

    fn as_script_mut(&mut self) -> Option<&mut Script> {
        Some(self)
    }

    fn as_create(&self) -> Option<&Create> {
        None
    }

    fn as_create_mut(&mut self) -> Option<&mut Create> {
        None
    }

    fn transaction_type() -> Word {
        TransactionRepr::Script as Word
    }
}

/// The initial balances of the transaction.
#[derive(Default, Debug, Clone, Eq, PartialEq, Hash)]
pub struct InitialBalances {
    /// See [`NonRetryableFreeBalances`].
    pub non_retryable: NonRetryableFreeBalances,
    /// See [`RetryableAmount`].
    pub retryable: Option<RetryableAmount>,
}

/// Methods that should be implemented by the checked metadata of supported transactions.
pub trait CheckedMetadata {
    /// Returns the initial balances from the checked metadata of the transaction.
    fn balances(self) -> InitialBalances;

    /// Get gas used by predicates. Returns zero if the predicates haven't been checked.
    fn gas_used_by_predicates(&self) -> Word;

    /// Set gas used by predicates after checking them.
    fn set_gas_used_by_predicates(&mut self, gas_used: Word);
}

impl CheckedMetadata for ScriptCheckedMetadata {
    fn balances(self) -> InitialBalances {
        InitialBalances {
            non_retryable: self.non_retryable_balances,
            retryable: Some(self.retryable_balance),
        }
    }

    fn gas_used_by_predicates(&self) -> Word {
        self.gas_used_by_predicates
    }

    fn set_gas_used_by_predicates(&mut self, gas_used: Word) {
        self.gas_used_by_predicates = gas_used;
    }
}

impl CheckedMetadata for CreateCheckedMetadata {
    fn balances(self) -> InitialBalances {
        InitialBalances {
            non_retryable: self.free_balances,
            retryable: None,
        }
    }

    fn gas_used_by_predicates(&self) -> Word {
        self.gas_used_by_predicates
    }

    fn set_gas_used_by_predicates(&mut self, gas_used: Word) {
        self.gas_used_by_predicates = gas_used;
    }
}
