//! [`Interpreter`] implementation

use crate::call::CallFrame;
use crate::consts::*;
use crate::context::Context;
use crate::state::Debugger;
use fuel_asm::PanicReason;
use std::collections::BTreeMap;
use std::io::Read;
use std::ops::Index;
use std::{io, mem};

use fuel_tx::{
    field, Chargeable, CheckError, ConsensusParameters, Create, CreateCheckedMetadata, Executable, IntoChecked, Output,
    Receipt, Script, ScriptCheckedMetadata, Transaction, TransactionFee, UniqueIdentifier,
};
use fuel_types::{Address, AssetId, Word};

mod alu;
mod balances;
mod blockchain;
mod constructors;
mod contract;
mod crypto;
mod executors;
mod flow;
mod frame;
mod gas;
mod initialization;
mod internal;
mod log;
mod memory;
mod metadata;
mod post_execution;

#[cfg(feature = "debug")]
mod debug;

#[cfg(feature = "profile-any")]
use crate::profiler::Profiler;

#[cfg(feature = "profile-gas")]
use crate::profiler::InstructionLocation;

pub use balances::RuntimeBalances;
pub use memory::MemoryRange;

#[derive(Debug, Clone)]
/// VM interpreter.
///
/// The internal state of the VM isn't expose because the intended usage is to
/// either inspect the resulting receipts after a transaction execution, or the
/// resulting mutated transaction.
///
/// These can be obtained with the help of a [`crate::transactor::Transactor`]
/// or a client implementation.
pub struct Interpreter<S, Tx = ()> {
    registers: [Word; VM_REGISTER_COUNT],
    memory: Vec<u8>,
    frames: Vec<CallFrame>,
    receipts: Vec<Receipt>,
    tx: Tx,
    initial_balances: InitialBalances,
    storage: S,
    debugger: Debugger,
    context: Context,
    balances: RuntimeBalances,
    #[cfg(feature = "profile-any")]
    profiler: Profiler,
    params: ConsensusParameters,
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

    pub(crate) const fn is_unsafe_math(&self) -> bool {
        self.registers[REG_FLAG] & 0x01 == 0x01
    }

    pub(crate) const fn is_wrapping(&self) -> bool {
        self.registers[REG_FLAG] & 0x02 == 0x02
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

    /// Receipts generated by a transaction execution.
    pub fn receipts(&self) -> &[Receipt] {
        self.receipts.as_slice()
    }

    #[cfg(feature = "profile-gas")]
    fn current_location(&self) -> InstructionLocation {
        use crate::consts::*;
        InstructionLocation::new(
            self.frames.last().map(|frame| *frame.to()),
            self.registers[REG_PC] - self.registers[REG_IS],
        )
    }

    /// Reference to the underlying profiler
    #[cfg(feature = "profile-any")]
    pub const fn profiler(&self) -> &Profiler {
        &self.profiler
    }
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
    + UniqueIdentifier
    + field::Maturity
    + field::Inputs
    + field::Outputs
    + field::Witnesses
    + Into<Transaction>
{
    /// Casts the `Self` transaction into `&Script` if any.
    fn as_script(&self) -> Option<&Script>;

    /// Casts the `Self` transaction into `&mut Script` if any.
    fn as_script_mut(&mut self) -> Option<&mut Script>;

    /// Casts the `Self` transaction into `&Create` if any.
    fn as_create(&self) -> Option<&Create>;

    /// Dumps the `Output` by the `idx` into the `buf` buffer.
    fn output_to_mem(&mut self, idx: usize, buf: &mut [u8]) -> io::Result<usize> {
        self.outputs_mut()
            .get_mut(idx)
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "Invalid output idx"))
            .and_then(|o| o.read(buf))
    }

    /// Replaces the `Output::Message` with the `output`(should be also `Output::Message`)
    /// by the `idx` index.
    fn replace_message_output(&mut self, idx: usize, output: Output) -> Result<(), PanicReason> {
        // TODO increase the error granularity for this case - create a new variant of panic reason
        if !matches!(&output, Output::Message {
                recipient,
                ..
            } if recipient != &Address::zeroed())
        {
            return Err(PanicReason::OutputNotFound);
        }

        self.outputs_mut()
            .get_mut(idx)
            .and_then(|o| match o {
                Output::Message { recipient, .. } if recipient == &Address::zeroed() => Some(o),
                _ => None,
            })
            .map(|o| mem::replace(o, output))
            .map(|_| ())
            .ok_or(PanicReason::NonZeroMessageOutputRecipient)
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
                [&AssetId::BASE]
                .checked_add(gas_refund)
                .map(|v| *amount = v)
                .ok_or(CheckError::ArithmeticOverflow),

            // If revert, reset any non-base asset to its initial balance
            Output::Change { asset_id, amount, .. } if revert => {
                *amount = initial_balances[asset_id];
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
}

/// The alias of initial balances of the transaction.
pub type InitialBalances = BTreeMap<AssetId, Word>;

/// Methods that should be implemented by the checked metadata of supported transactions.
pub trait CheckedMetadata {
    /// Returns the initial balances from the checked metadata of the transaction.
    fn balances(self) -> InitialBalances;
}

impl CheckedMetadata for ScriptCheckedMetadata {
    fn balances(self) -> InitialBalances {
        self.initial_free_balances
    }
}

impl CheckedMetadata for CreateCheckedMetadata {
    fn balances(self) -> InitialBalances {
        self.initial_free_balances
    }
}
