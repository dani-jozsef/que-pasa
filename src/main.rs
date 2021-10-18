extern crate itertools;
#[macro_use]
extern crate json;
#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate log;
#[macro_use]
extern crate serde;

pub mod config;
pub mod contract_denylist;
pub mod debug;
pub mod highlevel;
pub mod octez;
pub mod sql;
pub mod storage_structure;
pub mod storage_update;
pub mod storage_value;

use anyhow::Context;
use chrono::Duration;
use config::CONFIG;
use env_logger::Env;
use octez::node;
use sql::db::DBClient;
use std::collections::HashMap;
use std::panic;
use std::process;
use std::thread;

use config::ContractID;
use contract_denylist::is_contract_denylisted;
use storage_structure::relational;

fn main() {
    let orig_hook = panic::take_hook();
    panic::set_hook(Box::new(move |panic_info| {
        // invoke the default handler and exit the process
        orig_hook(panic_info);
        // wait for a bit to give time to the root error's thread to print
        // its error
        thread::sleep(std::time::Duration::from_millis(500));
        process::exit(1);
    }));

    dotenv::dotenv().ok();
    let env = Env::default().filter_or("RUST_LOG", "info");
    env_logger::init_from_env(env);

    let config = CONFIG.as_ref().unwrap();

    let node_cli =
        &node::NodeClient::new(config.node_url.clone(), "main".to_string());

    let mut dbcli = DBClient::connect(
        &config.database_url,
        config.ssl,
        config.ca_cert.clone(),
    )
    .with_context(|| "failed to connect to the db")
    .unwrap();

    let setup_db = config.reinit || !dbcli.common_tables_exist().unwrap();
    if config.reinit {
        println!(
"Re-initializing -- all data in DB related to ever set-up contracts, including those set-up in prior runs (!), will be destroyed. \
Interrupt within 15 seconds to abort"
);
        thread::sleep(std::time::Duration::from_millis(15000));
        dbcli
            .delete_everything(&mut node_cli.clone(), highlevel::get_rel_ast)
            .with_context(|| "failed to delete the db's content")
            .unwrap();
    }
    if setup_db {
        dbcli.create_common_tables().unwrap();
        info!("Common tables set up in db");
    }

    let mut executor = highlevel::Executor::new(
        node_cli.clone(),
        dbcli,
        &config.database_url,
        config.ssl,
        config.ca_cert.clone(),
    );
    #[cfg(feature = "regression")]
    if config.always_update_derived {
        executor.always_update_derived_tables();
    }
    if config.all_contracts {
        index_all_contracts(config, executor);
        return;
    }

    for contract_id in &config.contracts {
        executor
            .add_contract(contract_id)
            .unwrap();
    }
    let contracts = executor.get_config();
    assert_contracts_ok(&contracts);

    let num_getters = config.workers_cap;
    if !config.levels.is_empty() {
        executor
            .add_dependency_contracts()
            .unwrap();
        executor
            .create_contract_schemas()
            .unwrap();
        executor
            .exec_levels(num_getters, config.levels.clone())
            .unwrap();
        return;
    }

    // ensure we bootstrap until at least yesterday, from there it's acceptable
    // if continuous mode is running (setting an acceptable duration may be
    // necessary depending on how long it takes to derive the _ordered and _live
    // tables, unfortunately.
    let acceptable_head_offset = Duration::days(1);
    let new_initialized = executor
        .exec_new_contracts_historically(
            config
                .bcd_url
                .as_ref()
                .map(|url| (url.clone(), config.network.clone())),
            num_getters,
            acceptable_head_offset,
        )
        .unwrap();
    if !new_initialized.is_empty() {
        info!("all contracts historically bootstrapped. restart to begin normal continuous processing mode.");
        return;
    }

    info!("running for contracts: {:#?}", contracts);
    if !config.levels.is_empty() {
        executor
            .exec_levels(num_getters, config.levels.clone())
            .unwrap();
        executor.exec_dependents().unwrap();
        return;
    }

    // We will first load missing levels (if any)
    info!("processing missing levels");
    executor
        .exec_missing_levels(num_getters, acceptable_head_offset)
        .unwrap();

    // At last, normal operation.
    info!("processing blocks at the chain head");
    executor.exec_continuous().unwrap();
}

fn index_all_contracts(
    config: &config::Config,
    mut executor: highlevel::Executor,
) {
    executor.index_all_contracts();
    if !config.levels.is_empty() {
        executor
            .exec_levels(config.workers_cap, config.levels.clone())
            .unwrap();
    } else {
        info!("processing missing levels");
        executor
            .exec_missing_levels(config.workers_cap, Duration::days(0))
            .unwrap();

        info!("processing blocks at the chain head");
        executor.exec_continuous().unwrap();
    }
}

fn assert_contracts_ok(contracts: &[ContractID]) {
    if contracts.is_empty() {
        panic!("zero contracts to index..");
    }

    let mut names: HashMap<String, ()> = HashMap::new();
    for contract_id in contracts {
        if names.contains_key(&contract_id.name) {
            panic!("bad contract settings provided: name clash (multiple contracts assigned to name '{}'", contract_id.name);
        }
        if is_contract_denylisted(&contract_id.address) {
            panic!("bad contract settings provided: denylisted contract cannot be indexed ({})", contract_id.name);
        }
        names.insert(contract_id.name.clone(), ());
    }
}
