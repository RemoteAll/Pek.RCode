//! 数据备份 / 恢复 / 同步（对应 DH.NCode `DAL_Backup.cs` + `DbPackage.cs`）。
//!
//! 文件格式与 C# `DbPackage` 一致，可与 C# 双向收发：
//!
//! - **单表**：DbTable v3 二进制流（[`crate::dbtable`]）。表头列名为**实体属性名**（`Name`），
//!   表头行数为全表行数；文件后缀 `.gz` 时自动 GZip 压缩；
//! - **多表**：zip 包 = `{连接名}.xml`（模型 XML，`backup_schema=true` 时写入）+ 每表 `{实体名}.table`；
//! - 行数上限 i32（对齐 C# “最大支持 21 亿行”）。
//!
//! 与 C# 的差异（机制近似、语义对等）：
//!
//! - C# 备份/恢复为双线程 Actor 流水线；Rust 为单线程顺序执行（更简、确定性强）；
//! - Rust 备份按主键分页读取（每批 5000 行，`Dal.GetBatchSize()` 缺省值）后整体编码；
//!   C# 边读边写，超大表的内存占用更小（Rust 侧整表驻留内存，注意表规模）；
//! - 恢复按批多行 `INSERT`（Oracle/Firebird/Access/文档与时序库退化为逐行），不包裹事务（与 C# 一致）；
//! - NULL 在 DbTable 通道内折叠为类型默认值（与 C# 相同，见 [`crate::dbtable`] 模块说明）。
//!
//! 典型用法：
//!
//! ```no_run
//! # use pek_rcode::dal::Dal;
//! # fn main() -> pek_rcode::Result<()> {
//! let dal = Dal::open("Data Source=demo.db;Provider=SQLite")?;
//! let rows = dal.backup("User", "user.table")?; // 单表备份（.gz 后缀自动压缩）
//! let n = dal.restore("User", "user.table", false)?; // 恢复
//! let tables = dal.backup_all(&["User", "Order"], "all.zip", true)?; // 多表 + 模型
//! dal.restore_all("all.zip", None, true)?; // 多表恢复（表名从包内推导）
//! # let _ = (rows, n, tables);
//! # Ok(())
//! # }
//! ```

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

use crate::dal::Dal;
use crate::dbtable;
use crate::dialect::DatabaseKind;
use crate::error::{Error, Result};
use crate::model::TableMeta;
use crate::session::{DbRow, RowSet};
use crate::value::DbValue;

/// 备份分页批大小（对齐 C# `Dal.GetBatchSize()` 缺省值 5000）。
pub const BACKUP_BATCH: usize = 5000;

/// 单条语句的最大参数个数（多行 INSERT 拆分依据；兼容 SQLite 传统上限 999）。
const MAX_PARAMS_PER_STMT: usize = 900;

impl Dal {
    /// 备份单表到文件（`.gz` 后缀时 GZip 压缩；目录不存在时自动创建）。
    /// <param name="table">实体名或表名</param>
    /// <param name="file">目标文件</param>
    /// <returns>备份行数</returns>
    pub fn backup(&self, table: &str, file: impl AsRef<Path>) -> Result<u64> {
        let file = file.as_ref();
        if let Some(dir) = file.parent()
            && !dir.as_os_str().is_empty()
        {
            std::fs::create_dir_all(dir)?;
        }
        let mut fs = File::create(file)?;
        let rows = if is_gz(file) {
            let mut gz = flate2::write::GzEncoder::new(fs, flate2::Compression::default());
            let n = self.backup_to(table, &mut gz)?;
            gz.finish()?.flush()?;
            n
        } else {
            let n = self.backup_to(table, &mut fs)?;
            fs.flush()?;
            n
        };
        Ok(rows)
    }

    /// 备份单表到任意写入器（对应 C# `Backup(IDataTable, Stream)`）。
    /// <param name="table">实体名或表名</param>
    /// <param name="writer">目标流</param>
    /// <returns>备份行数</returns>
    pub fn backup_to(&self, table: &str, writer: &mut dyn Write) -> Result<u64> {
        let meta = table_meta(self, table)?;
        let set = fetch_all(self, meta)?;
        writer.write_all(&dbtable::encode_rowset(&set))?;
        writer.flush()?;
        Ok(set.rows.len() as u64)
    }

    /// 备份一批表到 zip 包（对应 C# `BackupAll`）。
    ///
    /// `backup_schema=true` 时写入 `{连接名}.xml`（仅含被备份的表）；单表失败跳过（对齐 `IgnoreError=true`）。
    /// <param name="tables">实体名集合</param>
    /// <param name="file">zip 文件</param>
    /// <param name="backup_schema">是否备份结构（模型 XML）</param>
    /// <returns>成功备份的表数</returns>
    pub fn backup_all(
        &self,
        tables: &[&str],
        file: impl AsRef<Path>,
        backup_schema: bool,
    ) -> Result<usize> {
        let file = file.as_ref();
        if let Some(dir) = file.parent()
            && !dir.as_os_str().is_empty()
        {
            std::fs::create_dir_all(dir)?;
        }
        let fs = File::create(file)?;
        let mut zip = zip::ZipWriter::new(fs);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);

        if backup_schema {
            let xml = filtered_model_xml(self, tables)?;
            zip.start_file(format!("{}.xml", conn_name(self)), options)
                .map_err(zip_error)?;
            zip.write_all(xml.as_bytes())?;
        }

        let mut count = 0usize;
        for name in tables {
            let Ok(meta) = table_meta(self, name) else {
                continue;
            };
            let Ok(set) = fetch_all(self, meta) else {
                continue;
            };
            zip.start_file(format!("{}.table", meta.name), options)
                .map_err(zip_error)?;
            zip.write_all(&dbtable::encode_rowset(&set))?;
            count += 1;
        }
        zip.finish().map_err(zip_error)?;
        Ok(count)
    }

    /// 从文件恢复单表数据（对应 C# `Restore(file, table, setSchema)`）。
    ///
    /// `set_schema=true` 时先确保目标表存在（不存在则建表）；文件不存在返回 0。
    /// <param name="table">实体名或表名</param>
    /// <param name="file">备份文件（`.gz` 自动解压）</param>
    /// <param name="set_schema">是否自动建表</param>
    /// <returns>恢复行数</returns>
    pub fn restore(&self, table: &str, file: impl AsRef<Path>, set_schema: bool) -> Result<u64> {
        let file = file.as_ref();
        if !file.exists() {
            return Ok(0);
        }
        let meta = table_meta(self, table)?;
        if set_schema {
            ensure_table(self, meta)?;
        }
        let fs = File::open(file)?;
        if is_gz(file) {
            let mut gz = flate2::read::GzDecoder::new(fs);
            self.restore_from(table, &mut gz)
        } else {
            let mut fs = fs;
            self.restore_from(table, &mut fs)
        }
    }

    /// 从任意读取器恢复单表数据（对应 C# `Restore(Stream, IDataTable)`）。
    ///
    /// 空数据视为空备份返回 0（对齐 C# 空文件行为）；表头列按**实体属性名**匹配模型列，
    /// 匹配不上的列跳过（对齐 C# `WriteDbActor` 的列匹配）。
    /// <param name="table">实体名或表名</param>
    /// <param name="reader">数据流</param>
    /// <returns>恢复行数</returns>
    pub fn restore_from(&self, table: &str, reader: &mut dyn Read) -> Result<u64> {
        let meta = table_meta(self, table)?;
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes)?;
        if bytes.is_empty() {
            return Ok(0);
        }
        if !dbtable::is_dbtable(&bytes) {
            return Err(Error::Db("备份数据不是 DbTable 二进制流".into()));
        }
        let set = dbtable::decode_rowset(&bytes)?;
        self.insert_rowset(meta, &set)
    }

    /// 从 zip 包恢复一批表（对应 C# `RestoreAll`）。
    ///
    /// `tables=None` 时从包内 `*.table` 条目推导表名（C# 从包内模型 XML 读取，等价行为）；
    /// `set_schema=true` 时逐表确保存在（不存在则建表）；单表失败跳过并继续。
    /// <param name="file">zip 文件</param>
    /// <param name="tables">表集合（实体名）</param>
    /// <param name="set_schema">是否自动建表</param>
    /// <returns>成功恢复的表名列表</returns>
    pub fn restore_all(
        &self,
        file: impl AsRef<Path>,
        tables: Option<&[&str]>,
        set_schema: bool,
    ) -> Result<Vec<String>> {
        let file = file.as_ref();
        if !file.exists() {
            return Ok(Vec::new());
        }
        let fs = File::open(file)?;
        let mut zip = zip::ZipArchive::new(fs).map_err(zip_error)?;

        let names: Vec<String> = match tables {
            Some(list) => list.iter().map(|s| (*s).to_string()).collect(),
            None => (0..zip.len())
                .filter_map(|index| {
                    let entry = zip.by_index(index).ok()?;
                    entry
                        .name()
                        .strip_suffix(".table")
                        .map(str::to_string)
                        .filter(|s| !s.is_empty())
                })
                .collect(),
        };

        let mut done = Vec::new();
        for name in &names {
            let Ok(meta) = table_meta(self, name) else {
                continue;
            };
            if set_schema && ensure_table(self, meta).is_err() {
                continue;
            }
            let entry_name = format!("{}.table", meta.name);
            let mut bytes = Vec::new();
            {
                let Ok(mut entry) = zip.by_name(&entry_name) else {
                    continue;
                };
                if entry.read_to_end(&mut bytes).is_err() {
                    continue;
                }
            }
            if bytes.is_empty() {
                continue;
            }
            let Ok(set) = dbtable::decode_rowset(&bytes) else {
                continue;
            };
            if self.insert_rowset(meta, &set).is_ok() {
                done.push(meta.name.clone());
            }
        }
        Ok(done)
    }

    /// 单表数据同步到另一个库（对应 C# `Sync`）。
    ///
    /// `sync_schema=true` 时目标表不存在则按本端模型建表；目标库不做列补齐（对齐 C# `SetTables` 的建表语义）。
    /// 同步为**纯追加**（与 C# 一致，不判重）：目标已有同主键行时该批插入失败。
    /// <param name="table">实体名或表名</param>
    /// <param name="target">目标数据访问层</param>
    /// <param name="sync_schema">是否同步结构（建表）</param>
    /// <returns>同步行数</returns>
    pub fn sync_table(&self, table: &str, target: &Dal, sync_schema: bool) -> Result<u64> {
        let meta = table_meta(self, table)?;
        if sync_schema {
            ensure_table(target, meta)?;
        }
        let set = fetch_all(self, meta)?;
        target.insert_rowset(meta, &set)
    }

    /// 一批表同步到另一个库（对应 C# `SyncAll`；单表失败跳过，返回成功表 → 行数）。
    ///
    /// 与 [`Dal::sync_table`] 相同为纯追加语义。
    /// <param name="tables">实体名集合</param>
    /// <param name="target">目标数据访问层</param>
    /// <param name="sync_schema">是否同步结构（建表）</param>
    /// <returns>表名 → 行数</returns>
    pub fn sync_all(
        &self,
        tables: &[&str],
        target: &Dal,
        sync_schema: bool,
    ) -> Result<BTreeMap<String, u64>> {
        let mut map = BTreeMap::new();
        for name in tables {
            let Ok(meta) = table_meta(self, name) else {
                continue;
            };
            if sync_schema && ensure_table(target, meta).is_err() {
                continue;
            }
            let Ok(set) = fetch_all(self, meta) else {
                continue;
            };
            if let Ok(rows) = target.insert_rowset(meta, &set) {
                map.insert(meta.name.clone(), rows);
            }
        }
        Ok(map)
    }

    /// 按批把备份行集写入本库（单表恢复/同步的公共实现）。
    fn insert_rowset(&self, meta: &TableMeta, set: &RowSet) -> Result<u64> {
        if set.rows.is_empty() {
            return Ok(0);
        }
        let kind = self.kind();

        // 表头列名（属性名）→ 模型字段；未匹配的列跳过（对齐 C# WriteDbActor 的列匹配）
        let mut fields: Vec<String> = Vec::new();
        let mut indexes: Vec<usize> = Vec::new();
        for (index, name) in set.columns.iter().enumerate() {
            if let Some(col) = meta.column(name) {
                fields.push(col.name.clone());
                indexes.push(index);
            }
        }
        if fields.is_empty() {
            return Err(Error::Model(format!(
                "备份数据的列在表 {} 的模型中均无匹配",
                meta.name
            )));
        }

        let rows_per_stmt = if supports_multi_row(kind) {
            (MAX_PARAMS_PER_STMT / fields.len().max(1)).max(1)
        } else {
            1
        };

        let mut session = self.open_session()?;
        let mut inserted = 0u64;
        for chunk in set.rows.chunks(rows_per_stmt) {
            let (sql, params) = build_insert(kind, meta, &fields, &indexes, chunk);
            session.execute(&sql, &params)?;
            inserted += chunk.len() as u64;
        }
        Ok(inserted)
    }
}

/// 是否 GZip 文件（`.gz` 后缀，忽略大小写）。
fn is_gz(file: &Path) -> bool {
    file.extension()
        .map(|e| e.eq_ignore_ascii_case("gz"))
        .unwrap_or(false)
}

/// zip 错误包装。
fn zip_error(e: zip::result::ZipError) -> Error {
    Error::Db(format!("备份包处理失败：{e}"))
}

/// 模型中的表（支持实体名与数据库表名，忽略大小写）。
fn table_meta<'a>(dal: &'a Dal, name: &str) -> Result<&'a TableMeta> {
    let model = dal
        .model()
        .ok_or_else(|| Error::Model("尚未加载数据模型，备份/恢复需要模型".into()))?;
    model
        .table(name)
        .or_else(|| {
            model
                .tables
                .iter()
                .find(|t| t.effective_table_name().eq_ignore_ascii_case(name))
        })
        .ok_or_else(|| Error::Model(format!("模型不存在表 {name}")))
}

/// 连接名（zip 内模型条目命名）：模型 `ConnName` → 数据源文件名 → `Model`。
fn conn_name(dal: &Dal) -> String {
    if let Some(name) = dal.model().and_then(|m| m.options.conn_name()) {
        return name.to_string();
    }
    dal.connection_string()
        .data_source()
        .map(|s| {
            Path::new(s)
                .file_stem()
                .map(|v| v.to_string_lossy().into_owned())
                .unwrap_or_else(|| s.to_string())
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Model".into())
}

/// 仅导出指定表的模型 XML（对齐 C# `DAL.Export(tables)`）。
fn filtered_model_xml(dal: &Dal, tables: &[&str]) -> Result<String> {
    let model = dal
        .model()
        .ok_or_else(|| Error::Model("尚未加载数据模型，无法备份结构".into()))?;
    let mut filtered = (**model).clone();
    filtered.tables.retain(|t| {
        tables.iter().any(|n| {
            t.name.eq_ignore_ascii_case(n) || t.effective_table_name().eq_ignore_ascii_case(n)
        })
    });
    Ok(filtered.to_xml())
}

/// 确保表存在（不存在则按本端模型建表，对齐 C# `Dal.SetTables(table)`）。
fn ensure_table(dal: &Dal, meta: &TableMeta) -> Result<()> {
    let mut session = dal.open_session()?;
    if session.table_exists(meta.effective_table_name())? {
        return Ok(());
    }
    for sql in dal.kind().create_table_sql(meta) {
        session.execute(&sql, &[])?;
    }
    Ok(())
}

/// 分页读取全表数据（属性名作为列名；按主键/自增列排序，缺省取首列）。
fn fetch_all(dal: &Dal, meta: &TableMeta) -> Result<RowSet> {
    let kind = dal.kind();
    let columns: Vec<String> = meta.columns.iter().map(|c| c.name.clone()).collect();
    let select = format!(
        "SELECT {} FROM {}",
        meta.columns
            .iter()
            .map(|c| kind.quote(meta.effective_column_name(c)))
            .collect::<Vec<_>>()
            .join(", "),
        kind.quote(meta.effective_table_name())
    );
    let order = meta
        .identity()
        .or_else(|| meta.columns.iter().find(|c| c.primary_key))
        .or_else(|| meta.columns.first())
        .map(|c| format!("ORDER BY {}", kind.quote(meta.effective_column_name(c))))
        .unwrap_or_default();

    let mut session = dal.open_session()?;
    let mut set = RowSet::new(columns);
    let mut offset = 0usize;
    loop {
        let sql = kind.apply_paging(&select, &order, offset, BACKUP_BATCH);
        let page = session.query(&sql, &[])?;
        let count = page.rows.len();
        if count > 0 {
            set.rows.extend(page.rows);
        }
        if count < BACKUP_BATCH {
            break;
        }
        offset += count;
    }
    Ok(set)
}

/// 该方言是否支持多行 `VALUES (...), (...)` 写法。
fn supports_multi_row(kind: DatabaseKind) -> bool {
    matches!(
        kind,
        DatabaseKind::Sqlite
            | DatabaseKind::MySql
            | DatabaseKind::PostgreSql
            | DatabaseKind::SqlServer
            | DatabaseKind::DuckDb
            | DatabaseKind::Hana
            | DatabaseKind::DaMeng
            | DatabaseKind::Iris
            | DatabaseKind::Db2
            | DatabaseKind::ClickHouse
            | DatabaseKind::TDengine
    )
}

/// 组装（多行）INSERT 语句与参数。
fn build_insert(
    kind: DatabaseKind,
    meta: &TableMeta,
    fields: &[String],
    indexes: &[usize],
    rows: &[DbRow],
) -> (String, Vec<DbValue>) {
    let columns = fields
        .iter()
        .map(|f| {
            let col = meta.column(f);
            match col {
                Some(col) => kind.quote(meta.effective_column_name(col)),
                None => kind.quote(f),
            }
        })
        .collect::<Vec<_>>()
        .join(", ");

    let mut marks = Vec::with_capacity(rows.len());
    let mut params = Vec::with_capacity(rows.len() * indexes.len());
    let mut index = 0usize;
    for row in rows {
        let mut group = Vec::with_capacity(indexes.len());
        let values = row.values();
        for &i in indexes {
            group.push(kind.placeholder(index));
            params.push(values.get(i).cloned().unwrap_or(DbValue::Null));
            index += 1;
        }
        marks.push(format!("({})", group.join(", ")));
    }

    let sql = format!(
        "INSERT INTO {} ({columns}) VALUES {}",
        kind.quote(meta.effective_table_name()),
        marks.join(", ")
    );
    (sql, params)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EntityModel;
    use crate::value::DbValue;

    const MODEL: &str = r#"<EntityModel><Tables>
      <Table Name="Item" TableName="DH_Item" Description="备份测试表">
        <Columns>
          <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
          <Column Name="Name" DataType="String" Length="50" Nullable="True" />
          <Column Name="Amount" DataType="Decimal" Precision="18" Scale="4" Nullable="True" />
          <Column Name="Ok" DataType="Boolean" Nullable="True" />
          <Column Name="CreateTime" DataType="DateTime" Nullable="True" />
          <Column Name="SId" ColumnName="dh_sid" DataType="Int64" Nullable="True" />
        </Columns>
      </Table>
      <Table Name="Key" TableName="DH_Key" Description="字符串主键">
        <Columns>
          <Column Name="Key" DataType="String" Length="40" PrimaryKey="True" />
          <Column Name="Value" DataType="String" Length="100" Nullable="True" />
        </Columns>
      </Table>
    </Tables></EntityModel>"#;

    /// 建临时库并同步结构。
    fn temp_dal(tag: &str) -> (Dal, std::path::PathBuf) {
        let stamp = chrono::Local::now()
            .format("%H%M%S%.6f")
            .to_string()
            .replace('.', "");
        let dir = std::env::temp_dir().join(format!(
            "rcode-backup-{}-{}-{tag}",
            std::process::id(),
            stamp
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("test.db");
        let conn = format!("Data Source={};Provider=SQLite", db.display());
        let dal = Dal::open_with_model(&conn, EntityModel::parse(MODEL).unwrap()).unwrap();
        dal.sync_schema().unwrap();
        (dal, dir)
    }

    fn item_row(name: &str, amount: &str, ok: bool, sid: Option<i64>) -> Vec<DbValue> {
        vec![
            DbValue::Text(name.into()),
            DbValue::Decimal(amount.parse().unwrap()),
            DbValue::Bool(ok),
            DbValue::DateTime(
                chrono::NaiveDate::from_ymd_opt(2026, 9, 27)
                    .unwrap()
                    .and_hms_micro_opt(1, 2, 3, 456_000)
                    .unwrap(),
            ),
            sid.map(DbValue::Int).unwrap_or(DbValue::Null),
        ]
    }

    fn insert_items(dal: &Dal, rows: &[Vec<DbValue>]) {
        let mut session = dal.open_session().unwrap();
        for row in rows {
            session
                .execute(
                    "INSERT INTO DH_Item(Name, Amount, Ok, CreateTime, dh_sid) VALUES(?, ?, ?, ?, ?)",
                    row,
                )
                .unwrap();
        }
    }

    fn count(dal: &Dal, table: &str) -> i64 {
        let mut session = dal.open_session().unwrap();
        let set = session
            .query(&format!("SELECT COUNT(*) FROM {table}"), &[])
            .unwrap();
        set.rows[0].get(0).unwrap().as_i64().unwrap()
    }

    #[test]
    fn backup_restore_roundtrip_with_header_names() {
        let (dal, dir) = temp_dal("rt");
        insert_items(
            &dal,
            &[
                item_row("中文名称", "12.3400", true, Some(9_000_000_001)),
                item_row("b", "0.0001", false, None),
            ],
        );

        let file = dir.join("item.table");
        assert_eq!(dal.backup("Item", &file).unwrap(), 2);

        // 文件为 DbTable 二进制；表头列名是**实体属性名**（对齐 C# 备份中的改名列）
        let bytes = std::fs::read(&file).unwrap();
        assert!(dbtable::is_dbtable(&bytes));
        let set = dbtable::decode_rowset(&bytes).unwrap();
        let columns: Vec<String> = set.columns.as_ref().clone();
        assert_eq!(columns, vec!["Id", "Name", "Amount", "Ok", "CreateTime", "SId"]);

        // 清空后恢复：行数与主键值均应还原
        {
            let mut session = dal.open_session().unwrap();
            session.execute("DELETE FROM DH_Item", &[]).unwrap();
        }
        assert_eq!(dal.restore("Item", &file, false).unwrap(), 2);
        assert_eq!(count(&dal, "DH_Item"), 2);

        let mut session = dal.open_session().unwrap();
        let set = session
            .query("SELECT Id, Name, Amount, dh_sid FROM DH_Item ORDER BY Id", &[])
            .unwrap();
        assert_eq!(set.rows[0].get(0).unwrap().as_i64(), Some(1));
        assert_eq!(set.rows[0].get(1).unwrap().as_str(), Some("中文名称"));
        assert_eq!(set.rows[1].get(2).unwrap().to_text(), "0.0001");
        // NULL 在 DbTable 通道内折叠为类型默认值（与 C# 一致）：SId 的 NULL 还原为 0
        assert_eq!(set.rows[1].get(3).unwrap().as_i64(), Some(0));
    }

    #[test]
    fn backup_gz_roundtrip() {
        let (dal, dir) = temp_dal("gz");
        insert_items(&dal, &[item_row("a", "1.5", true, None)]);
        insert_items(&dal, &[item_row("b", "2.5", false, Some(7))]);

        let file = dir.join("item.table.gz");
        assert_eq!(dal.backup("Item", &file).unwrap(), 2);
        // GZip 幻数
        let raw = std::fs::read(&file).unwrap();
        assert_eq!(&raw[..2], &[0x1f, 0x8b]);

        {
            let mut session = dal.open_session().unwrap();
            session.execute("DELETE FROM DH_Item", &[]).unwrap();
        }
        assert_eq!(dal.restore("Item", &file, false).unwrap(), 2);
        assert_eq!(count(&dal, "DH_Item"), 2);
    }

    #[test]
    fn backup_all_and_restore_all_zip() {
        let (dal, dir) = temp_dal("zip");
        insert_items(&dal, &[item_row("a", "1.0", true, None)]);
        {
            let mut session = dal.open_session().unwrap();
            session
                .execute("INSERT INTO DH_Key(\"Key\", Value) VALUES('k1', 'v1')", &[])
                .unwrap();
        }

        let file = dir.join("all.zip");
        assert_eq!(dal.backup_all(&["Item", "Key"], &file, true).unwrap(), 2);

        // zip 内容：`{连接名}.xml` + `{实体名}.table`
        {
            let fs = File::open(&file).unwrap();
            let mut zip = zip::ZipArchive::new(fs).unwrap();
            let names: Vec<String> = (0..zip.len())
                .map(|i| zip.by_index(i).unwrap().name().to_string())
                .collect();
            assert_eq!(names.len(), 3, "{names:?}");
            assert!(names.iter().any(|n| n.ends_with(".xml")), "{names:?}");
            assert!(names.contains(&"Item.table".to_string()), "{names:?}");
            assert!(names.contains(&"Key.table".to_string()), "{names:?}");
        }

        // 清空两表 → 按包内条目恢复
        {
            let mut session = dal.open_session().unwrap();
            session.execute("DELETE FROM DH_Item", &[]).unwrap();
            session.execute("DELETE FROM DH_Key", &[]).unwrap();
        }
        let done = dal.restore_all(&file, None, true).unwrap();
        assert_eq!(done.len(), 2, "{done:?}");
        assert_eq!(count(&dal, "DH_Item"), 1);
        assert_eq!(count(&dal, "DH_Key"), 1);

        // 文件不存在：返回空
        assert!(dal.restore_all(dir.join("none.zip"), None, true).unwrap().is_empty());
    }

    #[test]
    fn sync_table_and_sync_all_to_another_db() {
        let (source, dir1) = temp_dal("src");
        let (target, dir2) = temp_dal("dst");
        insert_items(
            &source,
            &[item_row("a", "1.0", true, None), item_row("b", "2.0", false, Some(5))],
        );
        {
            let mut session = source.open_session().unwrap();
            session
                .execute("INSERT INTO DH_Key(\"Key\", Value) VALUES('k1', 'v1')", &[])
                .unwrap();
        }

        // 先清空目标（sync_schema 之前），验证 Sync 的建表语义
        {
            let mut session = target.open_session().unwrap();
            session.execute("DROP TABLE DH_Item", &[]).unwrap();
            session.execute("DROP TABLE DH_Key", &[]).unwrap();
        }

        let rows = source.sync_table("Item", &target, true).unwrap();
        assert_eq!(rows, 2);
        assert_eq!(count(&target, "DH_Item"), 2);

        // 再次同步到清空后的目标：Sync 为“纯追加”（不判重），主键冲突时整批失败跳过
        {
            let mut session = target.open_session().unwrap();
            session.execute("DELETE FROM DH_Item", &[]).unwrap();
        }
        let map = source.sync_all(&["Item", "Key"], &target, true).unwrap();
        assert_eq!(map.get("Item"), Some(&2));
        assert_eq!(map.get("Key"), Some(&1));
        assert_eq!(count(&target, "DH_Item"), 2);
        assert_eq!(count(&target, "DH_Key"), 1);

        let _ = std::fs::remove_dir_all(&dir1);
        let _ = std::fs::remove_dir_all(&dir2);
    }
}
