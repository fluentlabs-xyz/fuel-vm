//! Types to help constrain inputs to functions to only what is used.
use std::ops::Deref;
use std::ops::DerefMut;

use fuel_asm::PanicReason;
use fuel_asm::Word;
use fuel_types::ContractId;

use crate::consts::VM_MAX_RAM;
use crate::interpreter::VmMemory;
use crate::prelude::Bug;
use crate::prelude::BugId;
use crate::prelude::BugVariant;
use crate::prelude::RuntimeError;

pub mod reg_key;

/// A range of memory that has been checked that it fits into the VM memory.
#[derive(Clone)]
pub struct CheckedMemRange(core::ops::Range<usize>);

/// A range of memory that has been checked that it fits into the VM memory.
#[derive(Clone)]
// TODO: Replace `LEN` constant with a generic object that implements some trait that knows
//  the static size of the generic.
pub struct CheckedMemConstLen<const LEN: usize>(CheckedMemRange);

/// A range of memory that has been checked that it fits into the VM memory.
/// This range can be used to read a value of type `T` from memory.
#[derive(Clone)]
// TODO: Merge this type with `CheckedMemConstLen`.
pub struct CheckedMemValue<T>(CheckedMemRange, core::marker::PhantomData<T>);

impl<T> CheckedMemValue<T> {
    /// Create a new const sized memory range.
    pub fn new<const SIZE: usize>(address: Word) -> Result<Self, RuntimeError> {
        Ok(Self(
            CheckedMemRange::new_const::<SIZE>(address)?,
            core::marker::PhantomData,
        ))
    }

    /// Try to read a value of type `T` from memory.
    pub fn from<const SIZE: usize>(self, memory: &VmMemory) -> Result<T, RuntimeError>
    where
        T: From<[u8; SIZE]>,
    {
        let bytes = memory.read_bytes(self.0.start())?;
        Ok(T::from(bytes))
    }

    pub fn read_array<const SIZE: usize>(self, memory: &VmMemory) -> Result<[u8; SIZE], RuntimeError> {
        memory.read_bytes(self.0.start())
    }

    /// Write access to the memory range.
    pub fn write<const SIZE: usize>(self, memory: &VmMemory) -> Result<&mut [u8], RuntimeError> {
        todo!("write access");
    }

    /// The start of the range.
    pub fn start(&self) -> usize {
        self.0.start()
    }

    /// The end of the range.
    pub fn end(&self) -> usize {
        self.0.end()
    }

    #[cfg(test)]
    /// Inspect a value of type `T` from memory.
    pub fn inspect(self, memory: &VmMemory) -> T
    where
        T: std::io::Write + Default,
    {
        let mut t = T::default();
        t.write_all(&memory[self.0 .0]).unwrap();
        t
    }
}

impl CheckedMemRange {
    const DEFAULT_CONSTRAINT: core::ops::Range<Word> = 0..VM_MAX_RAM;

    /// Create a new const sized memory range.
    pub fn new_const<const SIZE: usize>(address: Word) -> Result<Self, RuntimeError> {
        Self::new(address, SIZE)
    }

    /// Create a new memory range.
    pub fn new(address: Word, size: usize) -> Result<Self, RuntimeError> {
        Self::new_inner(address as usize, size, Self::DEFAULT_CONSTRAINT)
    }

    /// Create a new memory range with a custom constraint.
    /// The min of the constraints end and `VM_MAX_RAM` will be used.
    pub fn new_with_constraint(
        address: Word,
        size: usize,
        constraint: core::ops::Range<Word>,
    ) -> Result<Self, RuntimeError> {
        if constraint.end > VM_MAX_RAM {
            return Err(Bug::new(BugId::ID009, BugVariant::InvalidMemoryConstraint).into());
        }
        Self::new_inner(address as usize, size, constraint)
    }

    /// Create a new memory range, checks that the range fits into the constraint.
    fn new_inner(address: usize, size: usize, constraint: core::ops::Range<Word>) -> Result<Self, RuntimeError> {
        let (end, of) = address.overflowing_add(size);
        let range = address..end;

        if of
            || !constraint.contains(&(range.start as Word))
            || size != 0 && !constraint.contains(&((range.end - 1) as Word))
        {
            return Err(PanicReason::MemoryOverflow.into());
        }
        Ok(Self(range))
    }

    /// The start of the range.
    pub fn start(&self) -> usize {
        self.0.start
    }

    /// The end of the range.
    pub fn end(&self) -> usize {
        self.0.end
    }

    /// The length of the range.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// This function is safe because it is only used to shrink the range
    /// and worst case the range will be empty.
    pub fn shrink_end(&mut self, by: usize) {
        self.0 = self.0.start..self.0.end.saturating_sub(by);
    }

    /// This function is safe because it is only used to grow the range
    /// and worst case the range will be empty.
    /// TODO: is the really safe??
    pub fn grow_start(&mut self, by: usize) {
        self.0 = self.0.start.saturating_add(by)..self.0.end;
    }

    pub fn read_to_vec(&self, memory: &VmMemory) -> Vec<u8> {
        memory
            .read(self.start(), self.len())
            .expect("Unreachable! Checked access")
            .copied()
            .collect()
    }

    pub fn clear(&self, memory: &mut VmMemory) {
        memory
            .clear_unchecked(self.start(), self.len())
            .expect("Unreachable! Checked access")
    }
}

impl<const LEN: usize> CheckedMemConstLen<LEN> {
    /// Create a new const sized memory range.
    pub fn new(address: Word) -> Result<Self, RuntimeError> {
        Ok(Self(CheckedMemRange::new_const::<LEN>(address)?))
    }

    /// Create a new memory range with a custom constraint.
    /// Panics if constraints end > `VM_MAX_RAM`.
    pub fn new_with_constraint(address: Word, constraint: core::ops::Range<Word>) -> Result<Self, RuntimeError> {
        assert!(constraint.end <= VM_MAX_RAM, "Constraint end must be <= VM_MAX_RAM.");
        Ok(Self(CheckedMemRange::new_inner(address as usize, LEN, constraint)?))
    }

    pub fn read(&self, memory: &VmMemory) -> [u8; LEN] {
        memory.read_bytes(self.start()).expect("Unreachable! Checked access")
    }
}

/// Location of an instructing collected during runtime
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InstructionLocation {
    /// Context, i.e. current contract. None if running a script.
    pub context: Option<ContractId>,
    /// Offset from the IS register
    pub offset: u64,
}

impl<const LEN: usize> Deref for CheckedMemConstLen<LEN> {
    type Target = CheckedMemRange;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<const LEN: usize> DerefMut for CheckedMemConstLen<LEN> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
