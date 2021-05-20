use crate::error::Res;
use crate::michelson::StorageParser;
use crate::node::{Context, Node};
use crate::postgresql_generator;
use crate::postgresql_generator::PostgresqlGenerator;
use crate::storage;
use crate::table_builder;
use json::JsonValue;
use std::error::Error;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

pub fn get_node_from_script_json(json: &JsonValue) -> Res<Node> {
    let storage_definition = json["code"][1]["args"][0].clone();
    debug!("{}", storage_definition.to_string());
    let ast = storage::storage_from_json(storage_definition)?;
    let mut big_map_tables_names = Vec::new();
    let node = Node::build(Context::init(), ast, &mut big_map_tables_names);
    Ok(node)
}

pub fn get_tables_from_node(node: &Node) -> Result<table_builder::TableMap, Box<dyn Error>> {
    let mut builder = table_builder::TableBuilder::new();
    builder.populate(&node)?;
    Ok(builder.tables)
}

pub fn get_origination(_contract_id: &str) -> Res<Option<u32>> {
    postgresql_generator::get_origination(&mut postgresql_generator::connect()?)
}

pub struct SaveLevelResult {
    pub is_origination: bool,
    pub tx_count: u32,
}

pub fn load_and_store_level(node: &Node, contract_id: &str, level: u32) -> Res<SaveLevelResult> {
    let mut generator = PostgresqlGenerator::new();
    let id = postgresql_generator::get_max_id(&mut postgresql_generator::connect()?)? as u32;
    let mut storage_parser = StorageParser::new(id);
    let mut connection = postgresql_generator::connect()?;
    let mut transaction = postgresql_generator::transaction(&mut connection)?;
    let json = StorageParser::level_json(level)?;

    if StorageParser::block_has_contract_origination(&json, contract_id)? {
        debug!("Setting origination to true");
        postgresql_generator::delete_level(&mut transaction, &StorageParser::level(level)?)?;
        postgresql_generator::save_level(&mut transaction, &StorageParser::level(level)?)?;
        postgresql_generator::set_origination(&mut transaction, level)?;
        transaction.commit()?;
        return Ok(SaveLevelResult {
            is_origination: true,
            tx_count: 0,
        });
    }

    if !StorageParser::level_has_tx_for_us(&json, contract_id)? {
        postgresql_generator::delete_level(&mut transaction, &StorageParser::level(level)?)?;
        postgresql_generator::save_level(&mut transaction, &StorageParser::level(level)?)?;
        transaction.commit()?; // TODO: think about this
        return Ok(SaveLevelResult {
            is_origination: false,
            tx_count: 0,
        });
    }

    let json = storage_parser.get_storage(&contract_id.to_string(), level)?;
    let v = storage_parser.preparse_storage(&json);
    let result = storage_parser.parse_storage(&v)?;
    debug!("storage: {:#?}", result);
    let result = storage_parser.read_storage(&result, &node)?;
    debug!("{:#?}", result);

    let operations = StorageParser::get_operations_from_node(contract_id, Some(level))?;
    let tx_count = operations.len() as u32;
    //storageParser.store_big_map_list();
    let big_map_ops = StorageParser::get_big_map_operations_from_operations(&operations)?;
    for big_map_op in big_map_ops {
        storage_parser.process_big_map(&big_map_op)?;
    }
    let inserts = crate::table::insert::get_inserts();
    let mut keys = inserts
        .keys()
        .collect::<Vec<&crate::table::insert::InsertKey>>();
    keys.sort_by_key(|a| a.id);
    debug!("keys: {:?}", keys);
    postgresql_generator::delete_level(&mut transaction, &StorageParser::level(level)?)?;
    postgresql_generator::save_level(&mut transaction, &StorageParser::level(level)?)?;
    for key in keys.iter() {
        postgresql_generator::exec(
            &mut transaction,
            &generator.build_insert(
                inserts
                    .get(key)
                    .ok_or_else(|| crate::error::Error::boxed("No insert for key"))?,
                level,
            ),
        )?;
    }
    postgresql_generator::set_max_id(
        &mut transaction,
        storage_parser.id_generator.get_id() as i32,
    )?;
    transaction.commit()?;
    crate::table::insert::clear_inserts();
    Ok(SaveLevelResult {
        is_origination: false,
        tx_count,
    })
}

/// Load from the ../test directory, only for testing
#[allow(dead_code)]
fn load_test(name: &str) -> String {
    //println!("{}", name);
    std::fs::read_to_string(std::path::Path::new(name)).unwrap()
}

#[test]
fn test_generate() {
    let contract_id_array = vec![
        "KT1U7Adyu5A7JWvEVSKjJEkG2He2SU1nATfq",
        "KT1LYbgNsG2GYMfChaVCXunjECqY59UJRWBf",
        "KT1McJxUCT8qAybMfS6n5kjaESsi7cFbfck8",
    ];
    for contract_id in contract_id_array {
        let json = json::parse(&load_test(&format!("test/{}.script", contract_id))).unwrap();
        let storage_definition = &json["code"][1]["args"][0];
        let ast = crate::storage::storage_from_json(storage_definition.clone()).unwrap();
        println!("{:#?}", ast);
        let mut big_map_tables_names = Vec::new();
        let node = Node::build(Context::init(), ast, &mut big_map_tables_names);
        println!("{:#?}", node);
        let mut generator = crate::postgresql_generator::PostgresqlGenerator::new();
        let mut builder = table_builder::TableBuilder::new();
        builder.populate(&node).unwrap();
        let mut sorted_tables: Vec<_> = builder.tables.iter().collect();
        sorted_tables.sort_by_key(|a| a.0);
        let mut tables: Vec<crate::table::Table> = vec![];
        for (_name, table) in sorted_tables {
            print!("{}", generator.create_table_definition(table).unwrap());
            tables.push(table.clone());
            println!();
        }
        println!("{}", serde_json::to_string(&tables).unwrap());

        let file_path = format!("test/{}.tables.json", contract_id);
        let p = Path::new(&file_path);
        let file = File::open(p).unwrap();
        let reader = BufReader::new(file);
        let v: Vec<crate::table::Table> = serde_json::from_reader(reader).unwrap();
        assert_eq!(v.len(), tables.len());
        for i in 0..v.len() {
            assert_eq!(v[i], tables[i]);
        }
    }
}

#[test]
fn test_has_tx_for_us() {
    let pass_json = json::parse(&load_test(
        "test/KT1U7Adyu5A7JWvEVSKjJEkG2He2SU1nATfq.level-132240.json",
    ))
    .unwrap();
    assert_eq!(
        true,
        StorageParser::level_has_tx_for_us(&pass_json, "KT1U7Adyu5A7JWvEVSKjJEkG2He2SU1nATfq")
            .unwrap()
    );
    let fail_json = json::parse(&load_test(
        "test/KT1U7Adyu5A7JWvEVSKjJEkG2He2SU1nATfq.level-123327.json",
    ))
    .unwrap();
    assert_eq!(
        false,
        StorageParser::level_has_tx_for_us(&fail_json, "KT1U7Adyu5A7JWvEVSKjJEkG2He2SU1nATfq")
            .unwrap()
    );
}

#[test]
fn test_block() {
    // this tests the generated table structures against known good ones.
    // if it fails for a good reason, the output can be used to repopulate the
    // test files. To do this:
    // `cargo test -- --test test_block | bash`
    use ron::ser::{to_string_pretty, PrettyConfig};

    #[derive(Debug)]
    struct Contract<'a> {
        id: &'a str,
        levels: Vec<i32>,
        operation_count: usize,
    };

    let contracts: [Contract; 3] = [
        Contract {
            id: "KT1U7Adyu5A7JWvEVSKjJEkG2He2SU1nATfq",
            levels: vec![
                132343, 123318, 123327, 123339, 128201, 132091, 132201, 132211, 132219, 132222,
                132240, 132242, 132259, 132262, 132278, 132282, 132285, 132298, 132300, 132343,
                132367, 132383, 132384, 132388, 132390, 135501, 138208, 149127,
            ],
            operation_count: 16,
        },
        Contract {
            id: "KT1McJxUCT8qAybMfS6n5kjaESsi7cFbfck8",
            levels: vec![
                228459, 228460, 228461, 228462, 228463, 228464, 228465, 228466, 228467, 228468,
                228490, 228505, 228506, 228507, 228508, 228509, 228510, 228511, 228512, 228516,
                228521, 228522, 228523, 228524, 228525, 228526, 228527,
            ],
            operation_count: 28,
        },
        Contract {
            id: "KT1LYbgNsG2GYMfChaVCXunjECqY59UJRWBf",
            levels: vec![
                147806, 147807, 147808, 147809, 147810, 147811, 147812, 147813, 147814, 147815,
                147816,
            ],
            operation_count: 10,
        },
    ];

    for contract in &contracts {
        println!("contract {:?}", contract);
        let script_json = json::parse(&load_test(&format!("test/{}.script", contract.id))).unwrap();
        let node = get_node_from_script_json(&script_json).unwrap();
        let mut inserts_tested = 0;
        for level in &contract.levels {
            let level_json = json::parse(&load_test(&format!(
                "test/{}.level-{}.json",
                contract.id, level
            )))
            .unwrap();

            let operations = StorageParser::get_operations_from_block_json(
                &format!("{}", contract.id),
                &level_json,
            )
            .unwrap();
            for operation in &operations {
                if let JsonValue::Array(contents) = &operation["contents"] {
                    let operation = contents[0].clone();
                    if contents[0]["destination"].to_string().as_str() != contract.id {
                        println!("{} is not our contract_id", operation["destination"]);
                        continue;
                    }
                    let storage_json =
                        StorageParser::get_storage_from_operation(&operation).unwrap();

                    let mut storage_parser = StorageParser::new(1);

                    let preparsed_storage = storage_parser.preparse_storage(&storage_json);
                    let parsed_storage = storage_parser.parse_storage(&preparsed_storage).unwrap();
                    storage_parser.read_storage(&parsed_storage, &node).unwrap();

                    let big_map_ops =
                        StorageParser::get_big_map_operations_from_operations(&operations).unwrap();
                    for big_map_op in big_map_ops {
                        storage_parser.process_big_map(&big_map_op).unwrap();
                    }
                }

                let inserts = crate::table::insert::get_inserts();
                let filename = format!("test/{}-{}-inserts.json", contract.id, level);
                //println!("{} {}", filename, i);

                println!("cat > {} <<ENDOFJSON", filename);
                println!(
                    "{}",
                    to_string_pretty(&inserts, PrettyConfig::new()).unwrap()
                );
                println!("ENDOFJSON");

                let p = Path::new(&filename);

                if let Ok(file) = File::open(p) {
                    let reader = BufReader::new(file);
                    let v: crate::table::insert::Inserts = ron::de::from_reader(reader).unwrap();
                    assert_eq!(v.keys().len(), inserts.keys().len());
                    for key in inserts.keys() {
                        assert_eq!(v.get(key).unwrap(), inserts.get(key).unwrap());
                    }
                    inserts_tested += 1;
                }

                // let mut generator = crate::postgresql_generator::PostgresqlGenerator::new();
                // for (_key, value) in &inserts {
                //     println!("{}", generator.build_insert(value, level));
                // }
                crate::table::insert::clear_inserts();
            }
        }
        assert_eq!(inserts_tested, contract.operation_count);
    }
}

#[test]
fn test_get_big_map_operations_from_operations() {
    let json = json::parse(&load_test(
        "test/KT1U7Adyu5A7JWvEVSKjJEkG2He2SU1nATfq.level-149127.json",
    ))
    .unwrap();
    let ops = StorageParser::get_operations_from_block_json(
        &"KT1U7Adyu5A7JWvEVSKjJEkG2He2SU1nATfq",
        &json,
    )
    .unwrap();

    let diff_ops: Vec<JsonValue> =
        StorageParser::get_big_map_operations_from_operations(&ops).unwrap();

    assert_eq!(diff_ops.len(), 8);

    for op in diff_ops {
        println!("{}", op.to_string());
    }
}

#[test]
fn test_get_origination_operations_from_block() {
    let matching = json::parse(&load_test(
        "test/KT1U7Adyu5A7JWvEVSKjJEkG2He2SU1nATfq.level-132091.json",
    ))
    .unwrap();
    let contract_id = "KT1U7Adyu5A7JWvEVSKjJEkG2He2SU1nATfq";
    assert!(StorageParser::block_has_contract_origination(&matching, &contract_id).unwrap());

    for level in vec![
        132343, 123318, 123327, 123339, 128201, 132201, 132211, 132219, 132222, 132240, 132242,
        132259, 132262, 132278, 132282, 132285, 132298, 132300, 132343, 132367, 132383, 132384,
        132388, 132390, 135501, 138208, 149127,
    ] {
        let level_json = json::parse(&load_test(&format!(
            "test/KT1U7Adyu5A7JWvEVSKjJEkG2He2SU1nATfq.level-{}.json",
            level
        )))
        .unwrap();
        assert!(!StorageParser::block_has_contract_origination(&level_json, &contract_id).unwrap());
    }
}

#[test]
fn test_storage() {}
