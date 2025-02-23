// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

use crate::DbReader;
use anyhow::{format_err, Result};
use aptos_crypto::{
    hash::{CryptoHash, SPARSE_MERKLE_PLACEHOLDER_HASH},
    HashValue,
};
use aptos_state_view::{StateView, StateViewId};
use aptos_types::{
    access_path::AccessPath,
    account_state::AccountState,
    account_state_blob::AccountStateBlob,
    proof::SparseMerkleProof,
    state_store::{state_key::StateKey, state_value::StateValue},
    transaction::{Version, PRE_GENESIS_VERSION},
};
use move_core_types::account_address::AccountAddress;
use parking_lot::RwLock;
use scratchpad::{FrozenSparseMerkleTree, SparseMerkleTree, StateStoreStatus};
use std::{
    collections::{hash_map::Entry, HashMap},
    convert::{TryFrom, TryInto},
    sync::Arc,
};

/// `VerifiedStateView` is like a snapshot of the global state comprised of state view at two
/// levels, persistent storage and memory.
pub struct VerifiedStateView {
    /// For logging and debugging purpose, identifies what this view is for.
    id: StateViewId,

    /// A gateway implementing persistent storage interface, which can be a RPC client or direct
    /// accessor.
    reader: Arc<dyn DbReader>,

    /// The most recent version in persistent storage.
    latest_persistent_version: Option<Version>,

    /// The most recent state root hash in persistent storage.
    latest_persistent_state_root: HashValue,

    /// The in-memory version of sparse Merkle tree of which the states haven't been committed.
    speculative_state: FrozenSparseMerkleTree<StateValue>,

    /// The cache of verified account states from `reader` and `speculative_state_view`,
    /// represented by a hashmap with an account address as key and a pair of an ordered
    /// account state map and an an optional account state proof as value. When the VM queries an
    /// `access_path`, this cache will first check whether `reader_cache` is hit. If hit, it
    /// will return the corresponding value of that `access_path`; otherwise, the account state
    /// will be loaded into the cache from scratchpad or persistent storage in order as a
    /// deserialized ordered map and then be returned. If the VM queries this account again,
    /// the cached data can be read directly without bothering storage layer. The proofs in
    /// cache are needed by ScratchPad after VM execution to construct an in-memory sparse Merkle
    /// tree.
    /// ```text
    ///                      +----------------------------+
    ///                      | In-memory SparseMerkleTree <------+
    ///                      +-------------^--------------+      |
    ///                                    |                     |
    ///                                write sets                |
    ///                                    |          cached account state map
    ///                            +-------+-------+           proof
    ///                            |      V M      |             |
    ///                            +-------^-------+             |
    ///                                    |                     |
    ///                      value of `account_address/path`     |
    ///                                    |                     |
    ///        +---------------------------+---------------------+-------+
    ///        | +-------------------------+---------------------+-----+ |
    ///        | |           state_cache,     account_to_proof_cache   | |
    ///        | +---------------^---------------------------^---------+ |
    ///        |                 |                           |           |
    ///        |     state store values only        account state blob   |
    ///        |                 |                         proof         |
    ///        |                 |                           |           |
    ///        | +---------------+--------------+ +----------+---------+ |
    ///        | |      speculative_state       | |       reader       | |
    ///        | +------------------------------+ +--------------------+ |
    ///        +---------------------------------------------------------+
    /// ```
    account_state_cache: RwLock<HashMap<AccountAddress, AccountState>>,
    /// Cache of state key to state value, which is used in case of fine grained storage object.
    /// Eventually this should replace the `account_to_state_cache` as we deprecate account state blob
    /// completely and migrate to fine grained storage. A value of None in this cache reflects that
    /// the corresponding key has been deleted. This is a temporary hack until we support deletion
    /// in JMT node.
    state_cache: RwLock<HashMap<StateKey, StateValue>>,
    state_proof_cache: RwLock<HashMap<HashValue, SparseMerkleProof<StateValue>>>,
}

impl VerifiedStateView {
    /// Constructs a [`VerifiedStateView`] with persistent state view represented by
    /// `latest_persistent_state_root` plus a storage reader, and the in-memory speculative state
    /// on top of it represented by `speculative_state`.
    pub fn new(
        id: StateViewId,
        reader: Arc<dyn DbReader>,
        latest_persistent_version: Option<Version>,
        latest_persistent_state_root: HashValue,
        speculative_state: SparseMerkleTree<StateValue>,
    ) -> Self {
        // Hack: When there's no transaction in the db but state tree root hash is not the
        // placeholder hash, it implies that there's pre-genesis state present.
        let latest_persistent_version = latest_persistent_version.or_else(|| {
            if latest_persistent_state_root != *SPARSE_MERKLE_PLACEHOLDER_HASH {
                Some(PRE_GENESIS_VERSION)
            } else {
                None
            }
        });
        Self {
            id,
            reader,
            latest_persistent_version,
            latest_persistent_state_root,
            speculative_state: speculative_state.freeze(),
            account_state_cache: RwLock::new(HashMap::new()),
            state_cache: RwLock::new(HashMap::new()),
            state_proof_cache: RwLock::new(HashMap::new()),
        }
    }

    pub fn into_state_cache(self) -> StateCache {
        StateCache {
            frozen_base: self.speculative_state,
            accounts: self.account_state_cache.into_inner(),
            state_cache: self.state_cache.into_inner(),
            proofs: self.state_proof_cache.into_inner(),
        }
    }

    fn get_by_access_path(&self, access_path: &AccessPath) -> Result<Option<Vec<u8>>> {
        let address = access_path.address;
        let path = &access_path.path;

        // Lock for read first:
        if let Some(contents) = self.account_state_cache.read().get(&address) {
            return Ok(contents.get(path).cloned());
        }

        let state_store_value_option =
            self.get_state_value_internal(&StateKey::AccountAddressKey(address))?;

        // Hack: Convert the state store value to account blob option as that is the
        // only type of state value we support for now. This needs to change once we start
        // supporting tables and other fine grained resources.
        let new_account_blob = state_store_value_option
            .map(AccountStateBlob::try_from)
            .transpose()?
            .as_ref()
            .map(TryInto::try_into)
            .transpose()?
            .unwrap_or_default();

        // Now enter the locked region, and write if still empty.
        match self.account_state_cache.write().entry(address) {
            Entry::Occupied(occupied) => Ok(occupied.get().get(path).cloned()),
            Entry::Vacant(vacant) => Ok(vacant.insert(new_account_blob).get(path).cloned()),
        }
    }

    fn get_state_value_internal(&self, state_key: &StateKey) -> Result<Option<StateValue>> {
        // Do most of the work outside the write lock.
        let key_hash = state_key.hash();
        let state_store_value_option = match self.speculative_state.get(key_hash) {
            StateStoreStatus::ExistsInScratchPad(blob) => Some(blob),
            StateStoreStatus::DoesNotExist => None,
            // No matter it is in db or unknown, we have to query from db since even the
            // former case, we don't have the blob data but only its hash.
            StateStoreStatus::ExistsInDB | StateStoreStatus::Unknown => {
                let (state_store_value, proof) = match self.latest_persistent_version {
                    Some(version) => self
                        .reader
                        .get_state_value_with_proof_by_version(state_key, version)?,
                    None => (None, SparseMerkleProof::new(None, vec![])),
                };
                proof
                    .verify(
                        self.latest_persistent_state_root,
                        key_hash,
                        state_store_value.as_ref(),
                    )
                    .map_err(|err| {
                        format_err!(
                            "Proof is invalid for key {:?} with state root hash {:?}: {}",
                            state_key,
                            self.latest_persistent_state_root,
                            err
                        )
                    })?;

                // multiple threads may enter this code, and another thread might add
                // an address before this one. Thus the insertion might return a None here.
                self.state_proof_cache.write().insert(key_hash, proof);
                state_store_value
            }
        };

        Ok(state_store_value_option)
    }

    fn get_and_cache_state_value(&self, state_key: &StateKey) -> Result<Option<Vec<u8>>> {
        // First check if the cache has the state value.
        if let Some(contents) = self.state_cache.read().get(state_key) {
            // This can return None, which means the value has been deleted from the DB.
            return Ok(contents.maybe_bytes.as_ref().cloned());
        }
        let state_store_value_option = self.get_state_value_internal(state_key)?;
        // Update the cache if still empty
        let mut cache = self.state_cache.write();
        let new_value = cache
            .entry(state_key.clone())
            .or_insert_with(|| state_store_value_option.unwrap_or_default());

        Ok(new_value.maybe_bytes.as_ref().cloned())
    }
}

pub struct StateCache {
    pub frozen_base: FrozenSparseMerkleTree<StateValue>,
    pub accounts: HashMap<AccountAddress, AccountState>,
    pub state_cache: HashMap<StateKey, StateValue>,
    pub proofs: HashMap<HashValue, SparseMerkleProof<StateValue>>,
}

impl StateView for VerifiedStateView {
    fn id(&self) -> StateViewId {
        self.id
    }

    fn get_state_value(&self, state_key: &StateKey) -> Result<Option<Vec<u8>>> {
        // This is a hack to temporary support of legacy account address based access path. This should
        // be removed once we migrate to fine grained storage for all account resource.
        match state_key {
            StateKey::AccessPath(access_path) => self.get_by_access_path(access_path),
            _ => self.get_and_cache_state_value(state_key),
        }
    }

    fn is_genesis(&self) -> bool {
        self.latest_persistent_version.is_none()
    }
}
