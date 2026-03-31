// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::str::FromStr;

use anyhow::Result;
use clap::{Parser, Subcommand};
use sui_fork::SuiFork;
use sui_types::base_types::{ObjectID, SuiAddress};
use sui_types::effects::TransactionEffectsAPI;

#[derive(Parser)]
#[command(
    name = "sui-fork",
    about = "Fork Sui mainnet and execute transactions locally"
)]
struct Cli {
    #[arg(long, default_value = "https://fullnode.mainnet.sui.io:443")]
    rpc_url: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Show fork info (epoch, gas price)
    Info,
    /// Read an object from forked state
    GetObject {
        #[arg(long)]
        id: String,
    },
    /// Execute a Move call
    Call {
        #[arg(long)]
        sender: String,
        #[arg(long)]
        package: String,
        #[arg(long)]
        module: String,
        #[arg(long)]
        function: String,
        #[arg(long)]
        gas: String,
        #[arg(long, default_value = "10000000")]
        gas_budget: u64,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    eprintln!("Forking from {}...", cli.rpc_url);
    let mut fork = SuiFork::new(&cli.rpc_url)?;
    eprintln!(
        "Fork ready. Epoch: {}, Gas price: {}",
        fork.epoch(),
        fork.reference_gas_price()
    );

    match cli.command {
        Commands::Info => {
            println!("Epoch: {}", fork.epoch());
            println!("Reference gas price: {}", fork.reference_gas_price());
        }
        Commands::GetObject { id } => {
            let oid = ObjectID::from_hex_literal(&id)?;
            match fork.get_object(&oid) {
                Some(obj) => {
                    println!("Object: {}", obj.id());
                    println!("Version: {}", obj.version().value());
                    println!("Owner: {:?}", obj.owner);
                    println!("Type: {:?}", obj.type_());
                }
                None => println!("Object not found: {id}"),
            }
        }
        Commands::Call {
            sender,
            package,
            module,
            function,
            gas,
            gas_budget,
        } => {
            let sender = SuiAddress::from_str(&sender)?;
            let package_id = ObjectID::from_hex_literal(&package)?;
            let gas_id = ObjectID::from_hex_literal(&gas)?;
            let gas_obj = fork
                .get_object(&gas_id)
                .ok_or_else(|| anyhow::anyhow!("Gas object {gas} not found"))?;
            let gas_ref = gas_obj.compute_object_reference();

            eprintln!("Executing {package}::{module}::{function} as {sender}...");
            let (effects, exec_err) = fork.move_call(
                sender,
                package_id,
                &module,
                &function,
                vec![],
                vec![],
                gas_ref,
                gas_budget,
            )?;

            println!("Status: {:?}", effects.status());
            if let Some(err) = exec_err {
                println!("Error: {err:?}");
            }
            println!("Gas used: {:?}", effects.gas_cost_summary());
            for (oref, owner) in effects.created() {
                println!("  + {} (owner: {owner:?})", oref.0);
            }
        }
    }
    Ok(())
}
