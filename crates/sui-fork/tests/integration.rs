// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::str::FromStr;

use sui_fork::SuiFork;
use sui_types::base_types::{ObjectID, SuiAddress};
use sui_types::effects::TransactionEffectsAPI;
use sui_types::object::Object;
use sui_types::transaction::{
    Argument, CallArg, Command, GasData, ObjectArg, TransactionData, TransactionKind,
};

const TESTNET_RPC: &str = "https://fullnode.testnet.sui.io:443";
const MAINNET_RPC: &str = "https://fullnode.mainnet.sui.io:443";

#[test]
#[ignore]
fn bootstrap_from_testnet() {
    let fork = SuiFork::new(TESTNET_RPC);
    assert!(fork.is_ok(), "Bootstrap failed: {:?}", fork.err());
    let fork = fork.unwrap();
    assert!(fork.epoch() > 0);
    assert!(fork.reference_gas_price() > 0);
    println!(
        "Testnet epoch: {}, gas price: {}",
        fork.epoch(),
        fork.reference_gas_price()
    );
}

#[test]
#[ignore]
fn read_sui_framework() {
    let fork = SuiFork::new(TESTNET_RPC).unwrap();
    let sui_framework = ObjectID::from_hex_literal("0x2").unwrap();
    assert!(
        fork.get_object(&sui_framework).is_some(),
        "Sui framework should exist"
    );
}

/// Helper: find a SUI gas coin for the given owner via RPC.
/// Fetch a SUI gas coin with at least `min_balance` MIST for the given owner.
fn fetch_gas_coin(rpc_url: &str, owner: SuiAddress, min_balance: u64) -> Object {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let client = sui_sdk::SuiClientBuilder::default()
            .build(rpc_url)
            .await
            .unwrap();
        // Fetch multiple coins and pick one with sufficient balance
        let coins = client
            .coin_read_api()
            .get_coins(owner, None, None, Some(50))
            .await
            .unwrap();
        let coin_data = coins
            .data
            .into_iter()
            .find(|c| c.balance >= min_balance)
            .unwrap_or_else(|| {
                panic!(
                    "No SUI coin with >= {min_balance} MIST found for {owner}"
                )
            });
        let obj_id = coin_data.coin_object_id;

        let response = client
            .read_api()
            .get_object_with_options(
                obj_id,
                sui_json_rpc_types::SuiObjectDataOptions::bcs_lossless(),
            )
            .await
            .unwrap();
        let data = response.object().expect("object should exist").clone();
        TryInto::<Object>::try_into(data).expect("object conversion should succeed")
    })
}

#[test]
#[ignore]
fn mainnet_sui_transfer() {
    use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;

    let mut fork = SuiFork::new(MAINNET_RPC).unwrap();
    println!(
        "Forked mainnet at epoch {}, gas price {}",
        fork.epoch(),
        fork.reference_gas_price()
    );

    // Find a SUI coin holder on mainnet with sufficient balance.
    // If this address runs dry, find any recent transaction sender on suiscan
    // and replace with their address.
    let sender = SuiAddress::from_str(
        "0x0ffa5dcd5154e6e44671d33d4f17df0b31b486f7f0f379e9fc3356bb39e91dba",
    )
    .unwrap();

    let gas_obj = fetch_gas_coin(MAINNET_RPC, sender, 10_000_000);
    println!("Gas coin: {}, version: {}", gas_obj.id(), gas_obj.version().value());

    // Seed the gas coin into the fork's store so it's available locally
    fork.store_mut().seed_object(gas_obj.clone());
    let gas_ref = gas_obj.compute_object_reference();

    // Build a SplitCoins + TransferObjects PTB
    let recipient = SuiAddress::random_for_testing_only();
    let amount: u64 = 1_000;

    let mut builder = ProgrammableTransactionBuilder::new();
    let amt_arg = builder.pure(amount).unwrap();
    let recipient_arg = builder.pure(recipient).unwrap();
    let coin_arg = builder.command(Command::SplitCoins(Argument::GasCoin, vec![amt_arg]));
    builder.command(Command::TransferObjects(vec![coin_arg], recipient_arg));
    let pt = builder.finish();

    let tx_data = TransactionData::new_with_gas_data(
        TransactionKind::ProgrammableTransaction(pt),
        sender,
        GasData {
            payment: vec![gas_ref],
            owner: sender,
            price: fork.reference_gas_price(),
            budget: 5_000_000,
        },
    );

    let (effects, exec_err) = fork
        .execute_transaction(tx_data)
        .expect("Transaction execution should not panic");

    println!("Status: {:?}", effects.status());
    println!("Gas used: {:?}", effects.gas_cost_summary());
    println!("Objects created: {}", effects.created().len());
    println!("Objects mutated: {}", effects.mutated().len());

    assert!(
        exec_err.is_none(),
        "Transfer should succeed, got: {:?}",
        exec_err
    );
    assert!(
        effects.status().is_ok(),
        "Effects status should be success, got: {:?}",
        effects.status()
    );
    assert!(
        !effects.created().is_empty(),
        "Should have created at least one object (the split coin)"
    );
}

/// Extract initial_shared_version from a shared object.
fn shared_version(obj: &Object) -> sui_types::base_types::SequenceNumber {
    use sui_types::object::Owner;
    match obj.owner {
        Owner::Shared {
            initial_shared_version,
        } => initial_shared_version,
        _ => panic!("Object {} is not shared, owner: {:?}", obj.id(), obj.owner),
    }
}

#[test]
#[ignore]
fn mainnet_cetus_swap() {
    use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
    use sui_types::transaction::ProgrammableMoveCall;

    let mut fork = SuiFork::new(MAINNET_RPC).unwrap();
    println!(
        "Forked mainnet at epoch {}, gas price {}",
        fork.epoch(),
        fork.reference_gas_price()
    );

    // Cetus CLMM integrate package (pool_script_v2::swap_b2a)
    // This is the latest version as of 2026-03. If it gets upgraded again,
    // find a recent swap on suiscan and use that package ID.
    let cetus_integrate = ObjectID::from_hex_literal(
        "0xb2db7142fa83210a7d78d9c12ac49c043b3cbbd482224fea6e3da00aa5a5ae2d",
    )
    .unwrap();

    // Cetus global config (shared)
    let cetus_global_config_id = ObjectID::from_hex_literal(
        "0xdaa46292632c3c4d8f31f23ea0f9b36a28ff3677e9684980e4438403a67a3d8f",
    )
    .unwrap();

    // Cetus wUSDC/SUI pool (shared)
    let cetus_pool_id = ObjectID::from_hex_literal(
        "0xcf994611fd4c48e277ce3ffd4d4364c914af2c3cbb05f7bf6facd371de688630",
    )
    .unwrap();

    // Token types
    let wusdc_type: sui_types::TypeTag = "0x5d4b302506645c37ff133b98c4b50a5ae14841659738d6d733d59d0d217a93bf::coin::COIN"
        .parse()
        .unwrap();
    let sui_type: sui_types::TypeTag = "0x2::sui::SUI".parse().unwrap();

    // Clock
    let clock_id = sui_types::SUI_CLOCK_OBJECT_ID;

    // Sender with sufficient balance
    let sender = SuiAddress::from_str(
        "0x0ffa5dcd5154e6e44671d33d4f17df0b31b486f7f0f379e9fc3356bb39e91dba",
    )
    .unwrap();

    let gas_obj = fetch_gas_coin(MAINNET_RPC, sender, 10_000_000);
    println!(
        "Gas coin: {}, version: {}",
        gas_obj.id(),
        gas_obj.version().value()
    );
    fork.store_mut().seed_object(gas_obj.clone());
    let gas_ref = gas_obj.compute_object_reference();

    // Fetch shared objects to get initial_shared_version
    let global_config_obj = fork
        .get_object(&cetus_global_config_id)
        .expect("Cetus global config should exist");
    let pool_obj = fork
        .get_object(&cetus_pool_id)
        .expect("Cetus wUSDC/SUI pool should exist");
    let clock_obj = fork.get_object(&clock_id).expect("Clock should exist");

    let config_version = shared_version(&global_config_obj);
    let pool_version = shared_version(&pool_obj);
    let clock_version = shared_version(&clock_obj);

    println!("Global config initial_shared_version: {}", config_version.value());
    println!("Pool initial_shared_version: {}", pool_version.value());
    println!("Clock initial_shared_version: {}", clock_version.value());

    // Build swap PTB (pool_script_v2::swap_b2a pattern):
    // 1. coin::zero<wUSDC>() — empty output coin
    // 2. SplitCoins(GasCoin, [amount]) — split SUI for swap input
    // 3. MoveCall(swap_b2a<wUSDC, SUI>(config, pool, zero_wusdc, sui_coin, ...))
    let swap_amount: u64 = 100_000;

    let mut builder = ProgrammableTransactionBuilder::new();

    // Shared object args
    let config_arg = builder
        .input(CallArg::Object(ObjectArg::SharedObject {
            id: cetus_global_config_id,
            initial_shared_version: config_version,
            mutability: sui_types::transaction::SharedObjectMutability::Immutable,
        }))
        .unwrap();

    let pool_arg = builder
        .input(CallArg::Object(ObjectArg::SharedObject {
            id: cetus_pool_id,
            initial_shared_version: pool_version,
            mutability: sui_types::transaction::SharedObjectMutability::Mutable,
        }))
        .unwrap();

    let clock_arg = builder
        .input(CallArg::Object(ObjectArg::SharedObject {
            id: clock_id,
            initial_shared_version: clock_version,
            mutability: sui_types::transaction::SharedObjectMutability::Immutable,
        }))
        .unwrap();

    // 1. Create zero wUSDC coin for output
    let zero_coin = builder.command(Command::MoveCall(Box::new(ProgrammableMoveCall {
        package: ObjectID::from_hex_literal("0x2").unwrap(),
        module: "coin".to_string(),
        function: "zero".to_string(),
        type_arguments: vec![wusdc_type.clone().into()],
        arguments: vec![],
    })));

    // 2. Split SUI from gas
    let amt_arg = builder.pure(swap_amount).unwrap();
    let split_coin = builder.command(Command::SplitCoins(Argument::GasCoin, vec![amt_arg]));

    // swap_b2a params
    let by_amount_in = builder.pure(true).unwrap();
    let amount = builder.pure(swap_amount).unwrap();
    let amount_limit = builder.pure(0u64).unwrap();
    let sqrt_price_limit = builder
        .pure(79226673515401279992447579055u128)
        .unwrap();

    // 3. Call swap_b2a<wUSDC, SUI>
    builder.command(Command::MoveCall(Box::new(ProgrammableMoveCall {
        package: cetus_integrate,
        module: "pool_script_v2".to_string(),
        function: "swap_b2a".to_string(),
        type_arguments: vec![wusdc_type.into(), sui_type.into()],
        arguments: vec![
            config_arg,
            pool_arg,
            zero_coin,
            split_coin,
            by_amount_in,
            amount,
            amount_limit,
            sqrt_price_limit,
            clock_arg,
        ],
    })));

    let pt = builder.finish();

    let tx_data = TransactionData::new_with_gas_data(
        TransactionKind::ProgrammableTransaction(pt),
        sender,
        GasData {
            payment: vec![gas_ref],
            owner: sender,
            price: fork.reference_gas_price(),
            budget: 50_000_000,
        },
    );

    let (effects, exec_err) = fork
        .execute_transaction(tx_data)
        .expect("Transaction execution should not panic");

    println!("Cetus swap status: {:?}", effects.status());
    if let Some(ref err) = exec_err {
        println!("Execution error: {err:?}");
    }
    println!("Gas used: {:?}", effects.gas_cost_summary());
    println!("Objects created: {}", effects.created().len());
    println!("Objects mutated: {}", effects.mutated().len());

    assert!(
        exec_err.is_none(),
        "Cetus swap should succeed, got: {:?}",
        exec_err
    );
    assert!(
        effects.status().is_ok(),
        "Effects status should be success, got: {:?}",
        effects.status()
    );
}
