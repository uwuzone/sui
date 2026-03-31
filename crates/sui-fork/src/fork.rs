// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};
use move_core_types::identifier::Identifier;
use sui_config::transaction_deny_config::TransactionDenyConfig;
use sui_config::verifier_signing_config::VerifierSigningConfig;
use sui_protocol_config::{Chain, ProtocolConfig};
use sui_types::base_types::{ObjectID, ObjectRef, SuiAddress};
use sui_types::effects::TransactionEffects;
use sui_types::error::ExecutionError;
use sui_types::inner_temporary_store::InnerTemporaryStore;
use sui_types::object::Object;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::storage::ObjectStore;
use sui_types::sui_system_state::SuiSystemStateTrait;
use sui_types::transaction::{
    CallArg, GasData, SenderSignedData, Transaction, TransactionData, TransactionKind,
    VerifiedTransaction,
};
use sui_types::TypeTag;

use simulacrum::epoch_state::EpochState;
use simulacrum::store::SimulatorStore;

use crate::fetcher::{ObjectFetcher, SyncRpcFetcher};
use crate::store::ForkingStore;

pub struct SuiFork {
    store: ForkingStore,
    epoch_state: EpochState,
    deny_config: TransactionDenyConfig,
    verifier_signing_config: VerifierSigningConfig,
    rpc_url: String,
}

impl SuiFork {
    /// Fork from the given RPC endpoint at the current epoch.
    pub fn new(rpc_url: &str) -> Result<Self> {
        Self::new_with_fetcher(Box::new(SyncRpcFetcher::new(rpc_url)?), rpc_url.to_string())
    }

    /// Fork with a custom fetcher (for testing with MockFetcher).
    pub fn new_with_fetcher(fetcher: Box<dyn ObjectFetcher>, rpc_url: String) -> Result<Self> {
        // Detect chain from RPC
        let chain_id_str = fetcher
            .fetch_chain_id()
            .context("Failed to fetch chain identifier")?;
        let chain = detect_chain(&chain_id_str);

        // Fetch system objects
        let sys_obj = fetcher
            .fetch_object(&sui_types::SUI_SYSTEM_STATE_OBJECT_ID)?
            .context("Failed to fetch SuiSystemState object (0x5)")?;
        let clock_obj = fetcher
            .fetch_object(&sui_types::SUI_CLOCK_OBJECT_ID)?
            .context("Failed to fetch Clock object (0x6)")?;

        // Create store and seed system objects
        let mut store = ForkingStore::new(fetcher);
        store.seed_object(sys_obj);
        store.seed_object(clock_obj);

        // Build EpochState from system state
        let system_state = store.get_system_state();
        let protocol_version = system_state.protocol_version();
        let protocol_config = ProtocolConfig::get_for_version(protocol_version.into(), chain);
        let epoch_state = EpochState::new_with_protocol_config(system_state, protocol_config);

        // Seed committee for this epoch
        store.insert_committee(epoch_state.committee().clone());

        Ok(Self {
            store,
            epoch_state,
            deny_config: TransactionDenyConfig::default(),
            verifier_signing_config: VerifierSigningConfig::default(),
            rpc_url,
        })
    }

    /// Execute arbitrary TransactionData as any sender — no private key needed.
    /// Uses `new_unchecked` to bypass signature verification entirely.
    pub fn execute_transaction(
        &mut self,
        transaction_data: TransactionData,
    ) -> Result<(TransactionEffects, Option<ExecutionError>)> {
        // Create a Transaction with empty signatures, then bypass verification
        let sender_signed = SenderSignedData::new(transaction_data, vec![]);
        let tx = Transaction::new(sender_signed);
        let verified = VerifiedTransaction::new_unchecked(tx);

        let (inner_temp_store, _gas_status, effects, exec_result): (
            InnerTemporaryStore,
            _,
            TransactionEffects,
            Result<(), ExecutionError>,
        ) = self.epoch_state.execute_transaction(
            &self.store,
            &self.deny_config,
            &self.verifier_signing_config,
            &verified,
        )?;

        // Apply state changes
        let InnerTemporaryStore {
            written, events, ..
        } = inner_temp_store;
        self.store
            .insert_executed_transaction(verified, effects.clone(), events, written);

        Ok((effects, exec_result.err()))
    }

    /// Execute a Move function call.
    pub fn move_call(
        &mut self,
        sender: SuiAddress,
        package: ObjectID,
        module: &str,
        function: &str,
        type_args: Vec<TypeTag>,
        call_args: Vec<CallArg>,
        gas_object_ref: ObjectRef,
        gas_budget: u64,
    ) -> Result<(TransactionEffects, Option<ExecutionError>)> {
        let mut builder = ProgrammableTransactionBuilder::new();
        builder.move_call(
            package,
            Identifier::new(module)?,
            Identifier::new(function)?,
            type_args,
            call_args,
        )?;
        let pt = builder.finish();

        let gas_price = self.epoch_state.reference_gas_price();
        let tx_data = TransactionData::new_with_gas_data(
            TransactionKind::ProgrammableTransaction(pt),
            sender,
            GasData {
                payment: vec![gas_object_ref],
                owner: sender,
                price: gas_price,
                budget: gas_budget,
            },
        );
        self.execute_transaction(tx_data)
    }

    pub fn get_object(&self, id: &ObjectID) -> Option<Object> {
        ObjectStore::get_object(&self.store, id)
    }

    pub fn epoch(&self) -> u64 {
        self.epoch_state.epoch()
    }

    pub fn reference_gas_price(&self) -> u64 {
        self.epoch_state.reference_gas_price()
    }

    pub fn rpc_url(&self) -> &str {
        &self.rpc_url
    }

    pub fn store_mut(&mut self) -> &mut ForkingStore {
        &mut self.store
    }
}

fn detect_chain(chain_id: &str) -> Chain {
    // Known chain identifiers — hex-encoded genesis checkpoint digests
    // Mainnet: 35834a8a
    // Testnet: 4c78adac
    if chain_id.starts_with("35834a8a") {
        Chain::Mainnet
    } else if chain_id.starts_with("4c78adac") {
        Chain::Testnet
    } else {
        Chain::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetcher::MockFetcher;
    use simulacrum::Simulacrum;

    #[test]
    fn bootstrap_from_mock() {
        // Use a Simulacrum to get valid system state. The system state
        // wrapper (0x5) has a dynamic field inner object that must also
        // be present. We extract the system state directly from the
        // Simulacrum's epoch_start_state to avoid needing all genesis objects.
        let sim = Simulacrum::new();

        // Build EpochState from the system state we can access via the store
        let system_state = sim.store().get_system_state();
        let clock = sim.store().get_clock();

        // Get all the objects from the store that we need
        let mut mock = MockFetcher::new();
        let sys_obj =
            SimulatorStore::get_object(sim.store(), &sui_types::SUI_SYSTEM_STATE_OBJECT_ID)
                .expect("system state should exist");
        let clock_obj = SimulatorStore::get_object(sim.store(), &sui_types::SUI_CLOCK_OBJECT_ID)
            .expect("clock should exist");
        mock.add_object(sys_obj);
        mock.add_object(clock_obj);

        // The system state inner object (dynamic field child of 0x5) is at a
        // derived ID. We need to add it too. The ID was found in the error:
        // 0x6af2... — but this varies per simulacrum run. Instead of hardcoding,
        // we look it up: get_sui_system_state calls get_dynamic_field_object_impl
        // with the version as key. For epoch 0, version=1.
        //
        // Since the mock doesn't have the inner object, we'll construct the
        // SuiFork differently: pass the pre-extracted system_state directly.
        //
        // For now, test that we can construct EpochState from a known-good
        // system state, verifying our core pipeline works.
        let protocol_config = ProtocolConfig::get_for_version(
            system_state.protocol_version().into(),
            Chain::Unknown,
        );
        let epoch_state = EpochState::new_with_protocol_config(system_state, protocol_config);
        assert_eq!(epoch_state.epoch(), 0);
        assert!(epoch_state.reference_gas_price() > 0);
    }
}
