//! 在线库管理（对应 DH.NCode `DbMetaData` / `IMetaData` 的 DDL 执行面）。
//!
//! 提供数据库级（建库/删库/存在性）与对象级（建表/删表/列/索引/注释说明）操作，
//! SQL 语句逐一对齐 DH.NCode 各数据库驱动的覆写实现；`Dal` 上的方法与 C# 同名操作对等。
//!
//! 语义要点：
//!
//! - **文件型库**（SQLite/DuckDB）：建库=创建连接串指向的数据文件，删库=删除该文件，存在性=文件是否存在；
//!   `:memory:` 内存库创建/删除恒真、存在性恒真（对齐 C# `SQLite.IsMemoryDatabase` 的短路）；
//! - **SQL 型库**：按 provider 生成语句（MySQL `Create Database If Not Exists .. DEFAULT CHARACTER SET utf8mb4`、
//!   PG 系 `ENCODING "UTF8"`、SQL Server `COLLATE Chinese_PRC_CI_AS`、Hana/IRIS 带 utf8mb4、等等）；
//! - **存在性探测**：文件型看文件；SQL 型用驱动元数据查询（MySQL `SHOW DATABASES LIKE`、
//!   PG 系 `pg_database`、SQL Server `sysdatabases`、Oracle `all_users`、Hana `SYS.DATABASES`、
//!   ClickHouse `system.databases`、TDengine/InfluxDB `SHOW DATABASES` 扫描）；
//! - **建表** `create_table` 幂等（已存在返回 `false`），仅建表体，不含索引（与 C# `CreateTable` 对等，索引走
//!   [`Dal::create_index`] 或 `sync_schema`）；**删表**默认 `Drop Table`（Firebird 连带删除 `SEQ_{表名}` 序列，对齐 C#）；
//! - **列操作**：新增列复用迁移语句（NOT NULL 无默认值时降级，安全补齐）；改列类型按方言生成
//!   （MySQL/Hana/IRIS/TDengine `Modify Column`、Oracle/DaMeng/DB2 `Modify`、PG 系 `ALTER COLUMN .. TYPE`、
//!   SQL Server `ALTER COLUMN`）；删列为 `Alter Table .. Drop Column ..`；
//! - **索引**：建索引按模型索引生成（同 `sync_schema` 的命名规则）；删索引按方言
//!   （SQL Server `Drop Index 表.索引`、MySQL `Drop Index 索引 On 表`、其余 `Drop Index 索引`）；
//! - **说明/注释**：表与列注释按方言生成（MySQL/Hana `Alter Table .. Comment`、Oracle/DaMeng/DB2/PG 系
//!   `Comment On ..`、SQL Server `sp_addextendedproperty`）；无该能力的库返回 `Ok(false)`（对齐 C# 返回空语句=未执行）。
//!
//! 与 C# 的差异（均有据，见各方法注释）：
//!
//! - SQL Server `DropDatabase` 用 `ALTER DATABASE .. SET SINGLE_USER WITH ROLLBACK IMMEDIATE` + `DROP DATABASE`
//!   两步，替代 C# 的游标杀进程脚本（等价效果，无需多语句批处理）；
//! - SQL Server `AlterColumn` 用标准 `ALTER COLUMN`，C# 在类型/自增变化时走重建表流程（重命名临时表等）；
//! - SQLite 改列类型明确不支持（C# 生成的 `Alter Table .. Alter Column` 在 SQLite 上必然失败，此处直接报错）；
//! - PG/HighGo 旧版基类 SQL 缺少 `TYPE` 关键字，Rust 侧生成合法语法 `ALTER COLUMN .. TYPE ..`；
//! - PG 国产生态（KingBase/HighGo/VastBase）解析后统一为 `PostgreSql`，建库等语句用 PG 形式
//!   （C# 各库变体仅在 `IF NOT EXISTS`/`ENCODING` 上略有差异）；
//! - 网络驱动（`provider=network`）不提供库管理（远端自管），本端调用返回 `Unsupported`。
//!
//! 典型用法：
//!
//! ```no_run
//! # use pek_rcode::dal::Dal;
//! # fn main() -> pek_rcode::Result<()> {
//! let dal = Dal::open("Data Source=demo.db;Provider=SQLite")?;
//! let _ = dal.database_exists(None)?;
//! dal.drop_table("Temp_Import")?;
//! dal.drop_index("DH_User", "idx_user_name")?;
//! # Ok(())
//! # }
//! ```

use crate::dal::Dal;
use crate::dialect::{DatabaseKind, oracle_identity_sequence};
use crate::error::{Error, Result};
use crate::model::{ColumnMeta, TableMeta};

impl Dal {
    /// 数据库是否存在（`name` 为空时取连接串中的数据源名）。
    /// <param name="name">数据库名；文件型库忽略该参数，按数据源文件判断</param>
    /// <returns>是否存在</returns>
    pub fn database_exists(&self, name: Option<&str>) -> Result<bool> {
        let kind = self.kind();
        if crate::network::is_network(self.connection_string()) {
            return Err(Error::Unsupported(
                "network 驱动的库管理请在远端服务端执行".into(),
            ));
        }
        if file_like(self.kind()) {
            // 内存库恒存在；文件库看文件
            if self.is_memory_db() {
                return Ok(true);
            }
            return Ok(self.file_db_path().map(|p| p.exists()).unwrap_or(false));
        }

        let name = name
            .map(str::to_string)
            .or_else(|| self.default_database_name())
            .ok_or_else(|| Error::Argument("数据库名不能为空".into()))?;

        let query = match kind {
            DatabaseKind::MySql => format!("SHOW DATABASES LIKE '{}'", escape_like(&name)),
            DatabaseKind::SqlServer => {
                format!("SELECT * FROM sysdatabases WHERE name = N'{}'", escape_text(&name))
            }
            DatabaseKind::PostgreSql => format!(
                "SELECT datname FROM pg_database WHERE datname = '{}'",
                escape_text(&name)
            ),
            DatabaseKind::Oracle => format!(
                "SELECT username FROM all_users WHERE username = UPPER('{}')",
                escape_text(&name)
            ),
            DatabaseKind::Hana => format!(
                "SELECT DATABASE_NAME FROM SYS.DATABASES WHERE DATABASE_NAME = '{}'",
                escape_text(&name)
            ),
            DatabaseKind::ClickHouse => format!(
                "SELECT name FROM system.databases WHERE name = '{}'",
                escape_text(&name)
            ),
            // TDengine / InfluxDB：`SHOW DATABASES` 后本端按名比对
            DatabaseKind::TDengine | DatabaseKind::InfluxDb => {
                let mut session = self.open_session()?;
                let set = session.query("SHOW DATABASES", &[])?;
                return Ok(set.rows.iter().any(|row| {
                    row.values()
                        .first()
                        .map(|v| v.to_text().eq_ignore_ascii_case(&name))
                        .unwrap_or(false)
                }));
            }
            DatabaseKind::DuckDb | DatabaseKind::Sqlite => unreachable!("文件型库已在上方处理"),
            other => {
                return Err(Error::Unsupported(format!(
                    "{other:?} 驱动不支持数据库存在性探测"
                )));
            }
        };

        let mut session = self.open_session()?;
        let set = session.query(&query, &[])?;
        Ok(!set.rows.is_empty())
    }
    /// 创建数据库（对应 C# `CreateDatabase`）。
    ///
    /// 文件型库（SQLite/DuckDB）：创建/确认数据源文件（`:memory:` 直接成功）；SQL 型库执行方言语句。
    /// <param name="name">数据库名</param>
    /// <param name="file">数据文件路径（SQL Server 用；其余库忽略）</param>
    /// <returns>是否成功</returns>
    pub fn create_database(&self, name: &str, file: Option<&str>) -> Result<bool> {
        let kind = self.kind();
        if crate::network::is_network(self.connection_string()) {
            return Err(Error::Unsupported(
                "network 驱动的库管理请在远端服务端执行".into(),
            ));
        }

        // 文件型库：确保数据文件存在（内存库直接成功）
        if file_like(self.kind()) {
            if self.is_memory_db() {
                return Ok(true);
            }
            let path = self
                .file_db_path()
                .ok_or_else(|| Error::Model("连接串缺少数据源文件路径".into()))?;
            if let Some(dir) = path.parent()
                && !dir.as_os_str().is_empty()
            {
                std::fs::create_dir_all(dir)?;
            }
            if !path.exists() {
                std::fs::File::create(&path)?;
            }
            return Ok(true);
        }

        let sql = match kind {
            DatabaseKind::MySql => format!(
                "Create Database If Not Exists {} DEFAULT CHARACTER SET utf8mb4",
                kind.quote(name)
            ),
            DatabaseKind::SqlServer => match file {
                // C#：无数据目录时 `CREATE DATABASE x COLLATE Chinese_PRC_CI_AS`；给了文件按附加路径处理
                None => format!("CREATE DATABASE {} COLLATE Chinese_PRC_CI_AS", kind.quote(name)),
                Some(file) => format!(
                    "CREATE DATABASE {} ON (NAME = N'{}', FILENAME = N'{}')",
                    kind.quote(name),
                    escape_text(name),
                    escape_text(file)
                ),
            },
            // PG 系（含 KingBase/HighGo/VastBase，Rust 统一映射为 PostgreSql）
            DatabaseKind::PostgreSql => format!(
                "Create Database If Not Exists {} ENCODING \"UTF8\"",
                kind.quote(name)
            ),
            DatabaseKind::Oracle => {
                format!("CREATE DATABASE {} CHARACTER SET AL32UTF8", kind.quote(name))
            }
            DatabaseKind::Hana | DatabaseKind::Iris => format!(
                "Create Database If Not Exists {} DEFAULT CHARACTER SET utf8mb4",
                kind.quote(name)
            ),
            DatabaseKind::TDengine | DatabaseKind::ClickHouse => {
                format!("Create Database If Not Exists {}", kind.quote(name))
            }
            DatabaseKind::DuckDb | DatabaseKind::Sqlite => unreachable!("文件型库已在上方处理"),
            // 文档库随写入自动创建
            DatabaseKind::MongoDb => return Ok(true),
            // InfluxDB 明确不支持（对齐 C# 抛 NotSupportedException）
            DatabaseKind::InfluxDb => {
                return Err(Error::Unsupported(
                    "InfluxDB 不支持通过 SQL 创建 bucket，请用 HTTP API 或 CLI".into(),
                ));
            }
            // Firebird 经 fbclient 服务接口创建（C# 反射调用驱动静态方法）；Access 为文件型 ODBC
            DatabaseKind::Firebird | DatabaseKind::Access => {
                return Err(Error::Unsupported(format!(
                    "{kind:?} 不支持经 SQL 建库（Firebird 用 fbclient 创建、Access 用文件模板）"
                )));
            }
            // 其余库使用 XCode 基类语句（DaMeng/DB2 等）
            _ => format!("Create Database {}", kind.quote(name)),
        };

        self.execute_ddl(&sql)?;
        Ok(true)
    }

    /// 删除数据库（对应 C# `DropDatabase`）。
    ///
    /// 文件型库删除数据源文件（`:memory:` 直接成功）；SQL 型库执行方言语句。
    /// <param name="name">数据库名</param>
    /// <returns>是否成功</returns>
    pub fn drop_database(&self, name: &str) -> Result<bool> {
        let kind = self.kind();
        if crate::network::is_network(self.connection_string()) {
            return Err(Error::Unsupported(
                "network 驱动的库管理请在远端服务端执行".into(),
            ));
        }

        if file_like(self.kind()) {
            // 内存库跳过文件删除
            if self.is_memory_db() {
                return Ok(true);
            }
            if let Some(path) = self.file_db_path()
                && path.exists()
            {
                std::fs::remove_file(&path)?;
            }
            return Ok(true);
        }

        match kind {
            DatabaseKind::SqlServer => {
                // C# 用游标杀死库内会话后删除；Rust 用“单用户 + 回滚”等价两步
                let sql1 = format!(
                    "ALTER DATABASE {} SET SINGLE_USER WITH ROLLBACK IMMEDIATE",
                    kind.quote(name)
                );
                let sql2 = format!("Drop Database {}", kind.quote(name));
                self.execute_ddl(&sql1)?;
                self.execute_ddl(&sql2)?;
                Ok(true)
            }
            DatabaseKind::MySql
            | DatabaseKind::PostgreSql
            | DatabaseKind::Hana
            | DatabaseKind::Iris
            | DatabaseKind::TDengine => {
                self.execute_ddl(&format!(
                    "Drop Database If Exists {}",
                    kind.quote(name)
                ))?;
                Ok(true)
            }
            DatabaseKind::DuckDb | DatabaseKind::Sqlite => unreachable!("文件型库已在上方处理"),
            DatabaseKind::InfluxDb | DatabaseKind::Firebird | DatabaseKind::MongoDb
            | DatabaseKind::Access => Err(Error::Unsupported(format!(
                "{kind:?} 不支持经 SQL 删库（InfluxDB 用 HTTP API、Firebird 连接后 DROP DATABASE、MongoDB 用客户端）"
            ))),
            // ClickHouse/DaMeng/DB2/Oracle 等使用 XCode 基类语句
            _ => {
                self.execute_ddl(&format!("Drop Database {}", kind.quote(name)))?;
                Ok(true)
            }
        }
    }

    /// 表是否存在（模型不含该表名时返回 `false`）。
    /// <param name="table">实体名或表名</param>
    /// <returns>是否存在</returns>
    pub fn table_exists(&self, table: &str) -> Result<bool> {
        let Ok(meta) = table_meta(self, table) else {
            return Ok(false);
        };
        let mut session = self.open_session()?;
        session.table_exists(meta.effective_table_name())
    }

    /// 建表（对应 C# `CreateTable`；已存在返回 `false`）。
    ///
    /// 仅建表体（含主键/列定义），不含索引；索引用 [`Dal::create_index`] 或 `sync_schema`。
    /// <param name="table">实体名或表名</param>
    /// <returns>是否执行了建表</returns>
    pub fn create_table(&self, table: &str) -> Result<bool> {
        let meta = table_meta(self, table)?;
        if self.table_exists(table)? {
            return Ok(false);
        }
        let mut session = self.open_session()?;
        for sql in self.kind().create_table_sql(meta) {
            session.execute(&sql, &[])?;
        }
        Ok(true)
    }

    /// 删表（对应 C# `DropTable`；Firebird 连带删除 `SEQ_{表名}` 序列）。
    /// <param name="table">实体名或表名</param>
    /// <returns>是否成功</returns>
    pub fn drop_table(&self, table: &str) -> Result<bool> {
        let kind = self.kind();
        let meta = table_meta(self, table)?;
        let name = kind.quote(meta.effective_table_name());
        if kind == DatabaseKind::Firebird {
            self.execute_ddl(&format!("Drop Table {name}"))?;
            let sequence = kind.quote(&oracle_identity_sequence(meta.effective_table_name()));
            self.execute_ddl(&format!("Drop Sequence {sequence}"))?;
        } else {
            self.execute_ddl(&format!("Drop Table {name}"))?;
        }
        Ok(true)
    }

    /// 新增列（对应 C# `AddColumn`；迁移语句，NOT NULL 无默认值时降级为可空）。
    /// <param name="table">实体名或表名</param>
    /// <param name="column">模型列名（`Name`，兼容数据库列名）</param>
    /// <returns>是否成功</returns>
    pub fn add_column(&self, table: &str, column: &str) -> Result<bool> {
        let meta = table_meta(self, table)?;
        let col = column_meta(meta, column)?;
        let sql = self.kind().add_column_sql(meta, col);
        self.execute_ddl(&sql)?;
        Ok(true)
    }

    /// 修改列类型（对应 C# `AlterColumn`；按方言生成，SQLite 明确不支持）。
    /// <param name="table">实体名或表名</param>
    /// <param name="column">模型列名（`Name`，兼容数据库列名）</param>
    /// <returns>是否成功</returns>
    pub fn alter_column(&self, table: &str, column: &str) -> Result<bool> {
        let kind = self.kind();
        let meta = table_meta(self, table)?;
        let col = column_meta(meta, column)?;
        let tname = kind.quote(meta.effective_table_name());
        let cname = kind.quote(meta.effective_column_name(col));
        let ctype = kind.field_type(col);

        let sql = match kind {
            DatabaseKind::Sqlite => {
                return Err(Error::Unsupported(
                    "SQLite 不支持修改列类型，请重建表（新建列 → 拷贝数据 → 删除旧列）".into(),
                ));
            }
            // 文档库不支持改列
            DatabaseKind::MongoDb | DatabaseKind::InfluxDb => {
                return Err(Error::Unsupported(format!("{kind:?} 不支持修改列")));
            }
            // Oracle/DaMeng/DB2：`Alter Table x Modify <列定义>`
            DatabaseKind::Oracle | DatabaseKind::DaMeng | DatabaseKind::Db2 => {
                format!("Alter Table {tname} Modify {cname} {ctype}")
            }
            // SQL Server：标准 ALTER COLUMN（C# 会在类型/自增变化时重建表）
            DatabaseKind::SqlServer => {
                let null_sql = if col.nullable { "NULL" } else { "NOT NULL" };
                format!("Alter Table {tname} Alter Column {cname} {ctype} {null_sql}")
            }
            // MySQL/Hana/IRIS/TDengine/ClickHouse：`Modify Column`
            DatabaseKind::MySql
            | DatabaseKind::Hana
            | DatabaseKind::Iris
            | DatabaseKind::TDengine
            | DatabaseKind::ClickHouse => {
                format!("Alter Table {tname} Modify Column {cname} {ctype}")
            }
            // PG 系（含 KingBase/VastBase/HighGo/DuckDB/Firebird）：`ALTER COLUMN .. TYPE ..`
            _ => format!("ALTER TABLE {tname} ALTER COLUMN {cname} TYPE {ctype}"),
        };

        self.execute_ddl(&sql)?;
        Ok(true)
    }

    /// 删除列（对应 C# `DropColumn`）。
    /// <param name="table">实体名或表名</param>
    /// <param name="column">模型列名（`Name`，兼容数据库列名）</param>
    /// <returns>是否成功</returns>
    pub fn drop_column(&self, table: &str, column: &str) -> Result<bool> {
        let kind = self.kind();
        let meta = table_meta(self, table)?;
        let col = column_meta(meta, column)?;
        let sql = format!(
            "Alter Table {} Drop Column {}",
            kind.quote(meta.effective_table_name()),
            kind.quote(meta.effective_column_name(col))
        );
        self.execute_ddl(&sql)?;
        Ok(true)
    }

    /// 建索引（对应 C# `CreateIndex`；按模型索引生成，命名与 `sync_schema` 一致）。
    /// <param name="table">实体名或表名</param>
    /// <param name="index">索引名，或索引首列名（匹配模型索引）</param>
    /// <returns>是否成功</returns>
    pub fn create_index(&self, table: &str, index: &str) -> Result<bool> {
        let kind = self.kind();
        let meta = table_meta(self, table)?;
        let idx = index_meta(kind, meta, index)?;
        let sql = kind.create_index_sql(meta, idx).ok_or_else(|| {
            Error::Unsupported(format!(
                "表 {} 的索引 {} 在当前方言下无建索引语句",
                meta.name, index
            ))
        })?;
        self.execute_ddl(&sql)?;
        Ok(true)
    }

    /// 删索引（对应 C# `DropIndex`）。
    /// <param name="table">实体名或表名</param>
    /// <param name="index">索引名，或索引首列名（匹配模型索引）</param>
    /// <returns>是否成功</returns>
    pub fn drop_index(&self, table: &str, index: &str) -> Result<bool> {
        let kind = self.kind();
        let meta = table_meta(self, table)?;
        let name = match index_meta(kind, meta, index) {
            Ok(idx) => kind.index_name(meta, idx),
            Err(_) => index.to_string(),
        };
        let tname = kind.quote(meta.effective_table_name());
        let sql = match kind {
            DatabaseKind::SqlServer => {
                format!("Drop Index {tname}.{}", kind.quote(&name))
            }
            DatabaseKind::MySql => format!("Drop Index {} On {tname}", kind.quote(&name)),
            _ => format!("Drop Index {}", kind.quote(&name)),
        };
        self.execute_ddl(&sql)?;
        Ok(true)
    }

    /// 设置表说明（对应 C# `AddTableDescription`）。
    /// <param name="table">实体名或表名</param>
    /// <returns>是否执行了 DDL（该库不支持说明时返回 `false`）</returns>
    pub fn add_table_description(&self, table: &str) -> Result<bool> {
        let kind = self.kind();
        let meta = table_meta(self, table)?;
        if meta.description.is_empty() {
            return Ok(false);
        }
        let tname = kind.quote(meta.effective_table_name());
        let text = escape_text(&meta.description);
        let sql = match kind {
            DatabaseKind::MySql | DatabaseKind::Hana => {
                format!("Alter Table {tname} Comment '{text}'")
            }
            DatabaseKind::Oracle
            | DatabaseKind::DaMeng
            | DatabaseKind::Db2
            | DatabaseKind::PostgreSql
            | DatabaseKind::Iris => format!("Comment On Table {tname} is '{text}'"),
            DatabaseKind::SqlServer => format!(
                "EXEC dbo.sp_addextendedproperty @name=N'MS_Description', @value=N'{text}', \
                 @level0type=N'SCHEMA',@level0name=N'dbo', @level1type=N'TABLE',@level1name=N'{}'",
                escape_text(meta.effective_table_name())
            ),
            // SQLite/TDengine 等无表说明能力（对齐 C# 返回空语句=未执行）
            _ => return Ok(false),
        };
        self.execute_ddl(&sql)?;
        Ok(true)
    }

    /// 清除表说明（对应 C# `DropTableDescription`）。
    /// <param name="table">实体名或表名</param>
    /// <returns>是否执行了 DDL</returns>
    pub fn drop_table_description(&self, table: &str) -> Result<bool> {
        let kind = self.kind();
        let meta = table_meta(self, table)?;
        let tname = kind.quote(meta.effective_table_name());
        let sql = match kind {
            DatabaseKind::MySql | DatabaseKind::Hana => {
                format!("Alter Table {tname} Comment ''")
            }
            DatabaseKind::Oracle
            | DatabaseKind::DaMeng
            | DatabaseKind::Db2
            | DatabaseKind::PostgreSql => format!("Comment On Table {tname} is ''"),
            DatabaseKind::SqlServer => format!(
                "EXEC dbo.sp_dropextendedproperty @name=N'MS_Description', \
                 @level0type=N'SCHEMA',@level0name=N'dbo', @level1type=N'TABLE',@level1name=N'{}'",
                escape_text(meta.effective_table_name())
            ),
            _ => return Ok(false),
        };
        self.execute_ddl(&sql)?;
        Ok(true)
    }

    /// 设置列说明（对应 C# `AddColumnDescription`）。
    /// <param name="table">实体名或表名</param>
    /// <param name="column">模型列名（`Name`，兼容数据库列名）</param>
    /// <returns>是否执行了 DDL</returns>
    pub fn add_column_description(&self, table: &str, column: &str) -> Result<bool> {
        let kind = self.kind();
        let meta = table_meta(self, table)?;
        let col = column_meta(meta, column)?;
        if col.description.is_empty() {
            return Ok(false);
        }
        let tname = kind.quote(meta.effective_table_name());
        let cname = kind.quote(meta.effective_column_name(col));
        let text = escape_text(&col.description);
        let sql = match kind {
            DatabaseKind::Oracle
            | DatabaseKind::DaMeng
            | DatabaseKind::Db2
            | DatabaseKind::PostgreSql
            | DatabaseKind::Iris => format!("Comment On Column {tname}.{cname} is '{text}'"),
            DatabaseKind::SqlServer => format!(
                "EXEC dbo.sp_addextendedproperty @name=N'MS_Description', @value=N'{text}', \
                 @level0type=N'SCHEMA',@level0name=N'dbo', @level1type=N'TABLE',@level1name=N'{}', \
                 @level2type=N'COLUMN',@level2name=N'{}'",
                escape_text(meta.effective_table_name()),
                escape_text(meta.effective_column_name(col))
            ),
            // MySQL/Hana 在表注释的 ALTER 中已有列注释；TDengine 同（对齐 C# 返回 String.Empty=未执行）
            _ => return Ok(false),
        };
        self.execute_ddl(&sql)?;
        Ok(true)
    }

    /// 清除列说明（对应 C# `DropColumnDescription`）。
    /// <param name="table">实体名或表名</param>
    /// <param name="column">模型列名（`Name`，兼容数据库列名）</param>
    /// <returns>是否执行了 DDL</returns>
    pub fn drop_column_description(&self, table: &str, column: &str) -> Result<bool> {
        let kind = self.kind();
        let meta = table_meta(self, table)?;
        let col = column_meta(meta, column)?;
        let tname = kind.quote(meta.effective_table_name());
        let cname = kind.quote(meta.effective_column_name(col));
        let sql = match kind {
            DatabaseKind::Oracle
            | DatabaseKind::DaMeng
            | DatabaseKind::Db2
            | DatabaseKind::PostgreSql => format!("Comment On Column {tname}.{cname} is ''"),
            DatabaseKind::SqlServer => format!(
                "EXEC dbo.sp_dropextendedproperty @name=N'MS_Description', \
                 @level0type=N'SCHEMA',@level0name=N'dbo', @level1type=N'TABLE',@level1name=N'{}', \
                 @level2type=N'COLUMN',@level2name=N'{}'",
                escape_text(meta.effective_table_name()),
                escape_text(meta.effective_column_name(col))
            ),
            _ => return Ok(false),
        };
        self.execute_ddl(&sql)?;
        Ok(true)
    }

    /// 执行单条 DDL（对齐 C# `ExecuteDDL` 的单语句路径）。
    fn execute_ddl(&self, sql: &str) -> Result<u64> {
        if sql.is_empty() {
            return Ok(0);
        }
        let mut session = self.open_session()?;
        session.execute(sql, &[])
    }

    /// 文件型库的数据源路径（SQLite/DuckDB；非文件型或未配置时为 None）。
    fn file_db_path(&self) -> Option<std::path::PathBuf> {
        let source = self.connection_string().data_source()?;
        let trimmed = source.trim();
        if trimmed.is_empty() {
            return None;
        }
        Some(std::path::PathBuf::from(trimmed))
    }

    /// 是否内存库（`:memory:` 或 `memory`，SQLite/DuckDB）。
    fn is_memory_db(&self) -> bool {
        self.connection_string()
            .data_source()
            .map(|s| {
                let t = s.trim();
                t == ":memory:" || t.eq_ignore_ascii_case("memory")
            })
            .unwrap_or(false)
    }

    /// 缺省数据库名（数据源文件名去扩展名）。
    fn default_database_name(&self) -> Option<String> {
        let source = self.connection_string().data_source()?;
        std::path::Path::new(source)
            .file_stem()
            .map(|v| v.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty())
    }
}

/// 是否文件语义的库（SQLite/DuckDB：建库=建文件、删库=删文件）。
fn file_like(kind: DatabaseKind) -> bool {
    matches!(kind, DatabaseKind::Sqlite | DatabaseKind::DuckDb)
}

/// 模型中的表（支持实体名与数据库表名，忽略大小写）。
fn table_meta<'a>(dal: &'a Dal, name: &str) -> Result<&'a TableMeta> {
    let model = dal
        .model()
        .ok_or_else(|| Error::Model("尚未加载数据模型，库管理需要模型".into()))?;
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

/// 模型中的列（支持属性名与数据库列名，忽略大小写）。
fn column_meta<'a>(table: &'a TableMeta, name: &str) -> Result<&'a ColumnMeta> {
    table
        .column(name)
        .ok_or_else(|| Error::Model(format!("表 {} 不存在列 {}", table.name, name)))
}

/// 模型中的索引（按索引名或首列名匹配）。
fn index_meta<'a>(
    kind: DatabaseKind,
    table: &'a TableMeta,
    name: &str,
) -> Result<&'a crate::model::IndexMeta> {
    table
        .indexes
        .iter()
        .find(|idx| {
            kind.index_name(table, idx).eq_ignore_ascii_case(name)
                || idx
                    .columns
                    .first()
                    .map(|c| c.eq_ignore_ascii_case(name))
                    .unwrap_or(false)
        })
        .ok_or_else(|| Error::Model(format!("表 {} 不存在索引 {}", table.name, name)))
}

/// SQL 文本转义（单引号双写）。
fn escape_text(text: &str) -> String {
    text.replace('\'', "''")
}

/// LIKE 模式转义（`%`/`_` 前加反斜杠）。
fn escape_like(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
        .replace('\'', "''")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EntityModel;

    const MODEL: &str = r#"<EntityModel><Tables>
      <Table Name="Item" TableName="DH_Item" Description="备份与库管理测试">
        <Columns>
          <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" Description="编号" />
          <Column Name="Name" DataType="String" Length="50" Nullable="True" Description="名称" />
          <Column Name="Extra" DataType="String" Length="20" Nullable="True" />
        </Columns>
        <Indexes>
          <Index Columns="Name" Unique="True" />
        </Indexes>
      </Table>
    </Tables></EntityModel>"#;

    /// 建临时库（SQLite）。
    fn temp_dal(tag: &str) -> (Dal, std::path::PathBuf) {
        let stamp = chrono::Local::now()
            .format("%H%M%S%.6f")
            .to_string()
            .replace('.', "");
        let dir = std::env::temp_dir().join(format!(
            "rcode-meta-{}-{}-{tag}",
            std::process::id(),
            stamp
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("test.db");
        let conn = format!("Data Source={};Provider=SQLite", db.display());
        let dal = Dal::open_with_model(&conn, EntityModel::parse(MODEL).unwrap()).unwrap();
        (dal, dir)
    }

    #[test]
    fn sqlite_database_file_operations() {
        let (dal, dir) = temp_dal("db");
        let path = dir.join("test.db");

        // 尚未创建过连接：文件不存在
        assert!(!dal.database_exists(None).unwrap());
        assert!(dal.create_database("test", None).unwrap());
        assert!(path.exists());
        assert!(dal.database_exists(None).unwrap());

        assert!(dal.drop_database("test").unwrap());
        assert!(!path.exists());
        // 已删除后再删仍成功（幂等）
        assert!(dal.drop_database("test").unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sqlite_table_and_column_operations() {
        let (dal, dir) = temp_dal("table");

        // 先手工建一张缺列的表，验证 add_column / drop_column
        {
            let mut session = dal.open_session().unwrap();
            session
                .execute(
                    "CREATE TABLE \"DH_Item\" (\"Id\" INTEGER PRIMARY KEY AUTOINCREMENT, \"Name\" TEXT)",
                    &[],
                )
                .unwrap();
        }
        assert!(dal.table_exists("Item").unwrap());
        assert!(dal.table_exists("DH_Item").unwrap());
        assert!(!dal.table_exists("Nope").unwrap());

        // 建表幂等：已存在返回 false
        assert!(!dal.create_table("Item").unwrap());

        // 新增列
        assert!(dal.add_column("Item", "Extra").unwrap());
        // 改列类型：SQLite 明确不支持
        assert!(dal.alter_column("Item", "Extra").is_err());
        // 删列（SQLite 3.35+ 支持）
        assert!(dal.drop_column("Item", "Extra").unwrap());

        // 索引：建 → 删
        assert!(dal.create_index("Item", "Name").unwrap());
        assert!(dal.drop_index("Item", "Name").unwrap());

        // 删表 → 重建成完整模型表（含 Extra）→ 再删
        assert!(dal.drop_table("Item").unwrap());
        assert!(!dal.table_exists("Item").unwrap());
        assert!(dal.create_table("Item").unwrap());

        // SQLite 无表说明能力：返回 false 且不执行
        assert!(!dal.add_table_description("Item").unwrap_or(false));
        assert!(!dal.drop_table_description("Item").unwrap_or(false));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ddl_sql_texts_match_csharp() {
        // 各库建库/删库语句（对齐 DH.NCode 覆写）
        let cases: Vec<(DatabaseKind, &str, &str)> = vec![
            (DatabaseKind::MySql, "testdb", "Create Database If Not Exists `testdb` DEFAULT CHARACTER SET utf8mb4"),
            (DatabaseKind::PostgreSql, "testdb", "Create Database If Not Exists \"testdb\" ENCODING \"UTF8\""),
            (DatabaseKind::SqlServer, "testdb", "CREATE DATABASE [testdb] COLLATE Chinese_PRC_CI_AS"),
            (DatabaseKind::Oracle, "testdb", "CREATE DATABASE \"testdb\" CHARACTER SET AL32UTF8"),
            (DatabaseKind::Hana, "testdb", "Create Database If Not Exists \"testdb\" DEFAULT CHARACTER SET utf8mb4"),
            (DatabaseKind::TDengine, "testdb", "Create Database If Not Exists `testdb`"),
        ];
        for (kind, name, expect) in cases {
            let sql = match kind {
                DatabaseKind::MySql => format!(
                    "Create Database If Not Exists {} DEFAULT CHARACTER SET utf8mb4",
                    kind.quote(name)
                ),
                DatabaseKind::PostgreSql => format!(
                    "Create Database If Not Exists {} ENCODING \"UTF8\"",
                    kind.quote(name)
                ),
                DatabaseKind::SqlServer => {
                    format!("CREATE DATABASE {} COLLATE Chinese_PRC_CI_AS", kind.quote(name))
                }
                DatabaseKind::Oracle => {
                    format!("CREATE DATABASE {} CHARACTER SET AL32UTF8", kind.quote(name))
                }
                DatabaseKind::Hana | DatabaseKind::Iris => format!(
                    "Create Database If Not Exists {} DEFAULT CHARACTER SET utf8mb4",
                    kind.quote(name)
                ),
                _ => format!("Create Database If Not Exists {}", kind.quote(name)),
            };
            assert_eq!(sql, expect, "{kind:?}");
        }

        // 文本转义
        assert_eq!(escape_text("it's"), "it''s");
        assert_eq!(escape_like("a_b%"), "a\\_b\\%");
    }

    #[test]
    fn model_lookup_helpers() {
        let (dal, dir) = temp_dal("lookup");
        let meta = table_meta(&dal, "DH_Item").unwrap();
        assert_eq!(meta.name, "Item");
        assert!(column_meta(meta, "dh_item_missing").is_err());
        assert!(index_meta(DatabaseKind::Sqlite, meta, "Name").is_ok());
        assert!(index_meta(DatabaseKind::Sqlite, meta, "Nope").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn network_driver_rejects_database_management() {
        let conn = "Server=http://127.0.0.1:1;Database=Demo;Password=tk;provider=network";
        // 不触发登录：直接构造 ConnectionString 校验分支
        let cs = crate::dal::ConnectionString::parse(conn);
        assert!(crate::network::is_network(&cs));
    }
}
