use crate::sql::table::{Column, Table};
use crate::storage_structure::typing::SimpleExprTy;
use anyhow::Result;
use std::vec::Vec;

#[derive(Clone, Debug)]
pub struct PostgresqlGenerator {}

impl Default for PostgresqlGenerator {
    fn default() -> Self {
        Self::new()
    }
}

impl PostgresqlGenerator {
    pub(crate) fn new() -> Self {
        Self {}
    }

    pub(crate) fn create_sql(&self, column: Column) -> Option<String> {
        let name = Self::quote_id(&column.name);
        match column.column_type {
            SimpleExprTy::Address => Some(self.address(&name)),
            SimpleExprTy::Bool => Some(self.bool(&name)),
            SimpleExprTy::Bytes => Some(self.bytes(&name)),
            SimpleExprTy::Int => Some(self.int(&name)),
            SimpleExprTy::KeyHash => Some(self.string(&name)),
            SimpleExprTy::Mutez => Some(self.numeric(&name)),
            SimpleExprTy::Nat => Some(self.nat(&name)),
            SimpleExprTy::Stop => None,
            SimpleExprTy::String => Some(self.string(&name)),
            SimpleExprTy::Timestamp => Some(self.timestamp(&name)),
            SimpleExprTy::Unit => Some(self.unit(&name)),
        }
    }

    pub(crate) fn quote_id(s: &str) -> String {
        format!("\"{}\"", s)
    }

    pub(crate) fn address(&self, name: &str) -> String {
        format!("{} VARCHAR(127) NULL", name)
    }

    pub(crate) fn bool(&self, name: &str) -> String {
        format!("{} BOOLEAN NULL", name)
    }

    pub(crate) fn bytes(&self, name: &str) -> String {
        format!("{} TEXT NULL", name)
    }

    pub(crate) fn int(&self, name: &str) -> String {
        format!("{} NUMERIC(64) NULL", name)
    }

    pub(crate) fn nat(&self, name: &str) -> String {
        format!("{} NUMERIC(64) NULL", name)
    }

    pub(crate) fn numeric(&self, name: &str) -> String {
        format!("{} NUMERIC(64) NULL", name)
    }

    pub(crate) fn string(&self, name: &str) -> String {
        format!("{} TEXT NULL", name)
    }

    pub(crate) fn timestamp(&self, name: &str) -> String {
        format!("{} TIMESTAMP WITH TIME ZONE NULL", name)
    }

    pub(crate) fn unit(&self, name: &str) -> String {
        format!("{} VARCHAR(128) NULL", name)
    }

    pub(crate) fn start_table(&self, name: &str) -> String {
        format!(include_str!("../../sql/postgresql-table-header.sql"), name)
    }

    pub(crate) fn end_table(&self) -> String {
        include_str!("../../sql/postgresql-table-footer.sql").to_string()
    }

    pub(crate) fn create_columns(&self, table: &Table) -> Result<Vec<String>> {
        let mut cols: Vec<String> = match Self::parent_name(&table.name) {
            Some(x) => vec![format!(r#""{}_id" INTEGER"#, x)],
            None => vec![],
        };
        for column in &table.columns {
            if let Some(val) = self.create_sql(column.clone()) {
                cols.push(val);
            }
        }
        Ok(cols)
    }

    fn table_sql_columns(&self, table: &Table) -> Vec<String> {
        let mut cols: Vec<String> = table
            .columns
            .iter()
            .filter(|x| self.create_sql((*x).clone()).is_some())
            .map(|x| x.name.clone())
            .collect();

        if let Some(x) = Self::parent_name(&table.name) {
            cols.push(format!("{}_id", x))
        };
        cols.iter()
            .map(|c| Self::quote_id(c))
            .collect()
    }

    fn indices(&self, table: &Table) -> Vec<String> {
        let mut indices = table.indices.clone();
        if let Some(parent_key) = self.parent_key(table) {
            indices.push(parent_key);
        }
        indices
    }

    pub(crate) fn create_index(&self, table: &Table) -> String {
        let uniqueness_constraint = match table.has_uniqueness() {
            true => "UNIQUE",
            false => "",
        };
        format!(
            "CREATE {} INDEX ON \"{}\"({});\n",
            uniqueness_constraint,
            table.name,
            self.indices(table).join(", ")
        )
    }

    pub(crate) fn parent_name(name: &str) -> Option<String> {
        name.rfind('.')
            .map(|pos| name[0..pos].to_string())
    }

    fn parent_key(&self, table: &Table) -> Option<String> {
        Self::parent_name(&table.name)
            .map(|parent| format!(r#""{}_id""#, parent))
    }

    fn create_foreign_key_constraint(&self, table: &Table) -> Option<String> {
        Self::parent_name(&table.name).map(|parent| {
            format!(
                r#"FOREIGN KEY ("{}_id") REFERENCES "{}"(id)"#,
                parent, parent
            )
        })
    }

    pub(crate) fn create_common_tables(&self) -> String {
        include_str!("../../sql/postgresql-common-tables.sql").to_string()
    }

    pub(crate) fn create_table_definition(
        &self,
        table: &Table,
    ) -> Result<String> {
        let mut v: Vec<String> = vec![self.start_table(&table.name)];
        let mut columns: Vec<String> = self.create_columns(table)?;
        columns[0] = format!("\t{}", columns[0]);
        if let Some(fk) = self.create_foreign_key_constraint(table) {
            columns.push(fk);
        }
        let mut s = columns.join(",\n\t");
        s.push_str(",\n\t");
        v.push(s);
        v.push(self.end_table());
        v.push(self.create_index(table));
        Ok(v.join("\n"))
    }

    pub(crate) fn create_view_definition(
        &self,
        table: &Table,
    ) -> Result<String> {
        if table.name == "storage" {
            return Ok("".to_string());
        }
        let columns: Vec<String> = self.table_sql_columns(table);
        Ok(format!(
            r#"
CREATE VIEW "{}_live" AS (
    SELECT
        {}
    FROM "{}" t1
    JOIN tx_contexts ctx
      ON  ctx.id = t1.tx_context_id
      AND ctx.level = (
            SELECT
                MAX(ctx.level) AS _level
            FROM "{}" custom_table
            JOIN tx_contexts ctx ON custom_table.tx_context_id = ctx.id
        )
);
"#,
            table.name,
            columns.join(", "),
            table.name,
            table.name,
        ))
    }

    fn escape(s: &str) -> String {
        s.to_string()
            .replace("'", "''")
            .replace("\\", "\\\\")
    }
}
