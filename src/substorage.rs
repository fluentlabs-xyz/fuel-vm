//! SubStorage is three layer storage needed for execution of transactions
//!
//! Most notably `SubStorage` is used inside `Transactor` and contains three slices of storage data:
//! 1. DB connector that need to implement `InterpreterStorage` trait
//! 2. Commited storage that represent changes done and commited by previous transactions.
//! 3. Pending storage for current execution of transaction, this storage can be discarded if transaction execution fails or needs to be reverted or when we want to abandon transaction for any other reason.
//!
use fuel_asm::Word;
use fuel_storage::{MerkleRoot, MerkleStorage, Storage};
use fuel_tx::{Address, Bytes32, Color, ContractId, Salt};
use hashbrown::{hash_map::Entry, HashMap};
use std::borrow::Cow;

use crate::{contract::Contract, storage::InterpreterStorage};

/// SubStorage of transaction bundle execution
pub struct SubStorage<STORAGE> {
    /// State from database targeted for a particular block number or maybe even block hash.
    state: STORAGE,
    /// Commited storage
    commited_storage: HashMap<ContractId, ContractData>,
    /// Pending storage related to present executed transaction.
    pending_storage: HashMap<ContractId, ContractData>,
    /// VM metadata
    metadata: Metadata,
}

/// Metadata needed for execution of VM
#[derive(Default, Copy, Clone)]
pub struct Metadata {
    coinbase: Address,
    block_height: u32,
}

impl Metadata {
    /// Constructor
    pub fn new(coinbase: Address, block_height: u32) -> Self {
        Self { coinbase, block_height }
    }
    /// Return coinbase
    pub fn coinbase(&self) -> &Address {
        &self.coinbase
    }
    /// Return block height
    pub fn block_height(&self) -> u32 {
        self.block_height
    }
}

/// Contract related data
#[derive(Debug, Clone)]
pub struct ContractData {
    bytecode: Option<Contract>,
    balance: HashMap<Color, Option<Word>>,
    storage: HashMap<Bytes32, Option<Bytes32>>,
    root: Option<(Salt, Bytes32)>,
}

impl Default for ContractData {
    fn default() -> Self {
        Self {
            bytecode: None,
            balance: HashMap::new(),
            storage: HashMap::new(),
            root: None,
        }
    }
}

impl<STORAGE> SubStorage<STORAGE> {
    /// constructor
    pub fn new(state: STORAGE, metadata: Metadata) -> Self {
        Self {
            state,
            commited_storage: HashMap::new(),
            pending_storage: HashMap::new(),
            metadata,
        }
    }

    /// Take pending_storage and merge it inside commited_storage
    pub fn commit_pending(&mut self) {
        for (contract_id, data) in self.pending_storage.drain() {
            match self.commited_storage.entry(contract_id) {
                Entry::Vacant(entry) => {
                    entry.insert(data);
                }
                Entry::Occupied(mut entry) => {
                    // merge diff
                    let commited = entry.get_mut();
                    commited.balance.extend(data.balance.into_iter());
                    commited.storage.extend(data.storage.into_iter());
                    if data.bytecode.is_some() {
                        commited.bytecode = data.bytecode;
                    }
                    // todo check how is this going to be calculated.
                    commited.root = data.root;
                }
            }
        }
    }

    /// reject and clear pending storage.
    pub fn reject_pending(&mut self) {
        self.pending_storage.clear();
    }

    /// commited state
    pub fn commited_storage(&self) -> &HashMap<ContractId, ContractData> {
        &self.commited_storage
    }
}

impl<STORAGE> Storage<ContractId, Contract> for SubStorage<STORAGE>
where
    STORAGE: InterpreterStorage,
{
    type Error = STORAGE::DataError;

    /// storage_contract_insert
    fn insert(&mut self, id: &ContractId, bytecode: &Contract) -> Result<Option<Contract>, Self::Error> {
        let contract = self.pending_storage.entry(*id).or_default();
        // shold we panic if root is already set?
        contract.bytecode = Some(bytecode.clone());
        Ok(contract.bytecode.clone())
    }

    fn remove(&mut self, _key: &ContractId) -> Result<Option<Contract>, Self::Error> {
        unreachable!()
    }

    /// storage_contract
    fn get(&self, id: &ContractId) -> Result<Option<Cow<'_, Contract>>, Self::Error> {
        // is there posibility to have set pending storage root inside one tx?
        if let Some(contract) = self.pending_storage.get(id) {
            if let Some(ref bytecode) = contract.bytecode {
                return Ok(Some(Cow::Owned(bytecode.clone())));
            }
        }
        // check commited contract
        if let Some(contract) = self.commited_storage.get(id) {
            if let Some(ref bytecode) = contract.bytecode {
                return Ok(Some(Cow::Owned(bytecode.clone())));
            }
        }

        // check database
        let res = self.state.storage_contract(id);
        res
    }

    /// storage_contract_exist
    fn contains_key(&self, id: &ContractId) -> Result<bool, Self::Error> {
        // IMPL
        if let Some(contract) = self.pending_storage.get(id) {
            if contract.bytecode.is_some() {
                return Ok(true);
            }
        }
        // check commited contract
        if let Some(contract) = self.commited_storage.get(id) {
            if contract.bytecode.is_some() {
                return Ok(true);
            }
        }

        // check database
        self.state.storage_contract_exists(id)
    }
}

impl<STORAGE> Storage<ContractId, (Salt, Bytes32)> for SubStorage<STORAGE>
where
    STORAGE: InterpreterStorage,
{
    type Error = STORAGE::DataError;

    /// storage_contract_root_insert
    fn insert(&mut self, key: &ContractId, value: &(Salt, Bytes32)) -> Result<Option<(Salt, Bytes32)>, Self::Error> {
        let contract = self.pending_storage.entry(*key).or_default();
        // shold we panic if root is already set?
        contract.root = Some(*value);
        Ok(contract.root)
    }

    fn remove(&mut self, _key: &ContractId) -> Result<Option<(Salt, Bytes32)>, Self::Error> {
        unreachable!()
    }

    /// storage_contract_root
    fn get(&self, id: &ContractId) -> Result<Option<Cow<'_, (Salt, Bytes32)>>, Self::Error> {
        // is there posibility to have set pending storage root inside one tx?
        if let Some(contract) = self.pending_storage.get(id) {
            if let Some(root) = contract.root {
                return Ok(Some(Cow::Owned(root)));
            }
        }
        // check commited contract
        if let Some(contract) = self.commited_storage.get(id) {
            if let Some(root) = contract.root {
                return Ok(Some(Cow::Owned(root)));
            }
        }

        // check database
        self.state.storage_contract_root(id)
    }

    fn contains_key(&self, _key: &ContractId) -> Result<bool, Self::Error> {
        unreachable!()
    }
}

impl<STORAGE> MerkleStorage<ContractId, Bytes32, Bytes32> for SubStorage<STORAGE>
where
    STORAGE: InterpreterStorage,
{
    type Error = STORAGE::DataError;

    /// merkle_contract_state_insert
    fn insert(
        &mut self,
        id: &ContractId,
        storage_id: &Bytes32,
        value: &Bytes32,
    ) -> Result<Option<Bytes32>, Self::Error> {
        let contract = self.pending_storage.entry(*id).or_default();
        // shold we panic if root is already set?
        contract.storage.insert(*storage_id, Some(*value));
        Ok(Some(*value))
    }

    /// merkle_contract_state
    fn get(&self, id: &ContractId, storage_id: &Bytes32) -> Result<Option<Cow<'_, Bytes32>>, Self::Error> {
        if let Some(contract) = self.pending_storage.get(id) {
            if let Some(value) = contract.storage.get(storage_id) {
                return Ok(value.map(Cow::Owned));
            }
        }
        // check commited contract
        if let Some(contract) = self.commited_storage.get(id) {
            if let Some(value) = contract.storage.get(storage_id) {
                return Ok(value.map(Cow::Owned));
            }
        }

        // check database
        self.state.merkle_contract_state(id, storage_id)
    }

    fn remove(&mut self, _parent: &ContractId, _key: &Bytes32) -> Result<Option<Bytes32>, Self::Error> {
        unreachable!()
    }

    fn contains_key(&self, _parent: &ContractId, _key: &Bytes32) -> Result<bool, Self::Error> {
        unreachable!()
    }

    fn root(&mut self, _parent: &ContractId) -> Result<MerkleRoot, Self::Error> {
        unreachable!()
    }
}

impl<STORAGE> MerkleStorage<ContractId, Color, Word> for SubStorage<STORAGE>
where
    STORAGE: InterpreterStorage,
{
    type Error = STORAGE::DataError;

    /// merkle_contract_color_balance_insert
    fn insert(&mut self, id: &ContractId, asset_id: &Color, balance: &Word) -> Result<Option<Word>, Self::Error> {
        let contract = self.pending_storage.entry(*id).or_default();
        // shold we panic if root is already set?
        contract.balance.insert(*asset_id, Some(*balance));
        Ok(Some(*balance))
    }

    /// merkle_contract_color_balance
    fn get(&self, id: &ContractId, asset_id: &Color) -> Result<Option<Cow<'_, Word>>, Self::Error> {
        if let Some(contract) = self.pending_storage.get(id) {
            if let Some(value) = contract.balance.get(asset_id) {
                return Ok(value.map(Cow::Owned));
            }
        }
        // check commited contract
        if let Some(contract) = self.commited_storage.get(id) {
            if let Some(value) = contract.balance.get(asset_id) {
                return Ok(value.map(Cow::Owned));
            }
        }

        // check database
        self.state
            .merkle_contract_color_balance(id, asset_id)
            .map(|t| t.map(Cow::Owned))
    }

    fn remove(&mut self, _parent: &ContractId, _key: &Color) -> Result<Option<Word>, Self::Error> {
        unreachable!()
    }

    fn contains_key(&self, _parent: &ContractId, _key: &Color) -> Result<bool, Self::Error> {
        unreachable!()
    }

    fn root(&mut self, _parent: &ContractId) -> Result<MerkleRoot, Self::Error> {
        unreachable!()
    }
}

impl<STORAGE> InterpreterStorage for SubStorage<STORAGE>
where
    STORAGE: InterpreterStorage,
{
    type DataError = STORAGE::DataError;

    fn block_height(&self) -> Result<u32, Self::DataError> {
        Ok(self.metadata.block_height)
    }

    fn block_hash(&self, block_height: u32) -> Result<Bytes32, Self::DataError> {
        if block_height > self.metadata.block_height() {
            return Ok(Bytes32::zeroed());
        }
        self.state.block_hash(block_height)
    }

    fn coinbase(&self) -> Result<Address, Self::DataError> {
        Ok(*self.metadata.coinbase())
    }
}
