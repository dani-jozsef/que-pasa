use anyhow::{anyhow, ensure, Context, Result};
use postgres::Transaction;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt;
use std::io;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::thread;
use thiserror::Error;

#[cfg(test)]
use pretty_assertions::assert_eq;

use crate::config::ContractID;
use crate::debug;
use crate::octez::bcd;
use crate::octez::block::{
    get_implicit_origination_level, Block, LevelMeta, TxContext,
};
use crate::octez::block_getter::ConcurrentBlockGetter;
use crate::octez::node::NodeClient;
use crate::relational::RelationalAST;
use crate::sql::db::DBClient;
use crate::sql::insert::{Insert, Inserts};
use crate::storage_structure::relational;
use crate::storage_structure::typing;
use crate::storage_update::bigmap::IntraBlockBigmapDiffsProcessor;
use crate::storage_update::processor::StorageProcessor;

pub struct SaveLevelResult {
    pub level: u32,
    pub contract_id: ContractID,
    pub is_origination: bool,
    pub tx_count: u32,
}

pub struct Executor {
    node_cli: NodeClient,
    dbcli: DBClient,

    contracts: HashMap<ContractID, (RelationalAST, Option<u32>)>,
    all_contracts: bool,

    // Everything below this level has nothing to do with what we are indexing
    level_floor: LevelFloor,

    db_url: String,
    db_ssl: bool,
    db_ca_cert: Option<String>,
}

#[derive(Clone)]
struct LevelFloor {
    f: Arc<Mutex<u32>>,
}

impl LevelFloor {
    pub fn set(&self, floor: u32) -> Result<()> {
        let mut level_floor = self
            .f
            .lock()
            .map_err(|_| anyhow!("failed to lock level_floor mutex"))?;
        *level_floor = floor;
        Ok(())
    }

    pub fn get(&self) -> Result<u32> {
        let level_floor = self
            .f
            .lock()
            .map_err(|_| anyhow!("failed to lock level_floor mutex"))?;
        Ok(*level_floor)
    }
}

#[derive(Error, Debug)]
pub struct BadLevelHash {
    level: u32,
    err: anyhow::Error,
}

impl fmt::Display for BadLevelHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.err)
    }
}

impl Executor {
    pub fn new(
        node_cli: NodeClient,
        dbcli: DBClient,
        db_url: &str,
        db_ssl: bool,
        db_ca_cert: Option<String>,
    ) -> Self {
        Self {
            node_cli,
            dbcli,
            contracts: HashMap::new(),
            all_contracts: false,
            level_floor: LevelFloor {
                f: Arc::new(Mutex::new(0)),
            },
            db_url: db_url.to_string(),
            db_ssl,
            db_ca_cert,
        }
    }

    fn update_level_floor(&mut self) -> Result<()> {
        if self.all_contracts {
            return Ok(());
        }

        let mut floor = 0;
        if !self.all_contracts {
            floor = self
                .contracts
                .values()
                .map(|(_, lfloor)| lfloor.unwrap_or(0_u32))
                .min()
                .unwrap_or(0);
        }

        self.level_floor.set(floor)
    }

    pub fn index_all_contracts(&mut self) {
        self.all_contracts = true
    }

    pub fn add_contract(&mut self, contract_id: &ContractID) -> Result<()> {
        debug!(
            "getting the storage definition for contract={}..",
            contract_id.name
        );
        let rel_ast = get_rel_ast(&mut self.node_cli, &contract_id.address)?;
        debug!("rel_ast: {:#?}", rel_ast);
        let contract_floor = self
            .dbcli
            .get_origination(contract_id)?;
        self.contracts
            .insert(contract_id.clone(), (rel_ast, contract_floor));
        Ok(())
    }

    pub fn add_missing_contracts(
        &mut self,
        contracts: &[&ContractID],
    ) -> Result<()> {
        for contract_id in contracts {
            self.add_contract(contract_id)?;
            self.dbcli.create_contract_schema(
                contract_id,
                &self.contracts[contract_id].0,
            )?;
        }
        Ok(())
    }

    pub fn create_contract_schemas(&mut self) -> Result<Vec<ContractID>> {
        let mut new_contracts: Vec<ContractID> = vec![];
        for (contract_id, rel_ast) in &self.contracts {
            if self
                .dbcli
                .create_contract_schema(contract_id, &rel_ast.0)?
            {
                new_contracts.push(contract_id.clone());
            }
        }
        Ok(new_contracts)
    }

    pub fn get_contract_rel_ast(
        &self,
        contract_id: &ContractID,
    ) -> Option<&RelationalAST> {
        self.contracts
            .get(contract_id)
            .map(|x| &x.0)
    }

    pub fn get_config(&self) -> Vec<ContractID> {
        self.contracts
            .keys()
            .cloned()
            .collect::<Vec<ContractID>>()
    }

    pub fn add_dependency_contracts(&mut self) -> Result<()> {
        let config = self.get_config();
        let deps = self
            .dbcli
            .get_config_deps(&config)
            .unwrap();

        for addr in deps {
            self.add_contract(&ContractID {
                name: addr.clone(),
                address: addr,
            })?;
        }

        Ok(())
    }

    pub fn exec_dependents(&mut self) -> Result<()> {
        let config: Vec<&ContractID> = self.contracts.keys().collect();
        let mut levels = self
            .dbcli
            .get_dependent_levels(&config)?;
        if levels.is_empty() {
            return Ok(());
        }
        levels.sort_unstable();

        info!("reprocessing following levels, they have bigmap copies whose keys are now fully known: {:?}", levels);
        self.exec_levels(1, levels)
    }

    pub fn exec_continuous(&mut self) -> Result<()> {
        // Executes blocks monotically, from old to new, continues from the heighest block present
        // in the db

        fn wait(first_wait: &mut bool) {
            if *first_wait {
                print!("waiting for the next block");
            } else {
                print!(".");
            }
            *first_wait = false;
            io::stdout().flush().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(1000));
        }
        fn wait_done(first_wait: &mut bool) {
            if !*first_wait {
                println!();
                *first_wait = false;
            }
        }
        let mut first_wait = true;
        loop {
            let chain_head = self.node_cli.head()?;
            let db_head = match self.dbcli.get_head()? {
                Some(head) => Ok(head),
                None => {
                    if self.all_contracts {
                        Self::print_status(
                            chain_head.level,
                            &self.exec_level(chain_head.level, false, true)?,
                        );
                        continue;
                    }
                    Err(anyhow!(
                        "cannot run in continuous mode: DB is empty, expected at least 1 block present to continue from"
                    ))
                }
            }?;
            debug!("db: {} chain: {}", db_head.level, chain_head.level);
            match chain_head.level.cmp(&db_head.level) {
                Ordering::Greater => {
                    wait_done(&mut first_wait);
                    for level in (db_head.level + 1)..=chain_head.level {
                        match self.exec_level(level, false, true) {
                            Ok(res) => Self::print_status(level, &res),
                            Err(e) => {
                                if !e.is::<BadLevelHash>() {
                                    return Err(e);
                                }
                                let bad_lvl = e.downcast::<BadLevelHash>()?;
                                warn!(
                                    "{}, deleting level {} from database",
                                    bad_lvl.err, bad_lvl.level
                                );
                                let mut tx = self.dbcli.transaction()?;
                                DBClient::delete_level(&mut tx, bad_lvl.level)?;
                                tx.commit()?;
                                self.repopulate_derived_tables()?;
                                break;
                            }
                        }
                    }
                    first_wait = true;
                    continue;
                }
                Ordering::Less => {
                    wait(&mut first_wait);
                    continue;
                }
                Ordering::Equal => {
                    // they are equal, so we will just check that the hashes match.
                    if db_head.hash != chain_head.hash {
                        wait_done(&mut first_wait);
                        warn!(
                            "Hashes don't match: {:?} (db) <> {:?} (chain)",
                            db_head.hash, chain_head.hash
                        );
                        let mut tx = self.dbcli.transaction()?;
                        DBClient::delete_level(&mut tx, db_head.level)?;
                        tx.commit()?;
                        self.repopulate_derived_tables()?;
                    }
                    wait(&mut first_wait);
                }
            }
        }
    }

    pub fn exec_levels(
        &mut self,
        num_getters: usize,
        levels: Vec<u32>,
    ) -> Result<()> {
        let level_floor = self.level_floor.clone();
        let have_floor = !self.all_contracts;
        self.exec_parallel(num_getters, move |height_chan| {
            for l in levels {
                if have_floor && l < level_floor.get().unwrap() {
                    continue;
                }
                height_chan.send(l).unwrap();
            }
        })?;
        Ok(())
    }

    pub fn exec_new_contracts_historically(
        &mut self,
        bcd_settings: Option<(String, String)>,
        num_getters: usize,
    ) -> Result<Vec<ContractID>> {
        let mut res: Vec<ContractID> = vec![];
        loop {
            self.add_dependency_contracts().unwrap();
            let new_contracts = self.create_contract_schemas().unwrap();

            if new_contracts.is_empty() {
                break;
            }
            res.extend(new_contracts.clone());

            info!(
                "initializing the following contracts historically: {:#?}",
                new_contracts
            );

            if let Some((bcd_url, network)) = &bcd_settings {
                let mut exclude_levels: Vec<u32> = vec![];
                for contract_id in &new_contracts {
                    info!("Initializing contract {}..", contract_id.name);
                    let bcd_cli = bcd::BCDClient::new(
                        bcd_url.clone(),
                        network.clone(),
                        contract_id.address.clone(),
                    );

                    let excl = exclude_levels.clone();
                    let processed_levels = self
                        .exec_parallel(num_getters, move |height_chan| {
                            bcd_cli
                                .populate_levels_chan(height_chan, &excl)
                                .unwrap()
                        })
                        .unwrap();
                    exclude_levels.extend(processed_levels);

                    if let Some(l) =
                        get_implicit_origination_level(&contract_id.address)
                    {
                        self.exec_level(l, true, false).unwrap();
                    }

                    self.fill_in_levels(contract_id)
                        .unwrap();

                    info!("contract {} initialized.", contract_id.name)
                }
            } else {
                self.exec_missing_levels(num_getters)
                    .unwrap();
            }
        }
        if !res.is_empty() {
            self.exec_dependents().unwrap();
        }
        Ok(res)
    }

    pub fn exec_missing_levels(&mut self, num_getters: usize) -> Result<()> {
        loop {
            let latest_level = self.node_cli.head()?.level;

            let missing_levels: Vec<u32> = self.dbcli.get_missing_levels(
                self.contracts
                    .keys()
                    .cloned()
                    .collect::<Vec<ContractID>>()
                    .as_slice(),
                latest_level,
            )?;
            if missing_levels.is_empty() {
                self.exec_dependents()?;
                return Ok(());
            }

            self.exec_levels(num_getters, missing_levels)?;
        }
    }

    pub fn exec_parallel<F>(
        &mut self,
        num_getters: usize,
        levels_selector: F,
    ) -> Result<Vec<u32>>
    where
        F: FnOnce(flume::Sender<u32>) + Send + 'static,
    {
        // Fetches block data in parallel, processes each block sequentially

        let (height_send, height_recv) = flume::bounded::<u32>(num_getters);
        let (block_send, block_recv) =
            flume::bounded::<Box<(LevelMeta, Block)>>(num_getters);

        let block_getter =
            ConcurrentBlockGetter::new(self.node_cli.clone(), num_getters);
        let mut threads = block_getter.run(height_recv, block_send);

        threads.push(thread::spawn(|| levels_selector(height_send)));

        let processed_levels: Vec<u32> = self.read_block_chan(block_recv)?;

        for t in threads {
            t.join().map_err(|e| {
                anyhow!("parallel execution thread failed with err: {:?}", e)
            })?;
        }
        self.repopulate_derived_tables()?;
        Ok(processed_levels)
    }

    fn repopulate_derived_tables(&mut self) -> Result<()> {
        info!("re-populating derived tables (_live, _ordered)");
        for (contract_id, (rel_ast, _)) in &self.contracts {
            self.dbcli
                .repopulate_derived_tables(contract_id, rel_ast)?;
        }
        Ok(())
    }

    pub fn fill_in_levels(&mut self, contract_id: &ContractID) -> Result<()> {
        // fills in all levels in db as empty that are missing between min and max
        // level present

        self.dbcli.fill_in_levels(contract_id)
            .with_context(|| {
                "failed to mark levels unrelated to the contract as empty in the db"
            })?;
        Ok(())
    }

    fn read_block_chan(
        &mut self,
        block_recv: flume::Receiver<Box<(LevelMeta, Block)>>,
    ) -> Result<Vec<u32>> {
        let mut processed_levels: Vec<u32> = vec![];
        for b in block_recv {
            let (meta, block) = *b;
            Self::print_status(
                meta.level,
                &self.exec_for_block(&meta, &block, true, false)?,
            );
            processed_levels.push(meta.level);
        }

        Ok(processed_levels)
    }

    fn print_status(level: u32, contract_results: &[SaveLevelResult]) {
        let mut contract_statuses: String = contract_results
            .iter()
            .filter_map(|c| match c.tx_count {
                0 => None,
                1 => Some(format!(
                    "\n\t1 contract call for {}",
                    c.contract_id.name
                )),
                _ => Some(format!(
                    "\n\t{} contract calls for {}",
                    c.tx_count, c.contract_id.name
                )),
            })
            .collect::<Vec<String>>()
            .join(",");
        if contract_statuses.is_empty() {
            contract_statuses = "0 txs for us".to_string();
        }
        info!("level {}: {}", level, contract_statuses);
    }

    fn get_storage_processor(
        &mut self,
    ) -> Result<StorageProcessor<NodeClient, DBClient>> {
        let id = self
            .dbcli
            .get_max_id()
            .with_context(|| {
                "could not initialize storage processor from the db state"
            })?;
        Ok(StorageProcessor::new(
            id,
            self.node_cli.clone(),
            DBClient::connect(
                &self.db_url,
                self.db_ssl,
                self.db_ca_cert.clone(),
            )?,
        ))
    }

    pub(crate) fn exec_level(
        &mut self,
        level_height: u32,
        cleanup_on_reorg: bool,
        update_derived_tables: bool,
    ) -> Result<Vec<SaveLevelResult>> {
        let (_json, meta, block) = self
            .node_cli
            .level_json(level_height)
            .with_context(|| {
                format!(
                    "execute for level={} failed: could not get block json",
                    level_height
                )
            })?;

        self.exec_for_block(
            &meta,
            &block,
            cleanup_on_reorg,
            update_derived_tables,
        )
    }

    fn ensure_level_hash(
        &mut self,
        level: u32,
        hash: &str,
        prev_hash: &str,
    ) -> Result<()> {
        let prev = self.dbcli.get_level(level - 1)?;
        if let Some(db_prev_hash) = prev
            .as_ref()
            .map(|l| l.hash.as_ref())
            .flatten()
        {
            ensure!(
                db_prev_hash == prev_hash, BadLevelHash{
                    level: prev.as_ref().unwrap().level,
                    err: anyhow!(
                        "level {} has different predecessor hash ({}) than previous recorded level's hash ({}) in db",
                        level, prev_hash, db_prev_hash),
                }
            );
        }

        let next = self.dbcli.get_level(level + 1)?;
        if let Some(db_next_prev_hash) = next
            .as_ref()
            .map(|l| l.prev_hash.as_ref())
            .flatten()
        {
            ensure!(
                db_next_prev_hash == hash,
                BadLevelHash{
                    level: next.as_ref().unwrap().level,
                    err: anyhow!(
                        "level {} has different hash ({}) than next recorded level's predecessor hash ({}) in db",
                        level, hash, db_next_prev_hash),
                }
            );
        }

        Ok(())
    }

    fn exec_for_block(
        &mut self,
        level: &LevelMeta,
        block: &Block,
        cleanup_on_reorg: bool,
        update_derived_tables: bool,
    ) -> Result<Vec<SaveLevelResult>> {
        // note: we expect level's values to all be set (no None values in its fields)
        if let Err(e) = self.ensure_level_hash(
            level.level,
            level.hash.as_ref().unwrap(),
            level.prev_hash.as_ref().unwrap(),
        ) {
            if !cleanup_on_reorg || !e.is::<BadLevelHash>() {
                return Err(e);
            }
            let bad_lvl = e.downcast::<BadLevelHash>()?;
            warn!("{}, reprocessing level {}", bad_lvl.err, bad_lvl.level);
            self.exec_level(bad_lvl.level, true, update_derived_tables)?;
        }

        let process_contracts = if self.all_contracts {
            let active_contracts: Vec<ContractID> = block
                .active_contracts()
                .iter()
                .map(|address| ContractID {
                    name: address.clone(),
                    address: address.clone(),
                })
                .collect();
            let new_contracts: Vec<&ContractID> = active_contracts
                .iter()
                .filter(|contract_id| !self.contracts.contains_key(contract_id))
                .collect();

            if !new_contracts.is_empty() {
                info!(
                    "level {}, analyzing contracts: {:#?}..",
                    block.header.level,
                    new_contracts
                        .iter()
                        .map(|x| &x.address)
                        .collect::<Vec<&String>>()
                );
                self.add_missing_contracts(&new_contracts)?;
            }
            active_contracts
        } else {
            self.contracts
                .keys()
                .cloned()
                .collect::<Vec<ContractID>>()
        };
        let mut contract_results: Vec<SaveLevelResult> = vec![];

        info!(
            "processing level {}: (baked at {})",
            level.level,
            level.baked_at.unwrap()
        );

        let mut storage_processor = self.get_storage_processor()?;
        let mut tx = self.dbcli.transaction()?;
        DBClient::delete_level(&mut tx, level.level)?;
        DBClient::save_level(&mut tx, level)?;

        let diffs = IntraBlockBigmapDiffsProcessor::from_block(block);
        for contract_id in &process_contracts {
            let (rel_ast, _) = &self.contracts[contract_id];
            contract_results.push(Self::exec_for_block_contract(
                &mut tx,
                level,
                block,
                &diffs,
                &mut storage_processor,
                contract_id,
                rel_ast,
                update_derived_tables,
            )?);
        }
        tx.commit()?;
        for cres in &contract_results {
            if cres.is_origination {
                self.update_contract_floor(&cres.contract_id, cres.level)?;
            }
        }
        Ok(contract_results)
    }

    fn update_contract_floor(
        &mut self,
        contract_id: &ContractID,
        level: u32,
    ) -> Result<()> {
        let (rel_ast, _) = self
            .contracts
            .get(contract_id)
            .unwrap()
            .clone();
        self.contracts
            .insert(contract_id.clone(), (rel_ast, Some(level)));
        self.update_level_floor()
    }

    fn exec_for_block_contract(
        tx: &mut Transaction,
        meta: &LevelMeta,
        block: &Block,
        diffs: &IntraBlockBigmapDiffsProcessor,
        storage_processor: &mut StorageProcessor<NodeClient, DBClient>,
        contract_id: &ContractID,
        rel_ast: &RelationalAST,
        update_derived_tables: bool,
    ) -> Result<SaveLevelResult> {
        let is_origination =
            block.has_contract_origination(&contract_id.address);

        if !is_origination && !block.is_contract_active(&contract_id.address) {
            Self::mark_level_empty(tx, meta, contract_id)
            .with_context(|| {
                format!(
                    "execute failed (level={}, contract={}): could not mark level as empty in db",
                    meta.level, contract_id.name)
            })?;

            return Ok(SaveLevelResult {
                level: meta.level,
                contract_id: contract_id.clone(),
                is_origination: false,
                tx_count: 0,
            });
        }

        let (inserts, tx_contexts, bigmap_contract_deps) = storage_processor
                .process_block(block, diffs, &contract_id.address, rel_ast)
                .with_context(|| {
                    format!(
                        "execute failed (level={}, contract={}): could not process block",
                        meta.level, contract_id.name,
                    )
                })?;
        let tx_count = tx_contexts.len() as u32;

        Self::save_level_processed_contract(
            tx,
            meta,
            contract_id,
            inserts,
            &tx_contexts,
            bigmap_contract_deps,
            storage_processor.get_bigmap_keyhashes(),
        )
        .with_context(|| {
            format!(
                "execute failed (level={}, contract={}): could not save processed block",
                meta.level, contract_id.name,
            )
        })?;
        if update_derived_tables {
            DBClient::update_derived_tables(
                tx,
                contract_id,
                rel_ast,
                &tx_contexts,
            )?;
        }
        DBClient::set_max_id(tx, storage_processor.get_id_value() + 1)?;

        if is_origination {
            Self::mark_level_contract_origination(tx, meta, contract_id)
            .with_context(|| {
                format!(
                    "execute for level={} failed: could not mark level as contract origination in db",
                    meta.level)
            })?;
        }
        Ok(SaveLevelResult {
            level: meta.level,
            contract_id: contract_id.clone(),
            is_origination,
            tx_count,
        })
    }

    fn mark_level_contract_origination(
        tx: &mut Transaction,
        meta: &LevelMeta,
        contract_id: &ContractID,
    ) -> Result<()> {
        DBClient::set_origination(tx, contract_id, meta.level)?;
        Ok(())
    }

    fn mark_level_empty(
        tx: &mut Transaction,
        meta: &LevelMeta,
        contract_id: &ContractID,
    ) -> Result<()> {
        DBClient::delete_contract_level(tx, contract_id, meta.level)?;
        DBClient::save_contract_level(tx, contract_id, meta.level)?;
        Ok(())
    }

    fn save_level_processed_contract(
        tx: &mut Transaction,
        meta: &LevelMeta,
        contract_id: &ContractID,
        inserts: Inserts,
        tx_contexts: &[TxContext],
        bigmap_contract_deps: Vec<String>,
        bigmap_keyhashes: Vec<(TxContext, i32, String, String)>,
    ) -> Result<()> {
        DBClient::delete_contract_level(tx, contract_id, meta.level)?;
        DBClient::save_contract_level(tx, contract_id, meta.level)?;

        DBClient::save_tx_contexts(tx, tx_contexts)?;
        DBClient::apply_inserts(
            tx,
            contract_id,
            &inserts
                .into_values()
                .collect::<Vec<Insert>>(),
        )?;
        DBClient::save_contract_deps(
            tx,
            meta.level,
            contract_id,
            bigmap_contract_deps,
        )?;
        DBClient::save_bigmap_keyhashes(tx, bigmap_keyhashes)?;
        Ok(())
    }
}

pub(crate) fn get_rel_ast(
    node_cli: &mut NodeClient,
    contract_address: &str,
) -> Result<RelationalAST> {
    let storage_def =
        &node_cli.get_contract_storage_definition(contract_address, None)?;
    debug!("storage_def: {:#?}", storage_def);
    let type_ast =
        typing::storage_ast_from_json(storage_def).with_context(|| {
            "failed to derive a storage type from the storage definition"
        })?;
    debug!("storage definition retrieved, and type derived");
    debug!("type_ast: {:#?}", type_ast);

    debug!(
        "storage_def: {}, type_ast: {}",
        debug::pp_depth(6, &storage_def),
        debug::pp_depth(6, &type_ast),
    );

    // Build the internal representation from the storage defition
    let ctx = relational::Context::init();
    let rel_ast = relational::ASTBuilder::new()
        .build_relational_ast(&ctx, &type_ast)
        .with_context(|| {
            "failed to build a relational AST from the storage type"
        })?;
    debug!("rel_ast: {:#?}", rel_ast);
    Ok(rel_ast)
}

#[test]
fn test_generate() {
    use crate::sql::postgresql_generator::PostgresqlGenerator;
    use crate::storage_structure::relational::ASTBuilder;
    use crate::storage_structure::typing;

    use ron::ser::{to_string_pretty, PrettyConfig};
    use std::fs::File;
    use std::io::BufReader;
    use std::path::Path;
    let json = json::parse(&debug::load_test(
        "test/KT1U7Adyu5A7JWvEVSKjJEkG2He2SU1nATfq.script",
    ))
    .unwrap();
    let storage_definition = &json["code"][1]["args"][0];
    let type_ast =
        typing::storage_ast_from_json(&storage_definition.clone()).unwrap();
    println!("{:#?}", type_ast);

    use crate::relational::Context;
    let context = Context::init();

    let rel_ast = ASTBuilder::new()
        .build_relational_ast(&context, &type_ast)
        .unwrap();
    println!("{:#?}", rel_ast);
    let generator = PostgresqlGenerator::new(&ContractID {
        name: "testcontract".to_string(),
        address: "".to_string(),
    });
    let mut builder = crate::sql::table_builder::TableBuilder::new();
    builder.populate(&rel_ast);
    let mut sorted_tables: Vec<_> = builder.tables.iter().collect();
    sorted_tables.sort_by_key(|a| a.0);
    let mut tables: Vec<crate::sql::table::Table> = vec![];
    for (_name, table) in sorted_tables {
        print!(
            "{}",
            generator
                .create_table_definition(table)
                .unwrap()
        );
        tables.push(table.clone());
        println!();
    }

    let filename = "test/KT1U7Adyu5A7JWvEVSKjJEkG2He2SU1nATfq.tables.json";
    println!("cat > {} <<ENDOFJSON", filename);
    println!(
        "{}",
        to_string_pretty(&tables, PrettyConfig::new()).unwrap()
    );
    println!(
        "ENDOFJSON
    "
    );

    let p = Path::new(filename);
    let file = File::open(p).unwrap();
    let reader = BufReader::new(file);
    let v: Vec<crate::sql::table::Table> =
        ron::de::from_reader(reader).unwrap();
    assert_eq!(v.len(), tables.len());
    //test doesn't verify view exist
    for i in 0..v.len() {
        assert_eq!(v[i], tables[i]);
    }
}

#[test]
fn test_get_origination_operations_from_block() {
    use crate::octez::block::Block;
    let test_file =
        "test/KT1U7Adyu5A7JWvEVSKjJEkG2He2SU1nATfq.level-132091.json";
    let contract_id = "KT1U7Adyu5A7JWvEVSKjJEkG2He2SU1nATfq";
    let block: Block =
        serde_json::from_str(&debug::load_test(test_file)).unwrap();
    assert!(block.has_contract_origination(&contract_id));

    for level in vec![
        132343, 123318, 123327, 123339, 128201, 132201, 132211, 132219, 132222,
        132240, 132242, 132259, 132262, 132278, 132282, 132285, 132298, 132300,
        132343, 132367, 132383, 132384, 132388, 132390, 135501, 138208, 149127,
    ] {
        let filename = format!(
            "test/KT1U7Adyu5A7JWvEVSKjJEkG2He2SU1nATfq.level-{}.json",
            level
        );
        println!("testing {}", filename);
        let level_block: Block =
            serde_json::from_str(&debug::load_test(&filename)).unwrap();

        assert!(!level_block.has_contract_origination(&contract_id));
    }
}

#[test]
fn test_storage() {}
