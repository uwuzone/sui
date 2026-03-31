// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::Result;
use sui_json_rpc_types::{SuiGetPastObjectRequest, SuiObjectDataOptions, SuiPastObjectResponse};
use sui_sdk::{SuiClient, SuiClientBuilder};
use sui_types::base_types::{ObjectID, SequenceNumber, SuiAddress};
use sui_types::object::Object;

/// Abstraction over Sui RPC for fetching objects.
/// Sync interface — implementations handle async-to-sync bridging internally.
pub trait ObjectFetcher: Send + Sync {
    fn fetch_object(&self, id: &ObjectID) -> Result<Option<Object>>;
    fn fetch_object_by_version(
        &self,
        id: &ObjectID,
        version: SequenceNumber,
    ) -> Result<Option<Object>>;
    fn fetch_child_object(
        &self,
        child_id: &ObjectID,
        version_upper_bound: SequenceNumber,
    ) -> Result<Option<Object>>;
    fn fetch_chain_id(&self) -> Result<String>;
    fn fetch_owned_objects(&self, owner: SuiAddress) -> Result<Vec<Object>>;
}

/// Mock implementation for unit tests. Pre-populate with `add_object`.
pub struct MockFetcher {
    objects: Mutex<HashMap<ObjectID, Object>>,
    chain_id: String,
}

impl MockFetcher {
    pub fn new() -> Self {
        Self {
            objects: Mutex::new(HashMap::new()),
            chain_id: "mock-chain".into(),
        }
    }

    pub fn with_chain_id(mut self, chain_id: String) -> Self {
        self.chain_id = chain_id;
        self
    }

    pub fn add_object(&mut self, obj: Object) {
        self.objects.lock().unwrap().insert(obj.id(), obj);
    }
}

impl ObjectFetcher for MockFetcher {
    fn fetch_object(&self, id: &ObjectID) -> Result<Option<Object>> {
        Ok(self.objects.lock().unwrap().get(id).cloned())
    }

    fn fetch_object_by_version(
        &self,
        id: &ObjectID,
        _version: SequenceNumber,
    ) -> Result<Option<Object>> {
        Ok(self.objects.lock().unwrap().get(id).cloned())
    }

    fn fetch_child_object(
        &self,
        child_id: &ObjectID,
        _version_upper_bound: SequenceNumber,
    ) -> Result<Option<Object>> {
        Ok(self.objects.lock().unwrap().get(child_id).cloned())
    }

    fn fetch_chain_id(&self) -> Result<String> {
        Ok(self.chain_id.clone())
    }

    fn fetch_owned_objects(&self, owner: SuiAddress) -> Result<Vec<Object>> {
        use sui_types::object::Owner;
        Ok(self
            .objects
            .lock()
            .unwrap()
            .values()
            .filter(|obj| matches!(obj.owner, Owner::AddressOwner(addr) if addr == owner))
            .cloned()
            .collect())
    }
}

/// Real RPC fetcher. Creates a dedicated tokio runtime for sync-over-async bridging.
pub struct SyncRpcFetcher {
    runtime: tokio::runtime::Runtime,
    client: SuiClient,
}

impl SyncRpcFetcher {
    pub fn new(rpc_url: &str) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let client = runtime.block_on(SuiClientBuilder::default().build(rpc_url))?;
        Ok(Self { runtime, client })
    }
}

impl ObjectFetcher for SyncRpcFetcher {
    fn fetch_object(&self, id: &ObjectID) -> Result<Option<Object>> {
        self.runtime.block_on(async {
            let response = self
                .client
                .read_api()
                .get_object_with_options(*id, SuiObjectDataOptions::bcs_lossless())
                .await?;
            match response.object() {
                Ok(data) => Ok(Some(TryInto::<Object>::try_into(data.clone())?)),
                Err(_) => Ok(None),
            }
        })
    }

    fn fetch_object_by_version(
        &self,
        id: &ObjectID,
        version: SequenceNumber,
    ) -> Result<Option<Object>> {
        self.runtime.block_on(async {
            let request = SuiGetPastObjectRequest {
                object_id: *id,
                version,
            };
            let responses = self
                .client
                .read_api()
                .try_multi_get_parsed_past_object(
                    vec![request],
                    SuiObjectDataOptions::bcs_lossless(),
                )
                .await?;
            match responses.into_iter().next() {
                Some(SuiPastObjectResponse::VersionFound(data)) => {
                    Ok(Some(TryInto::<Object>::try_into(data)?))
                }
                _ => Ok(None),
            }
        })
    }

    fn fetch_child_object(
        &self,
        child_id: &ObjectID,
        version_upper_bound: SequenceNumber,
    ) -> Result<Option<Object>> {
        self.runtime.block_on(async {
            let response = self
                .client
                .read_api()
                .try_get_object_before_version(*child_id, version_upper_bound)
                .await?;
            match response {
                SuiPastObjectResponse::VersionFound(data) => {
                    Ok(Some(TryInto::<Object>::try_into(data)?))
                }
                _ => Ok(None),
            }
        })
    }

    fn fetch_chain_id(&self) -> Result<String> {
        self.runtime
            .block_on(async { Ok(self.client.read_api().get_chain_identifier().await?) })
    }

    fn fetch_owned_objects(&self, owner: SuiAddress) -> Result<Vec<Object>> {
        use sui_json_rpc_types::SuiObjectResponseQuery;

        self.runtime.block_on(async {
            let query = SuiObjectResponseQuery::new(
                None,
                Some(SuiObjectDataOptions::bcs_lossless()),
            );
            let mut objects = Vec::new();
            let mut cursor = None;
            loop {
                let page = self
                    .client
                    .read_api()
                    .get_owned_objects(owner, Some(query.clone()), cursor, None)
                    .await?;
                for response in &page.data {
                    if let Ok(data) = response.object() {
                        if let Ok(obj) = TryInto::<Object>::try_into(data.clone()) {
                            objects.push(obj);
                        }
                    }
                }
                if !page.has_next_page {
                    break;
                }
                cursor = page.next_cursor;
            }
            Ok(objects)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sui_types::base_types::SuiAddress;

    #[test]
    fn mock_returns_inserted_object() {
        let mut mock = MockFetcher::new();
        let obj =
            Object::with_id_owner_for_testing(ObjectID::random(), SuiAddress::random_for_testing_only());
        let id = obj.id();
        mock.add_object(obj.clone());
        let result = mock.fetch_object(&id).unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().id(), id);
    }

    #[test]
    fn mock_returns_none_for_missing() {
        let mock = MockFetcher::new();
        assert!(mock.fetch_object(&ObjectID::random()).unwrap().is_none());
    }
}
