// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, HashMap, HashSet};

use parking_lot::RwLock;

use simulacrum::store::in_mem_store::InMemoryStore;
use simulacrum::store::SimulatorStore;
use sui_types::base_types::{ObjectID, ObjectRef, SequenceNumber, SuiAddress};
use sui_types::committee::{Committee, EpochId};
use sui_types::digests::{ObjectDigest, TransactionDigest};
use sui_types::effects::{TransactionEffects, TransactionEffectsAPI, TransactionEvents};
use sui_types::error::SuiErrorKind;
use sui_types::messages_checkpoint::{
    CheckpointContents, CheckpointContentsDigest, CheckpointDigest, CheckpointSequenceNumber,
    VerifiedCheckpoint,
};
use sui_types::object::{Object, Owner};
use sui_types::storage::{
    BackingPackageStore, BackingStore, ChildObjectResolver, ObjectStore, PackageObject, ParentSync,
    load_package_object_from_object_store,
};
use sui_types::transaction::VerifiedTransaction;

use crate::fetcher::ObjectFetcher;

pub struct ForkingStore {
    inner: InMemoryStore,
    rpc_cache: RwLock<HashMap<ObjectID, Object>>,
    rpc_version_cache: RwLock<HashMap<(ObjectID, SequenceNumber), Object>>,
    deleted: RwLock<HashSet<ObjectID>>,
    fetcher: Box<dyn ObjectFetcher>,
    // Separate committee storage — InMemoryStore panics on non-sequential epoch insert
    committees: HashMap<EpochId, Committee>,
}

impl ForkingStore {
    pub fn new(fetcher: Box<dyn ObjectFetcher>) -> Self {
        Self {
            inner: InMemoryStore::default(),
            rpc_cache: RwLock::new(HashMap::new()),
            rpc_version_cache: RwLock::new(HashMap::new()),
            deleted: RwLock::new(HashSet::new()),
            fetcher,
            committees: HashMap::new(),
        }
    }

    pub fn seed_object(&mut self, obj: Object) {
        let id = obj.id();
        self.inner
            .update_objects(BTreeMap::from([(id, obj)]), vec![]);
    }

    pub fn mark_deleted(&mut self, id: ObjectID) {
        self.deleted.write().insert(id);
        self.rpc_cache.write().remove(&id);
    }

    fn try_rpc_fetch(&self, id: &ObjectID) -> Option<Object> {
        if self.deleted.read().contains(id) {
            return None;
        }
        if let Some(obj) = self.rpc_cache.read().get(id) {
            return Some(obj.clone());
        }
        match self.fetcher.fetch_object(id) {
            Ok(Some(obj)) => {
                self.rpc_cache.write().insert(*id, obj.clone());
                Some(obj)
            }
            _ => None,
        }
    }

    fn try_rpc_fetch_version(&self, id: &ObjectID, version: SequenceNumber) -> Option<Object> {
        if self.deleted.read().contains(id) {
            return None;
        }
        let key = (*id, version);
        if let Some(obj) = self.rpc_version_cache.read().get(&key) {
            return Some(obj.clone());
        }
        match self.fetcher.fetch_object_by_version(id, version) {
            Ok(Some(obj)) => {
                self.rpc_version_cache.write().insert(key, obj.clone());
                Some(obj)
            }
            _ => None,
        }
    }

    fn try_rpc_fetch_child(
        &self,
        child_id: &ObjectID,
        version_upper_bound: SequenceNumber,
    ) -> Option<Object> {
        if self.deleted.read().contains(child_id) {
            return None;
        }
        match self.fetcher.fetch_child_object(child_id, version_upper_bound) {
            Ok(Some(obj)) => {
                self.rpc_cache.write().insert(*child_id, obj.clone());
                Some(obj)
            }
            _ => None,
        }
    }
}

impl ObjectStore for ForkingStore {
    fn get_object(&self, object_id: &ObjectID) -> Option<Object> {
        if let Some(obj) = ObjectStore::get_object(&self.inner, object_id) {
            return Some(obj);
        }
        self.try_rpc_fetch(object_id)
    }

    fn get_object_by_key(
        &self,
        object_id: &ObjectID,
        version: SequenceNumber,
    ) -> Option<Object> {
        if let Some(obj) = self.inner.get_object_by_key(object_id, version) {
            return Some(obj);
        }
        self.try_rpc_fetch_version(object_id, version)
    }
}

impl BackingPackageStore for ForkingStore {
    fn get_package_object(
        &self,
        package_id: &ObjectID,
    ) -> sui_types::error::SuiResult<Option<PackageObject>> {
        load_package_object_from_object_store(self, package_id)
    }
}

impl ChildObjectResolver for ForkingStore {
    fn read_child_object(
        &self,
        parent: &ObjectID,
        child: &ObjectID,
        child_version_upper_bound: SequenceNumber,
    ) -> sui_types::error::SuiResult<Option<Object>> {
        // Try local store first
        match self
            .inner
            .read_child_object(parent, child, child_version_upper_bound)
        {
            Ok(Some(obj)) => return Ok(Some(obj)),
            Ok(None) => {}
            Err(_) => {}
        }

        let child_object = match self.try_rpc_fetch_child(child, child_version_upper_bound) {
            Some(obj) => obj,
            None => return Ok(None),
        };

        if child_object.owner != Owner::ObjectOwner((*parent).into()) {
            return Err(SuiErrorKind::InvalidChildObjectAccess {
                object: *child,
                given_parent: *parent,
                actual_owner: child_object.owner.clone(),
            }
            .into());
        }

        Ok(Some(child_object))
    }

    fn get_object_received_at_version(
        &self,
        owner: &ObjectID,
        receiving_object_id: &ObjectID,
        receive_object_at_version: SequenceNumber,
        epoch_id: EpochId,
    ) -> sui_types::error::SuiResult<Option<Object>> {
        match self.inner.get_object_received_at_version(
            owner,
            receiving_object_id,
            receive_object_at_version,
            epoch_id,
        ) {
            Ok(Some(obj)) => return Ok(Some(obj)),
            _ => {}
        }
        match self.try_rpc_fetch_version(receiving_object_id, receive_object_at_version) {
            Some(obj) => Ok(Some(obj)),
            None => Ok(None),
        }
    }
}

impl ParentSync for ForkingStore {
    fn get_latest_parent_entry_ref_deprecated(&self, _object_id: ObjectID) -> Option<ObjectRef> {
        None
    }
}

impl SimulatorStore for ForkingStore {
    fn get_checkpoint_by_sequence_number(
        &self,
        seq: CheckpointSequenceNumber,
    ) -> Option<VerifiedCheckpoint> {
        self.inner.get_checkpoint_by_sequence_number(seq).cloned()
    }

    fn get_checkpoint_by_digest(&self, digest: &CheckpointDigest) -> Option<VerifiedCheckpoint> {
        self.inner.get_checkpoint_by_digest(digest).cloned()
    }

    fn get_highest_checkpint(&self) -> Option<VerifiedCheckpoint> {
        self.inner.get_highest_checkpint().cloned()
    }

    fn get_checkpoint_contents(
        &self,
        digest: &CheckpointContentsDigest,
    ) -> Option<CheckpointContents> {
        self.inner.get_checkpoint_contents(digest).cloned()
    }

    fn get_committee_by_epoch(&self, epoch: EpochId) -> Option<Committee> {
        self.committees.get(&epoch).cloned()
    }

    fn get_transaction(&self, digest: &TransactionDigest) -> Option<VerifiedTransaction> {
        self.inner.get_transaction(digest).cloned()
    }

    fn get_transaction_effects(&self, digest: &TransactionDigest) -> Option<TransactionEffects> {
        self.inner.get_transaction_effects(digest).cloned()
    }

    fn get_transaction_events(&self, digest: &TransactionDigest) -> Option<TransactionEvents> {
        self.inner.get_transaction_events(digest).cloned()
    }

    fn get_object(&self, id: &ObjectID) -> Option<Object> {
        ObjectStore::get_object(self, id)
    }

    fn get_object_at_version(&self, id: &ObjectID, version: SequenceNumber) -> Option<Object> {
        ObjectStore::get_object_by_key(self, id, version)
    }

    fn get_system_state(&self) -> sui_types::sui_system_state::SuiSystemState {
        sui_types::sui_system_state::get_sui_system_state(self)
            .expect("SuiSystemState not found — was the store bootstrapped?")
    }

    fn get_clock(&self) -> sui_types::clock::Clock {
        let obj = ObjectStore::get_object(self, &sui_types::SUI_CLOCK_OBJECT_ID)
            .expect("Clock not found");
        obj.to_rust().expect("clock object should deserialize")
    }

    fn owned_objects(&self, owner: SuiAddress) -> Box<dyn Iterator<Item = Object> + '_> {
        Box::new(self.inner.owned_objects(owner).cloned())
    }

    fn insert_checkpoint(&mut self, checkpoint: VerifiedCheckpoint) {
        self.inner.insert_checkpoint(checkpoint);
    }

    fn insert_checkpoint_contents(&mut self, contents: CheckpointContents) {
        self.inner.insert_checkpoint_contents(contents);
    }

    fn insert_committee(&mut self, committee: Committee) {
        self.committees.insert(committee.epoch as EpochId, committee);
    }

    fn insert_executed_transaction(
        &mut self,
        transaction: VerifiedTransaction,
        effects: TransactionEffects,
        events: TransactionEvents,
        written_objects: BTreeMap<ObjectID, Object>,
    ) {
        for (id, _, _) in effects.deleted() {
            self.deleted.write().insert(id);
            self.rpc_cache.write().remove(&id);
        }
        self.inner
            .insert_executed_transaction(transaction, effects, events, written_objects);
    }

    fn insert_transaction(&mut self, transaction: VerifiedTransaction) {
        self.inner.insert_transaction(transaction);
    }

    fn insert_transaction_effects(&mut self, effects: TransactionEffects) {
        self.inner.insert_transaction_effects(effects);
    }

    fn insert_events(&mut self, tx_digest: &TransactionDigest, events: TransactionEvents) {
        self.inner.insert_events(tx_digest, events);
    }

    fn update_objects(
        &mut self,
        written_objects: BTreeMap<ObjectID, Object>,
        deleted_objects: Vec<(ObjectID, SequenceNumber, ObjectDigest)>,
    ) {
        for (id, _, _) in &deleted_objects {
            self.deleted.write().insert(*id);
            self.rpc_cache.write().remove(id);
        }
        for id in written_objects.keys() {
            self.deleted.write().remove(id);
        }
        self.inner.update_objects(written_objects, deleted_objects);
    }

    fn backing_store(&self) -> &dyn BackingStore {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetcher::MockFetcher;

    fn test_object(id: ObjectID) -> Object {
        Object::with_id_owner_for_testing(id, SuiAddress::random_for_testing_only())
    }

    #[test]
    fn local_object_returned_first() {
        let mock = MockFetcher::new();
        let mut store = ForkingStore::new(Box::new(mock));
        let obj = test_object(ObjectID::random());
        let id = obj.id();
        store.seed_object(obj.clone());

        let result = ObjectStore::get_object(&store, &id);
        assert!(result.is_some());
        assert_eq!(result.unwrap().id(), id);
    }

    #[test]
    fn rpc_fallback_on_miss() {
        let mut mock = MockFetcher::new();
        let obj = test_object(ObjectID::random());
        let id = obj.id();
        mock.add_object(obj.clone());

        let store = ForkingStore::new(Box::new(mock));
        let result = ObjectStore::get_object(&store, &id);
        assert!(result.is_some());
        assert_eq!(result.unwrap().id(), id);
    }

    #[test]
    fn returns_none_when_nowhere() {
        let mock = MockFetcher::new();
        let store = ForkingStore::new(Box::new(mock));
        assert!(ObjectStore::get_object(&store, &ObjectID::random()).is_none());
    }

    #[test]
    fn deleted_objects_blocked_from_rpc() {
        let mut mock = MockFetcher::new();
        let obj = test_object(ObjectID::random());
        let id = obj.id();
        mock.add_object(obj);

        let mut store = ForkingStore::new(Box::new(mock));
        store.mark_deleted(id);

        assert!(ObjectStore::get_object(&store, &id).is_none());
    }
}
