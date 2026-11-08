//! 数据库方言：把逻辑模型/查询翻译成各数据库的具体 SQL。
//!
//! 对应 DH.NCode 中每个数据库的 `DbBase` 子类（SqlServer.cs / MySql.cs / SQLite.cs ...）：
//! - 标识符引用、参数占位符
//! - 字段类型映射（CLR 类型 → 数据库类型）
//! - 建表 / 加列 DDL
//! - 分页语法（`LIMIT/OFFSET` 与 `OFFSET..FETCH`）
//! - 自增主键与自增 ID 读取语句
//!
//! 当前阶段：SQLite 方言用于真实执行（驱动已实现）；其余数据库方言可用于
//! 生成脚本与后续驱动接入（MySQL 优先）。

use crate::error::{Error, Result};
use crate::model::{ColumnMeta, IndexMeta, TableMeta};
use crate::types::DataType;

/// 数据库类型（对应 C# 的 `DatabaseType`，覆盖 DH.NCode 全部主要数据库）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DatabaseKind {
    /// SQLite（文件数据库）
    Sqlite,
    /// MySQL / MariaDB
    MySql,
    /// Microsoft SQL Server
    SqlServer,
    /// PostgreSQL（含 HighGo/KingBase/VastBase）
    PostgreSql,
    /// 人大金仓 KingBase（C# `DatabaseType.KingBase = 13`）
    ///
    /// 同 PG 协议，**方言行为目前委托给 [`DatabaseKind::PostgreSql`]**；
    /// C# 侧是独立实现类（`KingBase.cs` 612 行），其覆写点待逐项移植（见 [`DATABASE_TYPES`] 注释）。
    KingBase,
    /// 瀚高 HighGo（C# `DatabaseType.HighGo = 14`）
    ///
    /// 同 PG 协议，方言行为委托 [`DatabaseKind::PostgreSql`]；C# `HighGo.cs` 474 行的品牌差异待移植。
    HighGo,
    /// 海量 VastBase（C# `DatabaseType.VastBase = 16`）
    ///
    /// 同 PG 协议，方言行为委托 [`DatabaseKind::PostgreSql`]；C# `VastBase.cs` 1080 行的品牌差异待移植。
    VastBase,
    /// Oracle
    Oracle,
    /// DuckDB（内嵌分析数据库）
    DuckDb,
    /// Firebird（含 InterBase 兼容）
    Firebird,
    /// ClickHouse（列式分析库，HTTP 协议）
    ClickHouse,
    /// TDengine（时序数据库，REST 协议）
    TDengine,
    /// InfluxDB（时序数据库，HTTP 行协议）
    InfluxDb,
    /// SAP HANA
    Hana,
    /// MongoDB（文档库，非 SQL，走子集翻译）
    MongoDb,
    /// IBM DB2（按 DH.NCode 采用 Oracle 兼容模式）
    Db2,
    /// DaMeng 达梦（DM8）
    DaMeng,
    /// InterSystems IRIS
    Iris,
    /// Microsoft Access（JET/ACE）
    Access,
}

impl DatabaseKind {
    /// 全部已建模的数据库类型。
    pub const ALL: [DatabaseKind; 19] = [
        DatabaseKind::Sqlite,
        DatabaseKind::MySql,
        DatabaseKind::SqlServer,
        DatabaseKind::PostgreSql,
        DatabaseKind::KingBase,
        DatabaseKind::HighGo,
        DatabaseKind::VastBase,
        DatabaseKind::Oracle,
        DatabaseKind::DuckDb,
        DatabaseKind::Firebird,
        DatabaseKind::ClickHouse,
        DatabaseKind::TDengine,
        DatabaseKind::InfluxDb,
        DatabaseKind::Hana,
        DatabaseKind::MongoDb,
        DatabaseKind::Db2,
        DatabaseKind::DaMeng,
        DatabaseKind::Iris,
        DatabaseKind::Access,
    ];

    /// 是否 PostgreSQL 协议族（含国产 PG 系：KingBase / HighGo / VastBase）。
    ///
    /// **判断方言行为一律用该方法**，不要写 `matches!(kind, DatabaseKind::PostgreSql | ...)`——
    /// 后者在新增族内品牌时会静默漏掉（已踩：加变体后不当渲染会从 `TRUE` 退化成 `1`）。
    pub fn is_postgres_family(&self) -> bool {
        matches!(
            self,
            DatabaseKind::PostgreSql
                | DatabaseKind::KingBase
                | DatabaseKind::HighGo
                | DatabaseKind::VastBase
        )
    }

    /// 是否 MySQL 协议族（含 NovaDb）。
    ///
    /// NovaDb 在 [`DatabaseKind`] 里**不单独建变体**（用户 2026-11-01 明确「NovaDb 这个就不需要了」），
    /// 由 `from_provider` / `from_remote_name` 直接归并到 [`DatabaseKind::MySql`]。
    pub fn is_mysql_family(&self) -> bool {
        matches!(self, DatabaseKind::MySql)
    }

    /// 显示名称。
    ///
    /// 与 C# `DatabaseType` 枚举名逐字保持一致（如 `SQLite`/`PostgreSQL`/`DuckDB`/`IRIS`），
    /// 供 [`crate::db_service::DbService::login_info`] 返回给 C# `DbClient` 反序列化
    /// （NewLife 枚举解析区分大小写，不能返回 `Sqlite` 这类 Rust 调试名）。
    pub fn name(&self) -> &'static str {
        match self {
            DatabaseKind::Sqlite => "SQLite",
            DatabaseKind::MySql => "MySql",
            DatabaseKind::SqlServer => "SqlServer",
            DatabaseKind::PostgreSql => "PostgreSQL",
            // 国产 PG 系：**返回品牌名**（不是 `PostgreSQL`）—— C# `DbClient` 按它反序列化 `DatabaseType`，
            // 归并成 PostgreSQL 会让远端丢失品牌信息
            DatabaseKind::KingBase => "KingBase",
            DatabaseKind::HighGo => "HighGo",
            DatabaseKind::VastBase => "VastBase",
            DatabaseKind::Oracle => "Oracle",
            DatabaseKind::DuckDb => "DuckDB",
            DatabaseKind::Firebird => "Firebird",
            DatabaseKind::ClickHouse => "ClickHouse",
            DatabaseKind::TDengine => "TDengine",
            DatabaseKind::InfluxDb => "InfluxDB",
            DatabaseKind::Hana => "Hana",
            DatabaseKind::MongoDb => "MongoDB",
            DatabaseKind::Db2 => "DB2",
            DatabaseKind::DaMeng => "DaMeng",
            DatabaseKind::Iris => "IRIS",
            DatabaseKind::Access => "Access",
        }
    }

    /// 是否为文件型数据库。
    pub fn is_file_based(&self) -> bool {
        matches!(
            self,
            DatabaseKind::Sqlite
                | DatabaseKind::DuckDb
                | DatabaseKind::Firebird
                | DatabaseKind::Access
        )
    }

    /// 是否支持关系式 DDL（建表/加列）。
    ///
    /// InfluxDB 的 measurement 写入时自动创建；MongoDB 的 collection 亦然。
    pub fn supports_ddl(&self) -> bool {
        !matches!(self, DatabaseKind::InfluxDb | DatabaseKind::MongoDb)
    }

    /// 大文本阈值：字符串列长度超过该值即视为“无长度意义的大文本”。
    ///
    /// 对应 C# `IDatabase.LongTextLength`（默认 4000；Access 255、Firebird 32767、
    /// InfluxDB 65535）。结构比对时两侧都是大文本则忽略长度差异（对齐 `IsColumnLengthChanged`）。
    /// <returns>阈值字符数</returns>
    pub fn long_text_length(&self) -> i32 {
        match self {
            DatabaseKind::Access => 255,
            DatabaseKind::Firebird => 32_767,
            DatabaseKind::InfluxDb => 65_535,
            _ => 4_000,
        }
    }

    /// 是否支持标准多行 `VALUES (..),(..)` 批量插入语法。
    ///
    /// 对齐 C# 各数据库会话的 `BatchCapability.Insert`（`IDbSession.Insert(table, columns, list)` 覆写）；
    /// C# 部分库走参数数组绑定（Oracle/新驱动 MySQL），本库对标准 SQL 引擎生成多行 VALUES，
    /// 其余数据库由 [`crate::dal::TableRef::insert_batch`] 回退逐行执行（语义一致，仅少一次往返优化）。
    pub fn supports_multi_row_insert(&self) -> bool {
        matches!(
            self,
            DatabaseKind::Sqlite
                | DatabaseKind::MySql
                | DatabaseKind::PostgreSql
                | DatabaseKind::KingBase
                | DatabaseKind::HighGo
                | DatabaseKind::VastBase
                | DatabaseKind::SqlServer
                | DatabaseKind::DuckDb
        )
    }

    /// 单条语句允许的最大绑定参数个数（批量插入分块用；`0` = 不设上限）。
    ///
    /// - SQLite：`SQLITE_MAX_VARIABLE_NUMBER`（3.32+ 默认 32766）
    /// - SQL Server：单语句最多 2100 个参数
    /// - MySQL / PostgreSQL：按协议上限 65535 保守取值
    pub fn max_statement_params(&self) -> usize {
        match self {
            DatabaseKind::Sqlite | DatabaseKind::DuckDb => 32_766,
            DatabaseKind::SqlServer => 2_100,
            DatabaseKind::MySql
            | DatabaseKind::PostgreSql
            | DatabaseKind::KingBase
            | DatabaseKind::HighGo
            | DatabaseKind::VastBase => 65_535,
            _ => 0,
        }
    }

    /// 是否支持多行 `Upsert`（SQLite/DuckDB/PostgreSQL `On Conflict Do Update`、MySQL `On Duplicate Key Update`）。
    ///
    /// 不支持时由 [`crate::dal::TableRef::write_batch`] 的 Upsert 模式回退逐行（查主键后更新/插入）。
    pub fn supports_multi_upsert(&self) -> bool {
        matches!(
            self,
            DatabaseKind::Sqlite
                | DatabaseKind::MySql
                | DatabaseKind::PostgreSql
                | DatabaseKind::KingBase
                | DatabaseKind::HighGo
                | DatabaseKind::VastBase
                | DatabaseKind::DuckDb
        )
    }

    /// 从连接串中的 `provider` 名称解析（兼容常见别名）。
    ///
    /// 与 DH.NCode 支持的库对应关系：
    /// - HighGo（瀚高）/KingBase（金仓）/VastBase（海量）与 PostgreSQL 同协议，驱动直接复用
    /// - NovaDb 为 MySQL 系协议（端口 3306/反引号/`LAST_INSERT_ID()`），复用 MySQL 驱动
    /// - `network`（XCode 远程服务协议）由 `Dal::open`/`crate::network` 处理（类型需登录远端探明）；
    ///   `sqlce`（已停更的 SQL Server Compact 运行时）不在数据库驱动范畴
    pub fn from_provider(name: &str) -> Result<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "sqlite" | "sqlite3" | "system.data.sqlite" | "microsoft.data.sqlite" => {
                Ok(DatabaseKind::Sqlite)
            }
            "mysql" | "mariadb" | "system.data.mysql" => Ok(DatabaseKind::MySql),
            // NovaDb：NewLife 自研库，连接串/语法/自增均与 MySQL 一致
            "novadb" | "nova" => Ok(DatabaseKind::MySql),
            "sqlserver" | "mssql" | "system.data.sqlclient" | "microsoft.data.sqlclient" => {
                Ok(DatabaseKind::SqlServer)
            }
            "postgresql" | "postgres" | "pgsql" | "npgsql" => Ok(DatabaseKind::PostgreSql),
            // 国产 PG 系（金仓/瀚高/海量）：**返回各自的品牌变体**（不是 `PostgreSql`）——
            // 方言行为委托 PG 族（`is_postgres_family`），但 `name()` 要能回传品牌名
            "highgo" => Ok(DatabaseKind::HighGo),
            "kingbase" | "kingbasees" => Ok(DatabaseKind::KingBase),
            "vastbase" => Ok(DatabaseKind::VastBase),
            "oracle" | "system.data.oracleclient" | "oracle.manageddataaccess" => {
                Ok(DatabaseKind::Oracle)
            }
            "duckdb" => Ok(DatabaseKind::DuckDb),
            "firebird" | "fb" => Ok(DatabaseKind::Firebird),
            "clickhouse" | "clickhouse.client" => Ok(DatabaseKind::ClickHouse),
            "tdengine" | "td" => Ok(DatabaseKind::TDengine),
            "influxdb" | "influx" => Ok(DatabaseKind::InfluxDb),
            "hana" | "sap" => Ok(DatabaseKind::Hana),
            "mongodb" | "mongo" => Ok(DatabaseKind::MongoDb),
            "db2" => Ok(DatabaseKind::Db2),
            "dameng" | "dm" => Ok(DatabaseKind::DaMeng),
            "iris" | "iris.data.irisclient" => Ok(DatabaseKind::Iris),
            "access" | "microsoft.jet.oledb" | "oledb" | "ace" => Ok(DatabaseKind::Access),
            // network 为远程驱动：类型需登录远端探明，由 `Dal::open` 优先处理（见 crate::network）
            "network" | "net" => Err(Error::Unsupported(
                "provider=network 的类型需登录远端探明：请通过 Dal::open 打开连接串（见 crate::network）"
                    .into(),
            )),
            "sqlce" => Err(Error::Unsupported(
                "provider=sqlce（SQL Server Compact）依赖已停止维护的 SSCE 原生运行时，Rust 生态无可用驱动；\
                 建议迁移到 SQLite（Pek.RCode 内嵌支持）"
                    .into(),
            )),
            other => Err(Error::Unsupported(format!(
                "未知的数据库类型 provider={other}\n支持：sqlite/mysql(nova)/sqlserver/postgresql(highgo/kingbase/vastbase)/oracle/\
                 duckdb/firebird/clickhouse/tdengine/influxdb/hana/mongodb/db2/dameng/iris/access"
            ))),
        }
    }

    /// 由远端的类型名解析（兼容 C# `DatabaseType` 枚举名与 Rust `DatabaseKind` 调试名）。
    ///
    /// 用于 `provider=network`：登录远端后按其返回的数据库类型名确定本地方言。
    /// 国产/衍生库归并到协议族：KingBase/HighGo/VastBase → PostgreSQL，NovaDb → MySQL。
    /// <param name="name">远端类型名（如 `SQLite`/`MySql`/`PostgreSQL`/`Sqlite`）</param>
    /// <returns>数据库类型</returns>
    pub fn from_remote_name(name: &str) -> Result<Self> {
        let lower = name.trim().to_ascii_lowercase();

        // 兼容 C# `DatabaseType` 枚举的数值序列（NewLife JSON 可能序列化为数字）：
        // 数字 → 枚举名的对照走 [`DATABASE_TYPES`] 单一来源，避免两处清单漂移
        if let Ok(code) = lower.parse::<i32>() {
            let Some(name) = database_type_name(code) else {
                return Err(Error::Unsupported(format!(
                    "未知的远端数据库类型编号：{code}"
                )));
            };
            return Self::from_remote_name(name);
        }

        match lower.as_str() {
            "sqlite" => Ok(DatabaseKind::Sqlite),
            "mysql" | "mariadb" | "novadb" | "nova" => Ok(DatabaseKind::MySql),
            "sqlserver" | "mssql" => Ok(DatabaseKind::SqlServer),
            "oracle" => Ok(DatabaseKind::Oracle),
            "postgresql" | "postgres" => Ok(DatabaseKind::PostgreSql),
            // 国产 PG 系：远端报品牌名时**保留品牌变体**（方言行为委托 PG 族）
            "kingbase" | "kingbasees" => Ok(DatabaseKind::KingBase),
            "highgo" => Ok(DatabaseKind::HighGo),
            "vastbase" => Ok(DatabaseKind::VastBase),
            "duckdb" => Ok(DatabaseKind::DuckDb),
            "firebird" | "fb" => Ok(DatabaseKind::Firebird),
            "clickhouse" => Ok(DatabaseKind::ClickHouse),
            "tdengine" => Ok(DatabaseKind::TDengine),
            "influxdb" => Ok(DatabaseKind::InfluxDb),
            "hana" => Ok(DatabaseKind::Hana),
            "db2" => Ok(DatabaseKind::Db2),
            "dameng" | "da_meng" => Ok(DatabaseKind::DaMeng),
            "iris" => Ok(DatabaseKind::Iris),
            "access" => Ok(DatabaseKind::Access),
            "mongodb" | "mongo" => Ok(DatabaseKind::MongoDb),
            "network" | "net" => Err(Error::Unsupported(
                "远端类型仍为 network（不支持多级转发）".into(),
            )),
            "sqlce" => Err(Error::Unsupported(
                "远端为 SqlCe：SSCE 运行时已停更，本端无法使用".into(),
            )),
            other => Err(Error::Unsupported(format!("未知的远端数据库类型：{other}"))),
        }
    }

    /// 引用标识符（表名/列名）。
    pub fn quote(&self, ident: &str) -> String {
        // 内部引号统一双写转义，避免标识符注入
        match self {
            DatabaseKind::MySql | DatabaseKind::ClickHouse | DatabaseKind::TDengine => {
                format!("`{}`", ident.replace('`', "``"))
            }
            DatabaseKind::SqlServer | DatabaseKind::Access => {
                format!("[{}]", ident.replace(']', "]]"))
            }
            _ => format!("\"{}\"", ident.replace('"', "\"\"")),
        }
    }

    /// 参数占位符（`index` 从 0 开始）。
    ///
    /// Oracle 使用 1 基的 `:1/:2/...`（OCI 位置绑定，与 rust oracle 驱动一致）；
    /// HTTP/REST 型数据库（ClickHouse/TDengine/InfluxDB/MongoDB）由驱动把参数内联为字面量。
    pub fn placeholder(&self, index: usize) -> String {
        match self {
            DatabaseKind::SqlServer => format!("@p{index}"),
            DatabaseKind::PostgreSql
            | DatabaseKind::KingBase
            | DatabaseKind::HighGo
            | DatabaseKind::VastBase => format!("${}", index + 1),
            DatabaseKind::Oracle => format!(":{}", index + 1),
            _ => "?".to_string(),
        }
    }

    /// 把标识符按方言引用后**追加**到 `out`（与 [`DatabaseKind::quote`] 完全等价，但不分配中间 String）。
    ///
    /// 供 SQL 拼接热路径使用（如 CAS 的 `expected` 条件）：每列省一次堆分配。
    pub(crate) fn push_quoted(&self, out: &mut String, ident: &str) {
        let (open, close, escape) = match self {
            DatabaseKind::MySql | DatabaseKind::ClickHouse | DatabaseKind::TDengine => {
                ('`', '`', '`')
            }
            DatabaseKind::SqlServer | DatabaseKind::Access => ('[', ']', ']'),
            _ => ('"', '"', '"'),
        };
        out.push(open);
        for ch in ident.chars() {
            if ch == escape {
                out.push(escape);
            }
            out.push(ch);
        }
        out.push(close);
    }

    /// 把参数占位符**追加**到 `out`（与 [`DatabaseKind::placeholder`] 完全等价，不分配中间 String）。
    pub(crate) fn push_placeholder(&self, out: &mut String, index: usize) {
        use std::fmt::Write as _;
        match self {
            DatabaseKind::SqlServer => {
                out.push_str("@p");
                let _ = write!(out, "{index}");
            }
            DatabaseKind::PostgreSql
            | DatabaseKind::KingBase
            | DatabaseKind::HighGo
            | DatabaseKind::VastBase => {
                out.push('$');
                let _ = write!(out, "{}", index + 1);
            }
            DatabaseKind::Oracle => {
                out.push(':');
                let _ = write!(out, "{}", index + 1);
            }
            _ => out.push('?'),
        }
    }

    /// 自增列的列内联修饰（`None` 表示该方言不用内联写法，由类型改写/序列/特殊规则处理）。
    fn identity_clause(&self, _col: &ColumnMeta) -> Option<String> {
        match self {
            DatabaseKind::Sqlite => None, // INTEGER PRIMARY KEY AUTOINCREMENT，创建时特殊处理
            DatabaseKind::MySql => Some(" AUTO_INCREMENT".into()),
            DatabaseKind::SqlServer => Some(" IDENTITY(1,1)".into()),
            // PostgreSQL / 国产 PG 系：serial/serial8 是伪类型，需要替换列类型（见 identity_type）
            DatabaseKind::PostgreSql
            | DatabaseKind::KingBase
            | DatabaseKind::HighGo
            | DatabaseKind::VastBase => None,
            // Oracle/DB2/Firebird 自增由独立序列承担（XCode 约定 SEQ_{表名}），列本身无内联属性
            DatabaseKind::Oracle | DatabaseKind::Db2 | DatabaseKind::Firebird => None,
            // DuckDB：序列 + DEFAULT nextval（与 PostgreSQL 函数同源）
            DatabaseKind::DuckDb => None,
            DatabaseKind::Hana => Some(" GENERATED BY DEFAULT AS IDENTITY".into()),
            DatabaseKind::DaMeng => Some(" IDENTITY(1,1)".into()),
            DatabaseKind::Iris => Some(" IDENTITY".into()),
            // Access：自增列类型为 COUNTER（见 identity_type）
            DatabaseKind::Access => None,
            // 列式/时序/文档库无自增主键概念（与 DH.NCode 一致：Identity 回写返回 0）
            DatabaseKind::ClickHouse
            | DatabaseKind::TDengine
            | DatabaseKind::InfluxDb
            | DatabaseKind::MongoDb => None,
        }
    }

    /// 自增列的列类型改写（`serial`/`COUNTER` 等伪类型需整体替换原类型）。
    fn identity_type(&self, col: &ColumnMeta) -> Option<String> {
        match self {
            // 与 DH.NCode 对齐：PostgreSQL 系自增使用 serial（Int32）/serial8（Int64）
            DatabaseKind::PostgreSql
            | DatabaseKind::KingBase
            | DatabaseKind::HighGo
            | DatabaseKind::VastBase
                if col.identity && col.data_type == DataType::Int64 =>
            {
                Some("serial8".into())
            }
            DatabaseKind::PostgreSql
            | DatabaseKind::KingBase
            | DatabaseKind::HighGo
            | DatabaseKind::VastBase
                if col.identity =>
            {
                Some("serial".into())
            }
            // Access 自增为 COUNTER（32 位）；Int64 自增在 ACE 中降级为 BIGINT
            DatabaseKind::Access if col.identity && col.data_type == DataType::Int64 => None,
            DatabaseKind::Access if col.identity => Some("COUNTER".into()),
            _ => None,
        }
    }

    /// 查询“最近一次自增 ID”的语句（对应 XCode 的自增回写）。
    ///
    /// 说明：驱动实现已在插入后直接回读（PostgreSQL/DuckDB 系 `RETURNING`、
    /// SQL Server `SCOPE_IDENTITY()`、Oracle/DB2/Firebird 按表名推导序列）；
    /// 本方法保留用于脚本导出与诊断输出。
    pub fn last_identity_sql(&self) -> &'static str {
        match self {
            DatabaseKind::Sqlite => "SELECT last_insert_rowid()",
            DatabaseKind::MySql => "SELECT LAST_INSERT_ID()",
            DatabaseKind::SqlServer => "SELECT SCOPE_IDENTITY()",
            DatabaseKind::PostgreSql => "SELECT lastval()",
            // 国产 PG 系：协议与 PostgreSQL 一致
            DatabaseKind::KingBase | DatabaseKind::HighGo | DatabaseKind::VastBase => {
                "SELECT lastval()"
            }
            DatabaseKind::Oracle => "SELECT \"SEQ_<表名>\".CURRVAL FROM DUAL",
            DatabaseKind::DuckDb => "SELECT currval('<SEQ_表名>')",
            DatabaseKind::Firebird => "SELECT GEN_ID(\"SEQ_<表名>\", 0) FROM RDB$DATABASE",
            DatabaseKind::Hana => "SELECT CURRENT_IDENTITY_VALUE() FROM DUMMY",
            DatabaseKind::Db2 => "SELECT SEQ_<表名>.CURRVAL FROM dual",
            DatabaseKind::DaMeng => "SELECT @@IDENTITY",
            DatabaseKind::Iris => "SELECT LAST_IDENTITY()",
            DatabaseKind::Access => "SELECT @@IDENTITY",
            DatabaseKind::ClickHouse
            | DatabaseKind::TDengine
            | DatabaseKind::InfluxDb
            | DatabaseKind::MongoDb => "（无自增主键）",
        }
    }

    /// 字段类型映射（CLR 类型 → 数据库字段类型）。
    pub fn field_type(&self, col: &ColumnMeta) -> String {
        let t = col.data_type;
        match self {
            DatabaseKind::Sqlite => match t {
                DataType::Boolean => "bit".into(),
                DataType::Byte => "tinyint".into(),
                DataType::Int16 => "smallint".into(),
                DataType::Int32 => {
                    if col.identity {
                        "integer".into()
                    } else {
                        "int".into()
                    }
                }
                DataType::Int64 => "integer".into(),
                DataType::Single => "single".into(),
                DataType::Double => "real".into(),
                DataType::Decimal => "decimal".into(),
                DataType::String => {
                    if col.length > 0 {
                        format!("nvarchar({})", col.length)
                    } else {
                        "text".into()
                    }
                }
                DataType::DateTime => "datetime".into(),
                DataType::Binary => "binary".into(),
                // SQLite 无 JSON 类型；声明成 `json` 会得到 NUMERIC 亲和度而把 JSON 数字文字转成数字，
                // 故一律存 TEXT（JSON1/JSONB 函数对 TEXT 同样生效）
                DataType::Json => "text".into(),
            },
            DatabaseKind::MySql => match t {
                // 与 DH.NCode 对齐：MySQL 布尔用 TINYINT（读取时 0/1 → bool）
                DataType::Boolean => "TINYINT".into(),
                DataType::Byte => "tinyint".into(),
                DataType::Int16 => "smallint".into(),
                DataType::Int32 => "int".into(),
                DataType::Int64 => "bigint".into(),
                DataType::Single => "float".into(),
                DataType::Double => "double".into(),
                DataType::Decimal => {
                    let (p, s) = mysql_decimal_spec(col);
                    format!("decimal({p},{s})")
                }
                DataType::String => {
                    if col.length > 0 && col.length <= 16383 {
                        format!("varchar({})", col.length)
                    } else {
                        "longtext".into()
                    }
                }
                DataType::DateTime => datetime_spec("datetime", col.scale),
                DataType::Binary => "longblob".into(),
                // 5.7.8+ 原生 JSON：紧凑二进制存储 + 服务端合法性校验
                DataType::Json => "JSON".into(),
            },
            DatabaseKind::SqlServer => match t {
                DataType::Boolean => "bit".into(),
                DataType::Byte => "tinyint".into(),
                DataType::Int16 => "smallint".into(),
                DataType::Int32 => "int".into(),
                DataType::Int64 => "bigint".into(),
                DataType::Single => "real".into(),
                DataType::Double => "float".into(),
                DataType::Decimal => format!("decimal({},{})", col.precision, col.scale),
                DataType::String => {
                    if col.length <= 0 || col.length > 4000 {
                        "nvarchar(max)".into()
                    } else {
                        format!("nvarchar({})", col.length)
                    }
                }
                DataType::DateTime => datetime_spec_mssql(col.scale),
                DataType::Binary => "varbinary(max)".into(),
                // SQL Server 2016+ 以 nvarchar(max) + ISJSON 承载 JSON（2025 才有原生 json 类型）
                DataType::Json => "nvarchar(max)".into(),
            },
            // PostgreSQL 与国产 PG 系（金仓/瀚高/海量）类型映射一致
            DatabaseKind::PostgreSql
            | DatabaseKind::KingBase
            | DatabaseKind::HighGo
            | DatabaseKind::VastBase => match t {
                DataType::Boolean => "boolean".into(),
                DataType::Byte | DataType::Int16 => "smallint".into(),
                DataType::Int32 => "integer".into(),
                DataType::Int64 => "bigint".into(),
                DataType::Single => "real".into(),
                DataType::Double => "double precision".into(),
                DataType::Decimal => format!("numeric({},{})", col.precision, col.scale),
                DataType::String => {
                    if col.length > 0 {
                        format!("varchar({})", col.length)
                    } else {
                        "text".into()
                    }
                }
                DataType::DateTime => datetime_spec("timestamp", col.scale),
                DataType::Binary => "bytea".into(),
                // jsonb：二进制存储 + 去重键 + 可索引 / 可用操作符查询
                DataType::Json => "jsonb".into(),
            },
            DatabaseKind::Oracle => match t {
                DataType::Boolean => "number(1)".into(),
                DataType::Byte => "number(3)".into(),
                DataType::Int16 => "number(5)".into(),
                DataType::Int32 => "number(10)".into(),
                DataType::Int64 => "number(19)".into(),
                DataType::Single => "binary_float".into(),
                DataType::Double => "binary_double".into(),
                DataType::Decimal => format!("number({},{})", col.precision, col.scale),
                DataType::String => {
                    if col.length <= 0 || col.length > 2000 {
                        "clob".into()
                    } else {
                        format!("varchar2({})", col.length)
                    }
                }
                DataType::DateTime => "timestamp".into(),
                DataType::Binary => "blob".into(),
                // 在 Oracle 21c 之前无原生 JSON 类型；CLOB 兼容面最广
                DataType::Json => "clob".into(),
            },
            DatabaseKind::DuckDb => match t {
                DataType::Boolean => "BOOLEAN".into(),
                DataType::Byte | DataType::Int16 => "SMALLINT".into(),
                DataType::Int32 => "INTEGER".into(),
                DataType::Int64 => "BIGINT".into(),
                DataType::Single => "REAL".into(),
                DataType::Double => "DOUBLE".into(),
                DataType::Decimal => format!("DECIMAL({},{})", col.precision, col.scale),
                DataType::String => {
                    if col.length > 0 {
                        format!("VARCHAR({})", col.length)
                    } else {
                        "TEXT".into()
                    }
                }
                DataType::DateTime => "TIMESTAMP".into(),
                DataType::Binary => "BLOB".into(),
                DataType::Json => "JSON".into(),
            },
            DatabaseKind::Firebird => match t {
                // 与 DH.NCode 对齐：Firebird 无布尔类型，用 SMALLINT 承载
                DataType::Boolean => "SMALLINT".into(),
                DataType::Byte | DataType::Int16 => "SMALLINT".into(),
                DataType::Int32 => "INTEGER".into(),
                DataType::Int64 => "BIGINT".into(),
                DataType::Single => "FLOAT".into(),
                DataType::Double => "DOUBLE PRECISION".into(),
                DataType::Decimal => format!("DECIMAL({},{})", col.precision, col.scale),
                DataType::String => {
                    if col.length > 0 && col.length <= 32767 {
                        format!("VARCHAR({})", col.length)
                    } else {
                        "BLOB SUB_TYPE TEXT".into()
                    }
                }
                DataType::DateTime => "TIMESTAMP".into(),
                DataType::Binary => "BLOB".into(),
                DataType::Json => "BLOB SUB_TYPE TEXT".into(),
            },
            DatabaseKind::ClickHouse => match t {
                DataType::Boolean | DataType::Byte => "UInt8".into(),
                DataType::Int16 => "Int16".into(),
                DataType::Int32 => "Int32".into(),
                DataType::Int64 => "Int64".into(),
                DataType::Single => "Float32".into(),
                DataType::Double => "Float64".into(),
                DataType::Decimal => format!("Decimal({},{})", col.precision, col.scale),
                // ClickHouse 字符串无长度参数
                DataType::String | DataType::Binary | DataType::Json => "String".into(),
                DataType::DateTime => "DateTime64(6)".into(),
            },
            DatabaseKind::TDengine => match t {
                DataType::Boolean => "BOOL".into(),
                DataType::Byte => "TINYINT".into(),
                DataType::Int16 => "SMALLINT".into(),
                DataType::Int32 => "INT".into(),
                DataType::Int64 => "BIGINT".into(),
                DataType::Single => "FLOAT".into(),
                DataType::Double => "DOUBLE".into(),
                DataType::Decimal => {
                    format!("DECIMAL({},{})", col.precision.max(1), col.scale.max(0))
                }
                DataType::String => {
                    if col.length > 0 && col.length <= 16374 {
                        format!("VARCHAR({})", col.length)
                    } else {
                        "TEXT".into()
                    }
                }
                DataType::DateTime => "TIMESTAMP".into(),
                DataType::Binary => "BLOB".into(),
                DataType::Json => "TEXT".into(),
            },
            DatabaseKind::InfluxDb => match t {
                DataType::Boolean => "BOOLEAN".into(),
                DataType::Byte | DataType::Int16 | DataType::Int32 | DataType::Int64 => {
                    "INTEGER".into()
                }
                DataType::Single | DataType::Double | DataType::Decimal => "FLOAT".into(),
                DataType::String => "STRING".into(),
                DataType::DateTime => "TIMESTAMP".into(),
                DataType::Binary => "BINARY".into(),
                DataType::Json => "STRING".into(),
            },
            DatabaseKind::Hana => match t {
                DataType::Boolean => "BOOLEAN".into(),
                DataType::Byte => "TINYINT".into(),
                DataType::Int16 => "SMALLINT".into(),
                DataType::Int32 => "INTEGER".into(),
                DataType::Int64 => "BIGINT".into(),
                DataType::Single => "REAL".into(),
                DataType::Double => "DOUBLE".into(),
                DataType::Decimal => format!("DECIMAL({},{})", col.precision, col.scale),
                DataType::String => {
                    if col.length > 0 && col.length <= 5000 {
                        format!("NVARCHAR({})", col.length)
                    } else {
                        "NCLOB".into()
                    }
                }
                DataType::DateTime => "TIMESTAMP".into(),
                DataType::Binary => {
                    if col.length > 0 && col.length <= 5000 {
                        format!("VARBINARY({})", col.length)
                    } else {
                        "BLOB".into()
                    }
                }
                DataType::Json => "NCLOB".into(),
            },
            // 文档型：返回 BSON 类型名（供模型导出/诊断参考）
            DatabaseKind::MongoDb => match t {
                DataType::Boolean => "bool".into(),
                DataType::Byte | DataType::Int16 | DataType::Int32 => "int".into(),
                DataType::Int64 => "long".into(),
                DataType::Single | DataType::Double => "double".into(),
                DataType::Decimal => "decimal".into(),
                DataType::String => "string".into(),
                DataType::DateTime => "date".into(),
                DataType::Binary => "binData".into(),
                DataType::Json => "object".into(),
            },
            // 与 DH.NCode 对齐：DB2 采用 Oracle 兼容模式（NUMBER/BINARY_FLOAT/To_Date）
            DatabaseKind::Db2 => match t {
                DataType::Boolean => "NUMBER(1,0)".into(),
                DataType::Byte => "NUMBER(1,0)".into(),
                DataType::Int16 => "NUMBER(5,0)".into(),
                DataType::Int32 => "NUMBER(10,0)".into(),
                DataType::Int64 => "NUMBER(20,0)".into(),
                DataType::Single => "BINARY_FLOAT".into(),
                DataType::Double => "BINARY_DOUBLE".into(),
                DataType::Decimal => format!("NUMBER({},{})", col.precision, col.scale),
                DataType::String => {
                    if col.length <= 0 || col.length > 4000 {
                        "CLOB".into()
                    } else {
                        format!("VARCHAR2({})", col.length)
                    }
                }
                DataType::DateTime => "TIMESTAMP".into(),
                DataType::Binary => "BLOB".into(),
                DataType::Json => "CLOB".into(),
            },
            // 与 DH.NCode 对齐：达梦类型（BIT/TINYINT/DEC/DATETIME/BLOB）
            DatabaseKind::DaMeng => match t {
                DataType::Boolean => "BIT".into(),
                DataType::Byte => "TINYINT".into(),
                DataType::Int16 => "SMALLINT".into(),
                DataType::Int32 => "INT".into(),
                DataType::Int64 => "BIGINT".into(),
                DataType::Single => "REAL".into(),
                DataType::Double => "DOUBLE".into(),
                DataType::Decimal => format!("DEC({},{})", col.precision, col.scale),
                DataType::String => {
                    if col.length <= 0 || col.length > 8188 {
                        "CLOB".into()
                    } else {
                        format!("VARCHAR({})", col.length)
                    }
                }
                DataType::DateTime => "DATETIME".into(),
                DataType::Binary => "BLOB".into(),
                DataType::Json => "CLOB".into(),
            },
            // 与 DH.NCode 对齐：IRIS 类型表（布尔用 TINYINT 承载）
            DatabaseKind::Iris => match t {
                DataType::Boolean => "TINYINT".into(),
                DataType::Byte => "TINYINT".into(),
                DataType::Int16 => "SMALLINT".into(),
                DataType::Int32 => "INT".into(),
                DataType::Int64 => "BIGINT".into(),
                DataType::Single => "FLOAT".into(),
                DataType::Double => "DOUBLE".into(),
                DataType::Decimal => format!("DECIMAL({},{})", col.precision, col.scale),
                DataType::String => {
                    if col.length > 0 && col.length <= 4000 {
                        format!("VARCHAR({})", col.length)
                    } else {
                        "LONGVARCHAR".into()
                    }
                }
                DataType::DateTime => "DATETIME".into(),
                DataType::Binary => "BLOB".into(),
                DataType::Json => "LONGVARCHAR".into(),
            },
            DatabaseKind::Access => match t {
                DataType::Boolean => "BIT".into(),
                DataType::Byte => "BYTE".into(),
                DataType::Int16 => "SMALLINT".into(),
                DataType::Int32 => "LONG".into(),
                DataType::Int64 => "BIGINT".into(),
                DataType::Single => "SINGLE".into(),
                DataType::Double => "DOUBLE".into(),
                DataType::Decimal => format!("DECIMAL({},{})", col.precision, col.scale),
                DataType::String => {
                    if col.length > 0 && col.length <= 255 {
                        format!("TEXT({})", col.length)
                    } else {
                        "MEMO".into()
                    }
                }
                DataType::DateTime => "DATETIME".into(),
                DataType::Binary => "BINARY".into(),
                DataType::Json => "MEMO".into(),
            },
        }
    }

    /// 索引名（未显式指定时按 `ix_{表名}_{列名}` 编制，与 DH.NCode 的建表约定一致）。
    /// <param name="table">表</param>
    /// <param name="idx">索引定义</param>
    /// <returns>索引名</returns>
    pub fn index_name(&self, table: &TableMeta, idx: &IndexMeta) -> String {
        idx.name.clone().unwrap_or_else(|| {
            format!(
                "ix_{}_{}",
                table.effective_table_name(),
                idx.columns.join("_")
            )
        })
    }

    /// 生成索引创建语句（`None` 表示索引无列，可忽略）。
    ///
    /// **MySQL 特殊处理**：`TEXT`/`BLOB` 系列列（模型里 `String` 无 `Length`
    /// 或超长、`Binary`）必须带前缀长度才能建索引（否则 `ERROR 1170 BLOB/TEXT column used
    /// in key specification without a key length`）。这里自动补 `(191)`：
    /// utf8mb4 下 191 字符 = 764 字节，兼容老版本 767 字节索引前缀上限。
    /// <param name="table">表</param>
    /// <param name="idx">索引定义</param>
    /// <returns>CREATE INDEX 语句</returns>
    pub fn create_index_sql(&self, table: &TableMeta, idx: &IndexMeta) -> Option<String> {
        if idx.columns.is_empty() {
            return None;
        }
        let cols: Vec<String> = idx
            .columns
            .iter()
            .map(|c| match table.column(c) {
                Some(col) => {
                    let name = self.quote(table.effective_column_name(col));
                    if *self == DatabaseKind::MySql && mysql_needs_index_prefix(col) {
                        format!("{name}({MYSQL_INDEX_PREFIX_CHARS})")
                    } else {
                        name
                    }
                }
                None => self.quote(c),
            })
            .collect();
        Some(format!(
            "CREATE {}INDEX {} ON {} ({})",
            if idx.unique { "UNIQUE " } else { "" },
            self.quote(&self.index_name(table, idx)),
            self.quote(table.effective_table_name()),
            cols.join(", ")
        ))
    }

    /// 生成删除索引的语句（`Full` 档专用；`None` 表示该数据库不支持独立删索引）。
    /// <param name="index_name">索引名</param>
    /// <param name="table_name">所属表（MySQL/SQL Server 语法需要）</param>
    /// <returns>DROP INDEX 语句（与 DH.NCode 各驱动 `DropIndexSQL` 对齐）</returns>
    pub fn drop_index_sql(&self, index_name: &str, table_name: Option<&str>) -> Option<String> {
        let name = self.quote(index_name);
        match self {
            // SQL Server：`Drop Index 表.索引`
            DatabaseKind::SqlServer => {
                table_name.map(|t| format!("Drop Index {}.{name}", self.quote(t)))
            }
            // MySQL：`Drop Index 索引 On 表`
            DatabaseKind::MySql => {
                table_name.map(|t| format!("Drop Index {name} On {}", self.quote(t)))
            }
            // 列式/时序库的索引为表内定义：不支持独立删索引
            DatabaseKind::ClickHouse | DatabaseKind::TDengine => None,
            _ => Some(format!("Drop Index {name}")),
        }
    }

    /// 生成删除列的语句（`Full` 档专用；`None` 表示该数据库不支持直接删除列）。
    /// <param name="table_name">表名</param>
    /// <param name="column_name">列名</param>
    /// <returns>DROP COLUMN 语句（与 DH.NCode 各驱动 `DropColumnSQL` 对齐）</returns>
    pub fn drop_column_sql(&self, table_name: &str, column_name: &str) -> Option<String> {
        let t = self.quote(table_name);
        let c = self.quote(column_name);
        match self {
            // 文档库与 Access（JET/ACE）不支持列级删除
            DatabaseKind::MongoDb | DatabaseKind::InfluxDb | DatabaseKind::Access => None,
            // 其余统一 `Alter Table 表 Drop Column 列`
            _ => Some(format!("Alter Table {t} Drop Column {c}")),
        }
    }

    /// 生成删除表的语句（自动迁移**从不**调用——模型外的表不删，与 XCode 相同；供人工/工具使用）。
    /// <param name="table_name">表名</param>
    /// <returns>DROP TABLE 语句</returns>
    pub fn drop_table_sql(&self, table_name: &str) -> String {
        format!("Drop Table {}", self.quote(table_name))
    }

    /// 生成修改列类型的语句（`Full` 档专用；`None` 表示该数据库不支持直接改类型——如 SQLite 需重建表）。
    /// <param name="table">表</param>
    /// <param name="col">列</param>
    /// <returns>ALTER COLUMN 语句（与 DH.NCode 各驱动 `AlterColumnSQL` 对齐）</returns>
    pub fn alter_column_sql(&self, table: &TableMeta, col: &ColumnMeta) -> Option<String> {
        // 历史行为保持不变：SQL Server 写模型可空性，其余方言不写可空性子句。
        // 自动执行的结构同步请改用 [`DatabaseKind::alter_column_sql_keep`]（保留库端现状）。
        let nullable = match self {
            DatabaseKind::SqlServer => Some(col.nullable),
            _ => None,
        };
        self.alter_column_sql_keep(table, col, nullable, false)
    }

    /// 生成修改列类型的语句，**并按库端现状保留可空性与自增属性**（结构同步自动执行时使用）。
    ///
    /// 必要性：MySQL 的 `MODIFY COLUMN` 与 SQL Server 的 `ALTER COLUMN` 都要求**显式重复**
    /// 可空性，未写即按可空重建列——只改类型会把原来的 `NOT NULL` 悄悄丢掉（MySQL 还会丢掉
    /// `AUTO_INCREMENT`）。这里传入的是**数据库当前值**而非模型值，语义是「只改类型、其余不动」。
    ///
    /// 注意：本函数**不处理默认值**。MySQL 上要连 `DEFAULT` 与 `ON UPDATE CURRENT_TIMESTAMP`
    /// 一起保留，请改用 [`DatabaseKind::alter_column_sql_keep_full`]。
    /// <param name="table">表</param>
    /// <param name="col">模型列（提供目标类型）</param>
    /// <param name="keep_nullable">库端当前可空性（`None` = 不写该子句）</param>
    /// <param name="keep_identity">库端当前是否自增（MySQL 需重复 `AUTO_INCREMENT`）</param>
    /// <returns>ALTER 语句；`None` 表示该数据库不支持直接改列</returns>
    pub fn alter_column_sql_keep(
        &self,
        table: &TableMeta,
        col: &ColumnMeta,
        keep_nullable: Option<bool>,
        keep_identity: bool,
    ) -> Option<String> {
        let t = self.quote(table.effective_table_name());
        let c = self.quote(table.effective_column_name(col));
        let ty = self.field_type(col);
        let null_sql = match keep_nullable {
            Some(true) => " NULL",
            Some(false) => " NOT NULL",
            None => "",
        };
        match self {
            // SQL Server：Alter Column（必须重复可空性）
            DatabaseKind::SqlServer => {
                Some(format!("Alter Table {t} Alter Column {c} {ty}{null_sql}"))
            }
            // Oracle / 达梦 / DB2（Oracle 兼容模式）：Modify 列定义（默认值/可空性由库保留）
            DatabaseKind::Oracle | DatabaseKind::DaMeng | DatabaseKind::Db2 => {
                Some(format!("Alter Table {t} Modify {c} {ty}"))
            }
            // MySQL：Modify Column（不写 NULL/NOT NULL 即变成可空，必须显式带上）
            DatabaseKind::MySql => {
                let auto = if keep_identity { " AUTO_INCREMENT" } else { "" };
                Some(format!(
                    "Alter Table {t} Modify Column {c} {ty}{null_sql}{auto}"
                ))
            }
            // HANA / IRIS / TDengine / ClickHouse：Modify Column（自增语义各异，不追加）
            DatabaseKind::Hana
            | DatabaseKind::Iris
            | DatabaseKind::TDengine
            | DatabaseKind::ClickHouse => {
                Some(format!("Alter Table {t} Modify Column {c} {ty}{null_sql}"))
            }
            // PG 系（含 KingBase/VastBase/HighGo/DuckDB）：ALTER COLUMN .. TYPE ..
            // （PG 改类型不动可空性与默认值，无需重复）
            DatabaseKind::PostgreSql
            | DatabaseKind::KingBase
            | DatabaseKind::HighGo
            | DatabaseKind::VastBase
            | DatabaseKind::DuckDb => {
                Some(format!("ALTER TABLE {t} ALTER COLUMN {c} TYPE {ty}"))
            }
            // SQLite（需重建表）、Firebird、Access、文档库：不支持直接改列
            _ => None,
        }
    }

    /// 同 [`DatabaseKind::alter_column_sql_keep`]，但**把库端的默认值与 `ON UPDATE` 一并保留**。
    ///
    /// # 为什么需要
    ///
    /// MySQL 的 `MODIFY COLUMN` 会把没写出来的 `DEFAULT` 和 `ON UPDATE CURRENT_TIMESTAMP`
    /// 一起丢掉——所以早期实现遇到「有默认值的列」就直接跳过、只给人工 SQL，
    /// 于是「列加宽」这类安全变更也会被卡住。既然 `information_schema.COLUMNS` 的
    /// `COLUMN_DEFAULT` 与 `EXTRA` 都能读到，就应该照原样拼回去，让它**能自动执行**。
    ///
    /// # 参数
    /// - `keep_default`：库端 `COLUMN_DEFAULT` 原文（`None` = 该列无默认值）
    /// - `default_is_expression`：默认值是**表达式**（MySQL 8 的 `EXTRA` 含 `DEFAULT_GENERATED`）。
    ///   为真时**原样拼接不加引号**（如 `CURRENT_TIMESTAMP`、`(uuid())`）；为假按列类型渲染字面量
    /// - `keep_on_update`：库端 `EXTRA` 含 `on update CURRENT_TIMESTAMP`
    ///
    /// 只有 MySQL 需要重复默认值（其它方言改列本就保留默认值），其余方言直接返回
    /// [`DatabaseKind::alter_column_sql_keep`] 的结果。
    #[allow(clippy::too_many_arguments)]
    pub fn alter_column_sql_keep_full(
        &self,
        table: &TableMeta,
        col: &ColumnMeta,
        keep_nullable: Option<bool>,
        keep_identity: bool,
        keep_default: Option<&str>,
        default_is_expression: bool,
        keep_on_update: bool,
    ) -> Option<String> {
        let mut sql = self.alter_column_sql_keep(table, col, keep_nullable, keep_identity)?;
        if *self != DatabaseKind::MySql {
            return Some(sql);
        }
        if let Some(default) = keep_default.map(str::trim).filter(|d| !d.is_empty()) {
            let rendered = if default_is_expression {
                // 表达式默认值（CURRENT_TIMESTAMP / (uuid()) / (CURRENT_DATE + INTERVAL 1 DAY)）
                // 不能再加引号，否则语义从「表达式」变成「固定字符串」
                default.to_string()
            } else {
                render_default(self, default, col)
            };
            sql.push_str(&format!(" DEFAULT {rendered}"));
        }
        if keep_on_update {
            sql.push_str(" ON UPDATE CURRENT_TIMESTAMP");
        }
        Some(sql)
    }

    /// 生成建表语句（表 + 索引）。
    ///
    /// 说明：
    /// - 主键：SQLite 自增列内联为主键；其余数据库使用表级 `PRIMARY KEY (...)`
    /// - `Nullable` 缺省 false → 追加 `NOT NULL`（与 XCode 一致）
    /// - SQLite 字符串列追加 `COLLATE NOCASE`（与 XCode 保持一致，保证大小写不敏感检索）
    pub fn create_table_sql(&self, table: &TableMeta) -> Vec<String> {
        // 时序/文档库无建表 DDL（measurement/collection 写入时自动创建）
        if !self.supports_ddl() {
            return Vec::new();
        }

        let tname = table.effective_table_name();
        let mut sql = format!("CREATE TABLE {} (\n", self.quote(tname));

        let mut lines: Vec<String> = Vec::with_capacity(table.columns.len() + 1);
        let mut inline_pk = false;

        // TDengine 3.x 要求首列必须是 TIMESTAMP：把第一个时间列提到最前
        let ordered_columns: Vec<&ColumnMeta> = if *self == DatabaseKind::TDengine {
            let mut cols: Vec<&ColumnMeta> = table.columns.iter().collect();
            if let Some(pos) = cols.iter().position(|c| c.data_type == DataType::DateTime) {
                let first = cols.remove(pos);
                cols.insert(0, first);
            }
            cols
        } else {
            table.columns.iter().collect()
        };

        for col in &ordered_columns {
            let cname = table.effective_column_name(col);
            // 自增列在部分方言中需要改写类型（PostgreSQL 的 serial/serial8）
            let type_name = self
                .identity_type(col)
                .unwrap_or_else(|| self.field_type(col));
            let mut line = format!("  {} {}", self.quote(cname), type_name);

            if col.identity {
                match self {
                    DatabaseKind::Sqlite => {
                        // SQLite 自增必须是 INTEGER PRIMARY KEY AUTOINCREMENT
                        line.push_str(" PRIMARY KEY AUTOINCREMENT");
                        inline_pk = true;
                    }
                    _ => {
                        if let Some(extra) = self.identity_clause(col) {
                            line.push_str(&extra);
                        }
                    }
                }
            }

            // 默认值：Oracle 系（Oracle/DB2）要求 DEFAULT 出现在 NOT NULL 之前
            let mut default_part = column_default_sql(self, col).map(|d| format!(" DEFAULT {d}"));
            // DuckDB 自增：序列 + DEFAULT nextval（插入时由列默认值生成）
            if *self == DatabaseKind::DuckDb && col.identity && default_part.is_none() {
                // 序列名以“带双引号的字符串”传入（与 CREATE SEQUENCE 的大小写一致）
                default_part = Some(format!(
                    " DEFAULT nextval('{}')",
                    self.quote(&oracle_identity_sequence(tname))
                ));
            }
            let not_null = !col.nullable && !(col.identity && inline_pk);
            if matches!(self, DatabaseKind::Oracle | DatabaseKind::Db2) {
                if let Some(part) = &default_part {
                    line.push_str(part);
                }
                if not_null {
                    line.push_str(" NOT NULL");
                }
            } else {
                if not_null {
                    line.push_str(" NOT NULL");
                }
                if let Some(part) = &default_part {
                    line.push_str(part);
                }
            }

            if self == &DatabaseKind::Sqlite && col.data_type == DataType::String {
                line.push_str(" COLLATE NOCASE");
            }

            // MySQL 特有：把字段说明写成列注释（与 DH.NCode 的 COMMENT 行为一致）
            if self == &DatabaseKind::MySql && !col.description.is_empty() {
                line.push_str(&format!(
                    " COMMENT '{}'",
                    escape_mysql_text(&col.description)
                ));
            }

            lines.push(line);
        }

        // 表级主键（SQLite 自增列除外）
        if !inline_pk {
            let pk: Vec<String> = table
                .columns
                .iter()
                .filter(|c| c.primary_key)
                .map(|c| self.quote(table.effective_column_name(c)))
                .collect();
            if !pk.is_empty() {
                lines.push(format!("  PRIMARY KEY ({})", pk.join(", ")));
            }
        }

        sql.push_str(&lines.join(",\n"));
        sql.push_str("\n)");

        // ClickHouse 建表必须指定表引擎
        if *self == DatabaseKind::ClickHouse {
            sql.push_str(" ENGINE = MergeTree() ORDER BY tuple()");
        }

        let mut statements = vec![sql];

        // 索引
        for idx in &table.indexes {
            if let Some(sql) = self.create_index_sql(table, idx) {
                statements.push(sql);
            }
        }

        // 独立序列（XCode 约定 SEQ_{表名}）：Oracle/DB2/Firebird/DuckDB 的自增回写依赖它
        if table.identity().is_some() {
            let sequence = oracle_identity_sequence(tname);
            match self {
                DatabaseKind::Oracle => statements.push(format!(
                    "CREATE SEQUENCE {} START WITH 1 INCREMENT BY 1 CACHE 20",
                    self.quote(&sequence)
                )),
                // DB2（Oracle 兼容模式）：序列名不加引号（未引号折为大写）
                DatabaseKind::Db2 => statements.push(format!(
                    "CREATE SEQUENCE {sequence} START WITH 1 INCREMENT BY 1"
                )),
                DatabaseKind::Firebird => {
                    statements.push(format!("CREATE SEQUENCE {}", self.quote(&sequence)))
                }
                // DuckDB 的 DEFAULT nextval 在建表时即引用序列，需先创建（插到表之前）
                DatabaseKind::DuckDb => {
                    statements.insert(0, format!("CREATE SEQUENCE {}", self.quote(&sequence)))
                }
                _ => {}
            }
        }

        statements
    }

    /// 生成“新增列”的 ALTER 语句。
    ///
    /// 安全策略：目标列为 NOT NULL 且没有默认值时，降级为可空列，
    /// 避免历史数据无法通过约束检查（与 XCode 迁移时的做法一致）。
    pub fn add_column_sql(&self, table: &TableMeta, col: &ColumnMeta) -> String {
        let tname = self.quote(table.effective_table_name());
        let cname = self.quote(table.effective_column_name(col));
        // 自增列在部分方言中需要改写类型（PostgreSQL 的 serial/serial8、Access 的 COUNTER）
        let type_name = self
            .identity_type(col)
            .unwrap_or_else(|| self.field_type(col));
        // SQL Server / Oracle 的 ADD 子句不带 COLUMN 关键字；HANA 要求加括号
        let mut sql = match self {
            DatabaseKind::SqlServer | DatabaseKind::Oracle => {
                format!("ALTER TABLE {tname} ADD {cname} {type_name}")
            }
            DatabaseKind::Hana => format!("ALTER TABLE {tname} ADD ({cname} {type_name})"),
            _ => format!("ALTER TABLE {tname} ADD COLUMN {cname} {type_name}"),
        };

        let default_sql = column_default_sql(self, col);
        if col.identity
            && let Some(extra) = self.identity_clause(col)
        {
            sql.push_str(&extra);
        }

        // 无默认值时不追加 NOT NULL：存量数据无法满足约束
        let not_null = !col.nullable && default_sql.is_some();
        if matches!(self, DatabaseKind::Oracle | DatabaseKind::Db2) {
            if let Some(d) = &default_sql {
                sql.push_str(&format!(" DEFAULT {d}"));
            }
            if not_null {
                sql.push_str(" NOT NULL");
            }
        } else {
            if not_null {
                sql.push_str(" NOT NULL");
            }
            if let Some(d) = &default_sql {
                sql.push_str(&format!(" DEFAULT {d}"));
            }
        }
        if self == &DatabaseKind::MySql && !col.description.is_empty() {
            sql.push_str(&format!(
                " COMMENT '{}'",
                escape_mysql_text(&col.description)
            ));
        }
        sql
    }

    /// 拼装分页：`sql` 为不含排序与分页的查询；`order_sql` 为完整的 `ORDER BY ...`（可空）。
    ///
    /// - SQLite / MySQL / PostgreSQL 系：`LIMIT size OFFSET offset`
    /// - SQL Server：`OFFSET n ROWS FETCH NEXT m ROWS ONLY`（无排序时补 `ORDER BY (SELECT NULL)`）
    /// - Oracle：ROWNUM 双层包装（兼容 11g，与 DH.NCode 的分页思路一致）
    pub fn apply_paging(&self, sql: &str, order_sql: &str, offset: usize, size: usize) -> String {
        use std::fmt::Write as _;
        // 一次预分配：原 SQL + 排序 + 分页后缀（`format!` 的二次分配已消除）
        let mut s = String::with_capacity(sql.len() + order_sql.len() + 48);
        match self {
            DatabaseKind::Sqlite
            | DatabaseKind::MySql
            | DatabaseKind::PostgreSql
            | DatabaseKind::KingBase
            | DatabaseKind::HighGo
            | DatabaseKind::VastBase
            | DatabaseKind::DuckDb
            | DatabaseKind::ClickHouse
            | DatabaseKind::TDengine
            | DatabaseKind::InfluxDb
            | DatabaseKind::Hana
            | DatabaseKind::DaMeng
            | DatabaseKind::Iris
            // 文档库：跳过/限制由驱动翻译器解析 SQL 文本中的 LIMIT/OFFSET
            | DatabaseKind::MongoDb => {
                s.push_str(sql);
                if !order_sql.is_empty() {
                    s.push(' ');
                    s.push_str(order_sql);
                }
                let _ = write!(s, " LIMIT {size} OFFSET {offset}");
                s
            }
            DatabaseKind::SqlServer | DatabaseKind::Db2 => {
                s.push_str(sql);
                if order_sql.is_empty() {
                    // SQL Server 分页要求 ORDER BY，缺失时给出语法合法的兜底（DB2 同样接受）
                    s.push_str(" ORDER BY (SELECT NULL)");
                } else {
                    s.push(' ');
                    s.push_str(order_sql);
                }
                let _ = write!(s, " OFFSET {offset} ROWS FETCH NEXT {size} ROWS ONLY");
                s
            }
            DatabaseKind::Oracle => {
                let mut inner = String::with_capacity(sql.len() + order_sql.len() + 16);
                inner.push_str(sql);
                if !order_sql.is_empty() {
                    inner.push(' ');
                    inner.push_str(order_sql);
                }
                let upper = offset + size;
                let _ = write!(
                    s,
                    "SELECT * FROM (SELECT T0.*, ROWNUM AS rowNumber FROM ({inner}) T0) \
                     WHERE rowNumber > {offset} AND rowNumber <= {upper}"
                );
                s
            }
            // Firebird：ROWS a TO b（1 基，含两端）
            DatabaseKind::Firebird => {
                s.push_str(sql);
                if !order_sql.is_empty() {
                    s.push(' ');
                    s.push_str(order_sql);
                }
                let _ = write!(s, " ROWS {} TO {}", offset + 1, offset + size);
                s
            }
            // Access：无 OFFSET，用双层 TOP 实现（需要排序，无排序时由调用方保证主键兜底）
            DatabaseKind::Access => {
                if order_sql.is_empty() {
                    // 无排序无法保证双层 TOP 结果正确，退化为首页 TOP
                    return format!("SELECT TOP {size} * FROM ({sql}) AS T");
                }
                if offset == 0 {
                    format!("SELECT TOP {size} * FROM ({sql}) AS T {order_sql}")
                } else {
                    let reversed = reverse_order(order_sql);
                    let skip = offset + size;
                    format!(
                        "SELECT * FROM (SELECT TOP {size} * FROM (SELECT TOP {skip} * FROM ({sql}) AS T1 {order_sql}) AS T2 {reversed}) AS T3 {order_sql}"
                    )
                }
            }
        }
    }

    /// 按指定分页风格拼装（仅 SQL Server 的 [`PageStyle::RowNumber`] 与默认不同，其余库忽略风格）。
    /// <param name="sql">不含排序与分页的查询</param>
    /// <param name="order_sql">完整 `ORDER BY ...`（可空）</param>
    /// <param name="offset">跳过行数</param>
    /// <param name="size">每页行数</param>
    /// <param name="style">分页风格</param>
    /// <returns>分页 SQL</returns>
    pub fn apply_paging_with_style(
        &self,
        sql: &str,
        order_sql: &str,
        offset: usize,
        size: usize,
        style: PageStyle,
    ) -> String {
        if *self == DatabaseKind::SqlServer && style == PageStyle::RowNumber {
            return sqlserver_row_number_paging(sql, order_sql, offset, size);
        }
        self.apply_paging(sql, order_sql, offset, size)
    }
}

/// C# `DH.NCode` 的 `DatabaseType` **权威清单**（枚举值, 枚举名, 中文说明）。
///
/// # 为什么需要这张表
///
/// Rust 侧把国产/衍生库**按协议归并**了（见下表），枚举数量必然少于 C#。
/// 但「与 C# 互操作」不能因此丢品牌：`provider=network` 的远端会回传 C# 的枚举名或数值，
/// 解析对不对直接决定用哪套方言。把清单固化在这里，作为解析与校验的**单一来源**，
/// 避免像以前那样在 `from_remote_name` 里手写一份数字映射（两处清单早晚漂）。
///
/// # 与 [`DatabaseKind`] 的**有意差异**（要改先读这段）
///
/// | C# `DatabaseType` | Rust `DatabaseKind` | 差异原因 |
/// |---|---|---|
/// | `KingBase`(13) / `HighGo`(14) / `VastBase`(16) | 归并为 [`DatabaseKind::PostgreSql`] | 同 PG 协议；但 C# 是**独立实现类**，各有 SQL 差异（见下） |
/// | `NovaDb`(18) | 归并为 [`DatabaseKind::MySql`] | 连接串/语法/自增均与 MySQL 一致 |
/// | `SqlCe`(5) | 无（`from_provider` 明确报错） | SSCE 运行时已停更，Rust 生态无可用驱动 |
/// | （无） | 多出 [`DatabaseKind::Firebird`] | C# 里该枚举项被注释掉，但 `Firebird.cs` 实现仍在 |
///
/// **归并只影响方言选择，不影响品牌识别**：`from_remote_name` 仍认全品牌名。
/// ⚠️ 已知差距：C# 的 `HighGo`(474 行) / `KingBase`(612) / `VastBase`(1080) / `NovaDb`(509)
/// 都覆写了 `FormatValue` / `FormatLike` / `GetFieldType` / 索引与注释 SQL 等，
/// 归并到 PG/MySQL 后**这些品牌差异不会生效**。需要精确对齐某个国产品牌时，
/// 应按 C# 补独立变体，而不是继续往 PG 分支里加 `if provider == "HighGo"`。
pub const DATABASE_TYPES: [(i32, &str, &str); 22] = [
    (0, "None", "无效值"),
    (1, "Access", "Access 文件数据库"),
    (2, "SqlServer", "SqlServer 数据库"),
    (3, "Oracle", "Oracle 数据库"),
    (4, "MySql", "MySql 数据库"),
    (5, "SqlCe", "SqlCe 数据库"),
    (6, "SQLite", "SQLite 数据库"),
    (8, "PostgreSQL", "PostgreSQL 数据库"),
    (9, "DaMeng", "达梦数据库"),
    (10, "DB2", "DB2 数据库"),
    (11, "TDengine", "TDengine 数据库"),
    (12, "Hana", "Hana 数据库"),
    (13, "KingBase", "人大金仓(KingBase) 数据库"),
    (14, "HighGo", "瀚高(HighGo) 数据库"),
    (15, "IRIS", "InterSystems IRIS 数据库"),
    (16, "VastBase", "VastBase 数据库"),
    (17, "InfluxDB", "InfluxDB 时序数据库"),
    (18, "NovaDb", "NovaDb 数据库"),
    (19, "ClickHouse", "ClickHouse 列式分析数据库"),
    (20, "DuckDB", "DuckDB 嵌入式 OLAP 数据库"),
    (21, "MongoDB", "MongoDB 文档数据库"),
    (100, "Network", "网络虚拟数据库"),
];

/// 按 C# `DatabaseType` 枚举名（大小写不敏感）取枚举值；未知名返回 `None`。
///
/// 用于把「远端报来的类型名」原样转成 C# 数值（如日志/审计里记录远端真实品牌）。
/// <param name="name">枚举名，如 `KingBase` / `SQLite`</param>
pub fn database_type_code(name: &str) -> Option<i32> {
    let lower = name.trim().to_ascii_lowercase();
    DATABASE_TYPES
        .iter()
        .find(|(_, n, _)| n.to_ascii_lowercase() == lower)
        .map(|(code, _, _)| *code)
}

/// 按 C# `DatabaseType` 枚举值取枚举名；未知值返回 `None`。
pub fn database_type_name(code: i32) -> Option<&'static str> {
    DATABASE_TYPES
        .iter()
        .find(|(c, _, _)| *c == code)
        .map(|(_, n, _)| *n)
}

/// 分页风格（仅 SQL Server 区分；对应 C# `MSPageSplit`（2005/2008）与 2012+ 两套算法）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PageStyle {
    /// SQL Server 2012 及以上：`OFFSET n ROWS FETCH NEXT m ROWS ONLY`（DH.NCode 现行默认）
    #[default]
    OffsetFetch,
    /// SQL Server 2005/2008：`ROW_NUMBER() OVER(...)` 双层包装（对齐 `MSPageSplit.RowNumber`）
    RowNumber,
}

/// SQL Server 2005/2008 的 `ROW_NUMBER()` 分页（对齐 C# `MSPageSplit.RowNumber`）：
///
/// ```sql
/// SELECT * FROM (
///   SELECT *, row_number() over(Order By {排序}) as rowNumber FROM ({原查询}) AS XCode_T0
/// ) AS XCode_T1 WHERE rowNumber BETWEEN {offset+1} And {offset+size}
/// ```
///
/// 无排序时 `OVER(Order By (SELECT NULL))` 兜底（语法合法，不保证顺序，与调用方的主键兜底策略配合）。
fn sqlserver_row_number_paging(sql: &str, order_sql: &str, offset: usize, size: usize) -> String {
    let order_expr = order_sql
        .strip_prefix("ORDER BY ")
        .unwrap_or(order_sql)
        .trim();
    let order_expr = if order_expr.is_empty() {
        "(SELECT NULL)"
    } else {
        order_expr
    };
    let start = offset + 1;
    let end = offset + size;
    format!(
        "SELECT * FROM (SELECT *, row_number() over(Order By {order_expr}) as rowNumber FROM ({sql}) AS XCode_T0) AS XCode_T1 WHERE rowNumber BETWEEN {start} And {end}"
    )
}

/// Oracle 自增序列名（与 XCode 约定一致：`SEQ_{表名}`）。
pub fn oracle_identity_sequence(table_name: &str) -> String {
    format!("SEQ_{table_name}")
}

/// 列的默认值渲染（含 Oracle 隐式默认值，与 DH.NCode 行为一致）。
///
/// Oracle 为 NOT NULL 的 DateTime 列补 `To_Date('0001-01-01','yyyy-mm-dd')`，
/// 对应 DH.NCode `Oracle.GetDefault` 的处理。
fn column_default_sql(kind: &DatabaseKind, col: &ColumnMeta) -> Option<String> {
    if let Some(default) = &col.default_value
        && !default.is_empty()
    {
        return Some(render_default(kind, default, col));
    }
    if matches!(*kind, DatabaseKind::Oracle | DatabaseKind::Db2)
        && col.data_type == DataType::DateTime
        && !col.nullable
        && !col.identity
    {
        return Some("To_Date('0001-01-01','yyyy-mm-dd')".into());
    }
    None
}

/// 反转 `ORDER BY` 各排序项的方向（Access 双层 TOP 分页需要）。
fn reverse_order(order_sql: &str) -> String {
    let Some((prefix, terms)) = order_sql.split_at_checked("ORDER BY".len()) else {
        return order_sql.to_string();
    };
    let items: Vec<String> = terms
        .split(',')
        .map(|item| {
            let item = item.trim();
            if item.to_ascii_uppercase().ends_with(" DESC") {
                item[..item.len() - 5].trim().to_string()
            } else if item.to_ascii_uppercase().ends_with(" ASC") {
                format!("{} DESC", item[..item.len() - 4].trim())
            } else {
                format!("{item} DESC")
            }
        })
        .collect();
    format!("{prefix} {}", items.join(", "))
}

/// MySQL 索引前缀长度（utf8mb4 下 191 字符 = 764 字节，兼容 767 字节前缀上限）。
pub const MYSQL_INDEX_PREFIX_CHARS: i32 = 191;

/// 该列在 MySQL 里是不是 `TEXT`/`BLOB` 系列（建索引必须带前缀长度）。
///
/// 判定与 [`DatabaseKind::field_type`] 的 MySQL 分支保持一致：
/// `String` 且 `Length` 为 0（不限）或超 `16383` → `longtext`；`Binary` → `longblob`。
/// `Json` 不加前缀长度（`JSON(191)` 是非法语法；JSON 列本身只能通过生成列建索引）。
///
/// # 参数
/// - `col`：列定义。
///
/// # 返回值
/// 需要前缀长度时为 `true`。
fn mysql_needs_index_prefix(col: &ColumnMeta) -> bool {
    match col.data_type {
        DataType::String => col.length <= 0 || col.length > 16383,
        DataType::Binary => true,
        DataType::Json => false,
        _ => false,
    }
}

/// MySQL DECIMAL 精度规则（对齐 DH.NCode：Length 有值时覆盖 Precision，上限 255）。
fn mysql_decimal_spec(col: &ColumnMeta) -> (i32, i32) {
    let scale = col.scale.max(0);
    let mut precision = if col.length > 0 {
        col.length.min(255)
    } else if col.precision > 0 {
        col.precision
    } else {
        10
    };
    if precision <= scale {
        precision = scale + 1;
    }
    (precision, scale)
}

/// 日期时间类型 + 小数秒位数：`Scale` 1..=6 时生成 `datetime(3)` / `timestamp(3)`，否则保持无精度原类型。
///
/// 为什么需要：`ColumnMeta.scale` 一直有解析（C# 模型可写 `Scale="3"`，`sync-model.ps1` 也原样透传），
/// 但这里原来一律输出无精度的 `datetime` —— 毫秒在 MySQL 会被截断成秒（2026-10-30 审计时间要毫秒）。
/// **只对显式声明 `Scale>0` 的列生效**，现有模型（scale 默认 0）行为完全不变。
fn datetime_spec(base: &str, scale: i32) -> String {
    match scale {
        1..=6 => format!("{base}({scale})"),
        _ => base.to_string(),
    }
}

/// SQL Server：`Scale=0` 保持 `datetime`（历史类型），有小数位才换 `datetime2(s)`
/// （`datetime` 固定约 3.33ms 精度、且不支持 `datetime(n)` 写法）。
fn datetime_spec_mssql(scale: i32) -> String {
    match scale {
        1..=6 => format!("datetime2({scale})"),
        _ => "datetime".to_string(),
    }
}

/// MySQL 字符串字面量转义（COMMENT 等场景：先转义反斜杠，再双写单引号）。
fn escape_mysql_text(v: &str) -> String {
    v.replace('\\', "\\\\").replace('\'', "''")
}

/// 渲染默认值：数字/布尔直接内联；其余作为字符串字面量（已含引号的按原样处理）。
fn render_default(kind: &DatabaseKind, default: &str, col: &ColumnMeta) -> String {
    let trimmed = default.trim();
    if col.data_type.is_numeric() && trimmed.parse::<f64>().is_ok() {
        return trimmed.to_string();
    }
    if col.data_type == DataType::Boolean {
        return match trimmed.to_ascii_lowercase().as_str() {
            "true" | "1" => {
                if kind.is_postgres_family() || *kind == DatabaseKind::DuckDb {
                    "TRUE".into()
                } else {
                    "1".into()
                }
            }
            "false" | "0" => {
                if kind.is_postgres_family() || *kind == DatabaseKind::DuckDb {
                    "FALSE".into()
                } else {
                    "0".into()
                }
            }
            _ => "0".into(),
        };
    }
    // 字符串型默认值：已带引号时不重复包裹
    let literal = if (trimmed.starts_with('\'') && trimmed.ends_with('\''))
        || (trimmed.starts_with('"') && trimmed.ends_with('"'))
    {
        trimmed.to_string()
    } else {
        format!("'{}'", trimmed.replace('\'', "''"))
    };
    // MySQL：TEXT/BLOB 列不接受字面量默认值（ERROR 1101），必须写成表达式默认 `DEFAULT ('')`
    // （MySQL 8.0.13+ / MariaDB 10.2+ 支持；更老的版本会报语法错，属可接受的明确失败）
    if *kind == DatabaseKind::MySql && mysql_needs_index_prefix(col) {
        return format!("({literal})");
    }
    literal
}

/// 服务端版本信息（**启动时探测一次，进程内缓存复用**）。
///
/// 用途：同一数据库的 SQL 语法在不同版本上不同（如 MySQL 8.0.19+ 的 Upsert 行别名、
/// `VALUES()` 自 8.0.20 起废弃、MariaDB 不支持别名语法），而 `DatabaseKind` 只区分"哪种库"。
/// 版本探测只在打开连接时做一次（一次 `SELECT VERSION()`），后续所有 SQL 生成直接读缓存。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerInfo {
    /// `SELECT VERSION()` 的原始返回
    pub raw: String,
    /// 是否 MariaDB（版本串含 `MariaDB`）
    pub is_mariadb: bool,
    /// 主版本
    pub major: u32,
    /// 次版本
    pub minor: u32,
    /// 修订号
    pub patch: u32,
}

impl ServerInfo {
    /// 解析版本串（只取开头的 `主.次.修订`）。
    ///
    /// 实测形态都能解析：`8.0.32-0ubuntu0.22.04.2`、`5.7.44-log`、
    /// `10.11.6-MariaDB-1:10.11.6+maria~ubu2204`、`16.4 (Debian 16.4-1.pgdg120+1)`。
    ///
    /// # 参数
    /// - `raw`：`SELECT VERSION()` 的返回。
    ///
    /// # 返回值
    /// 版本信息（解析不出的段补 0）。
    pub fn parse(raw: &str) -> Self {
        let text = raw.trim();
        let is_mariadb = text.to_ascii_lowercase().contains("mariadb");

        // 取第一个「数字与点」组成的片段（跳过库名前缀，如 `MariaDB-10.11.6`）
        let first = text
            .split(|c: char| !(c.is_ascii_digit() || c == '.'))
            .find(|part| !part.is_empty())
            .unwrap_or("");
        let mut parts = first.split('.');
        let major = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        let minor = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        let patch = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);

        Self {
            raw: text.to_string(),
            is_mariadb,
            major,
            minor,
            patch,
        }
    }

    /// 版本是否 >= 给定的 `主.次.修订`。
    ///
    /// # 参数
    /// - `major`：主版本。
    /// - `minor`：次版本。
    /// - `patch`：修订号。
    ///
    /// # 返回值
    /// 达到或超过返回 `true`。
    pub fn at_least(&self, major: u32, minor: u32, patch: u32) -> bool {
        (self.major, self.minor, self.patch) >= (major, minor, patch)
    }

    /// 展示文本（日志/面板用）：`MySQL 8.0.32` / `MariaDB 10.11.6`。
    ///
    /// # 返回值
    /// 一行摘要。
    pub fn label(&self) -> String {
        let brand = if self.is_mariadb { "MariaDB" } else { "Server" };
        format!("{brand} {}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// 由服务端版本推出的 SQL 语法开关（**保守默认 = 全关**，即生成最兼容的写法）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SqlFeatures {
    /// MySQL `8.0.19+` 的 `INSERT ... VALUES (...) AS new ON DUPLICATE KEY UPDATE 列 = new.列`。
    ///
    /// - `VALUES(列)` 自 MySQL `8.0.20` 起被标记废弃（仍可用，但有 deprecation 警告，未来版本可能移除）；
    /// - **MariaDB 不支持**该别名语法（继续用 `VALUES()`）；
    /// - 版本未知/探测失败时保持 `false`（`VALUES()` 在所有 MySQL 5.x/8.x 上都可用）。
    pub upsert_row_alias: bool,
}

impl SqlFeatures {
    /// 按数据库类型与探测到的版本推导。
    ///
    /// # 参数
    /// - `kind`：数据库类型。
    /// - `info`：服务端版本（启动时探测）。
    ///
    /// # 返回值
    /// 特性开关（未知版本一律保守）。
    pub fn from_server(kind: DatabaseKind, info: &ServerInfo) -> Self {
        Self {
            upsert_row_alias: kind == DatabaseKind::MySql
                && !info.is_mariadb
                && info.at_least(8, 0, 19),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EntityModel, TableMeta};

    #[test]
    fn server_info_parses_common_version_strings() {
        let mysql = ServerInfo::parse("8.0.32-0ubuntu0.22.04.2");
        assert_eq!((mysql.major, mysql.minor, mysql.patch), (8, 0, 32));
        assert!(!mysql.is_mariadb);
        assert!(mysql.at_least(8, 0, 19), "8.0.32 支持行别名");

        let old = ServerInfo::parse("5.7.44-log");
        assert_eq!((old.major, old.minor, old.patch), (5, 7, 44));
        assert!(!old.at_least(8, 0, 19), "5.7 不支持行别名");

        let maria = ServerInfo::parse("10.11.6-MariaDB-1:10.11.6+maria~ubu2204");
        assert!(maria.is_mariadb);
        assert_eq!((maria.major, maria.minor, maria.patch), (10, 11, 6));

        let pg = ServerInfo::parse("16.4 (Debian 16.4-1.pgdg120+1)");
        assert_eq!(pg.major, 16);
    }

    #[test]
    fn sql_features_follow_server_version_and_brand() {
        // MySQL 8.0.19+：开别名
        let modern = ServerInfo::parse("8.0.19");
        assert!(
            SqlFeatures::from_server(DatabaseKind::MySql, &modern).upsert_row_alias,
            "8.0.19 起应使用行别名"
        );

        // MySQL 9.x：继续支持行别名（9.0 只移除了 mysql_native_password，未移除 upsert 语法）。
        // 9.7.4+ 的 `VERSION()` 仍返回三段兼容版本（四段只在 VERSION_WITH_UPDATE() 里），解析不受影响。
        let nine = ServerInfo::parse("9.0.1");
        assert_eq!((nine.major, nine.minor, nine.patch), (9, 0, 1));
        assert!(
            SqlFeatures::from_server(DatabaseKind::MySql, &nine).upsert_row_alias,
            "MySQL 9.x 应继续使用行别名"
        );
        let newer = ServerInfo::parse("9.7.4");
        assert!(
            SqlFeatures::from_server(DatabaseKind::MySql, &newer).upsert_row_alias,
            "9.7.x 同样支持行别名"
        );

        // MariaDB 不支持别名语法，即使版本号看着够新
        let maria = ServerInfo::parse("10.11.6-MariaDB");
        assert!(
            !SqlFeatures::from_server(DatabaseKind::MySql, &maria).upsert_row_alias,
            "MariaDB 不支持 AS 别名"
        );

        // 非 MySQL 方言不使用该开关
        let pg = ServerInfo::parse("16.4");
        assert!(!SqlFeatures::from_server(DatabaseKind::PostgreSql, &pg).upsert_row_alias);

        // 未探测 = 保守默认（全部关闭）
        assert!(!SqlFeatures::default().upsert_row_alias);
    }
    /// 追加式拼接必须与原 `quote` / `placeholder` 逐字节一致（热路径零分配优化的等价性护栏）。
    #[test]
    fn push_helpers_match_string_helpers() {
        let idents = ["Id", "a\"b", "a`b", "a]b", ""];
        for kind in [
            DatabaseKind::Sqlite,
            DatabaseKind::MySql,
            DatabaseKind::SqlServer,
            DatabaseKind::PostgreSql,
            DatabaseKind::Oracle,
        ] {
            for ident in idents {
                let mut out = String::new();
                kind.push_quoted(&mut out, ident);
                assert_eq!(out, kind.quote(ident), "{kind:?} 标识符引用不一致：{ident}");
            }
            for index in [0usize, 1, 9, 42] {
                let mut out = String::new();
                kind.push_placeholder(&mut out, index);
                assert_eq!(
                    out,
                    kind.placeholder(index),
                    "{kind:?} 占位符不一致：{index}"
                );
            }
        }
    }

    fn sample_table() -> TableMeta {
        let xml = r#"<EntityModel><Tables><Table Name="Order" TableName="DH_Order">
          <Columns>
            <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
            <Column Name="Code" DataType="String" Length="50" />
            <Column Name="Title" DataType="String" />
            <Column Name="Amount" DataType="Decimal" Precision="18" Scale="4" />
            <Column Name="Ok" DataType="Boolean" />
            <Column Name="CreateTime" DataType="DateTime" />
          </Columns>
        </Table></Tables></EntityModel>"#;
        EntityModel::parse(xml).unwrap().tables.remove(0)
    }

    /// `DateTime` 列的 `Scale` → 带小数秒的列类型（MySQL/PG 同名，SQL Server 换 `datetime2`）。
    ///
    /// 背景：模型一直能写 `Scale="3"`，但映射层原来忽略它 → 毫秒被 MySQL 截断成秒。
    /// 撤掉 `datetime_spec` 后本用例的 `datetime(3)` 断言会失败。
    #[test]
    fn datetime_scale_maps_to_fractional_type() {
        assert_eq!(datetime_spec("datetime", 0), "datetime", "未声明 Scale 时保持原类型");
        assert_eq!(datetime_spec("datetime", 3), "datetime(3)");
        assert_eq!(datetime_spec("timestamp", 6), "timestamp(6)");
        assert_eq!(datetime_spec("datetime", 9), "datetime", "超范围(>6)不生成非法 DDL");
        assert_eq!(datetime_spec_mssql(0), "datetime", "SQL Server 默认仍是 datetime");
        assert_eq!(datetime_spec_mssql(3), "datetime2(3)");
    }

    #[test]
    fn provider_parsing() {
        assert_eq!(
            DatabaseKind::from_provider("SQLite").unwrap(),
            DatabaseKind::Sqlite
        );
        assert_eq!(
            DatabaseKind::from_provider("mysql").unwrap(),
            DatabaseKind::MySql
        );
        assert_eq!(
            DatabaseKind::from_provider("SqlServer").unwrap(),
            DatabaseKind::SqlServer
        );
        assert_eq!(
            DatabaseKind::from_provider("postgresql").unwrap(),
            DatabaseKind::PostgreSql
        );
        assert_eq!(
            DatabaseKind::from_provider("dameng").unwrap(),
            DatabaseKind::DaMeng
        );
        assert_eq!(
            DatabaseKind::from_provider("nova").unwrap(),
            DatabaseKind::MySql
        );
        // 国产 PG 系：**保留品牌变体**（不再归并成 PostgreSql），方言行为仍委托 PG 族
        assert_eq!(
            DatabaseKind::from_provider("kingbase").unwrap(),
            DatabaseKind::KingBase
        );
        assert_eq!(
            DatabaseKind::from_provider("kingbasees").unwrap(),
            DatabaseKind::KingBase
        );
        assert_eq!(
            DatabaseKind::from_provider("highgo").unwrap(),
            DatabaseKind::HighGo
        );
        assert_eq!(
            DatabaseKind::from_provider("vastbase").unwrap(),
            DatabaseKind::VastBase
        );
        for p in ["postgresql", "kingbase", "highgo", "vastbase"] {
            let k = DatabaseKind::from_provider(p).unwrap();
            assert!(k.is_postgres_family(), "{p} 应属 PG 族");
            // 品牌保真：name() 必须回品牌名（C# DbClient 按它反序列化 DatabaseType）
            assert_ne!(k.name(), "PostgreSql", "{p} 的 name() 不能塌成 PostgreSql");
        }
        assert!(DatabaseKind::from_provider("network").is_err());
    }

    /// 国产 PG 系（金仓/瀚高/海量）的方言产物必须与 PostgreSQL **逐字节一致**。
    ///
    /// 背景：2026-11-01 给 `DatabaseKind` 补了这三个品牌变体（原来归并成 `PostgreSql`）。
    /// 加变体的真正风险不是编译不过，而是**散落的 `matches!(kind, PostgreSql | …)` 漏掉新变体**
    /// → 静默走兜底分支（如布尔默认值从 `TRUE` 退化成 `1`、插入后拿不到 `RETURNING` 的主键）。
    /// 本用例把「族内行为等同」钉住，以后漏改会直接红。
    #[test]
    fn postgres_family_brands_behave_like_postgres() {
        let table = sample_table();
        let code = table.column("Code").unwrap();
        let title = table.column("Title").unwrap();
        let pg = DatabaseKind::PostgreSql;

        for brand in [
            DatabaseKind::KingBase,
            DatabaseKind::HighGo,
            DatabaseKind::VastBase,
        ] {
            assert!(brand.is_postgres_family(), "{brand:?} 应属 PG 族");
            assert_eq!(brand.field_type(code), pg.field_type(code), "{brand:?} 类型映射");
            assert_eq!(brand.field_type(title), pg.field_type(title), "{brand:?} 大文本类型");
            assert_eq!(brand.placeholder(0), pg.placeholder(0), "{brand:?} 参数占位符");
            assert_eq!(
                brand.alter_column_sql(&table, code),
                pg.alter_column_sql(&table, code),
                "{brand:?} 改列语句"
            );
            assert_eq!(
                brand.last_identity_sql(),
                pg.last_identity_sql(),
                "{brand:?} 自增回读"
            );
            assert_eq!(
                brand.identity_clause(code),
                pg.identity_clause(code),
                "{brand:?} 自增修饰"
            );
            assert_eq!(
                brand.apply_paging("SELECT * FROM T", "ORDER BY \"Id\"", 20, 10),
                pg.apply_paging("SELECT * FROM T", "ORDER BY \"Id\"", 20, 10),
                "{brand:?} 分页"
            );
            assert_eq!(
                brand.max_statement_params(),
                pg.max_statement_params(),
                "{brand:?} 参数上限"
            );
            assert_eq!(
                brand.supports_multi_upsert(),
                pg.supports_multi_upsert(),
                "{brand:?} 多行 Upsert"
            );
            assert_eq!(
                brand.supports_multi_row_insert(),
                pg.supports_multi_row_insert(),
                "{brand:?} 多行插入"
            );
        }

        // 布尔默认值渲染也走 PG 分支（`TRUE` 而不是 `1`）——这条最容易被漏掉的 matches! 命中
        let mut ok = table.column("Ok").unwrap().clone();
        ok.default_value = Some("True".into());
        let mut t = table.clone();
        t.columns = vec![ok];
        for brand in [
            DatabaseKind::KingBase,
            DatabaseKind::HighGo,
            DatabaseKind::VastBase,
        ] {
            let sql = brand.create_table_sql(&t).remove(0);
            assert!(
                sql.contains("DEFAULT TRUE"),
                "{brand:?} 布尔默认值应渲染成 TRUE：{sql}"
            );
        }
    }

    #[test]
    fn quoting_and_placeholders() {
        assert_eq!(DatabaseKind::Sqlite.quote("DH_Order"), "\"DH_Order\"");
        assert_eq!(DatabaseKind::MySql.quote("DH_Order"), "`DH_Order`");
        assert_eq!(DatabaseKind::SqlServer.quote("DH_Order"), "[DH_Order]");
        // 标识符中的引号需要转义，避免注入
        assert_eq!(DatabaseKind::Sqlite.quote("a\"b"), "\"a\"\"b\"");

        assert_eq!(DatabaseKind::Sqlite.placeholder(0), "?");
        assert_eq!(DatabaseKind::SqlServer.placeholder(2), "@p2");
        assert_eq!(DatabaseKind::PostgreSql.placeholder(0), "$1");
        // Oracle 使用 1 基位置绑定（OCI）
        assert_eq!(DatabaseKind::Oracle.placeholder(0), ":1");
        assert_eq!(DatabaseKind::Oracle.placeholder(1), ":2");
    }

    #[test]
    fn field_type_mapping() {
        let table = sample_table();
        let id = table.column("Id").unwrap();
        let code = table.column("Code").unwrap();
        let title = table.column("Title").unwrap();
        let amount = table.column("Amount").unwrap();

        assert_eq!(DatabaseKind::Sqlite.field_type(id), "integer");
        assert_eq!(DatabaseKind::MySql.field_type(id), "int");
        assert_eq!(DatabaseKind::SqlServer.field_type(id), "int");

        assert_eq!(DatabaseKind::Sqlite.field_type(code), "nvarchar(50)");
        assert_eq!(DatabaseKind::Sqlite.field_type(title), "text");
        assert_eq!(DatabaseKind::MySql.field_type(code), "varchar(50)");
        assert_eq!(DatabaseKind::MySql.field_type(title), "longtext");
        assert_eq!(DatabaseKind::SqlServer.field_type(title), "nvarchar(max)");
        assert_eq!(DatabaseKind::PostgreSql.field_type(title), "text");

        assert_eq!(DatabaseKind::SqlServer.field_type(amount), "decimal(18,4)");
        assert_eq!(DatabaseKind::PostgreSql.field_type(amount), "numeric(18,4)");

        // 与 DH.NCode 对齐：MySQL 布尔为 TINYINT，SQLite 为 bit
        let ok = table.column("Ok").unwrap();
        assert_eq!(DatabaseKind::MySql.field_type(ok), "TINYINT");
        assert_eq!(DatabaseKind::Sqlite.field_type(ok), "bit");
    }

    #[test]
    fn drop_and_alter_sql_by_dialect() {
        let table = sample_table();
        let code = table.column("Code").unwrap();

        // 删除列（Full 档）：与 DH.NCode 各驱动 DropColumnSQL 对齐
        assert_eq!(
            DatabaseKind::Sqlite
                .drop_column_sql("DH_Order", "Extra")
                .unwrap(),
            "Alter Table \"DH_Order\" Drop Column \"Extra\""
        );
        assert_eq!(
            DatabaseKind::MySql
                .drop_column_sql("DH_Order", "Extra")
                .unwrap(),
            "Alter Table `DH_Order` Drop Column `Extra`"
        );
        // 文档库与 Access 不支持列级删除
        assert!(
            DatabaseKind::MongoDb
                .drop_column_sql("DH_Order", "Extra")
                .is_none()
        );
        assert!(
            DatabaseKind::Access
                .drop_column_sql("DH_Order", "Extra")
                .is_none()
        );

        // 删除索引（Full 档）：SQL Server 用 `Drop Index 表.索引`；MySQL 带 On；其余独立语句
        assert_eq!(
            DatabaseKind::SqlServer
                .drop_index_sql("ix_a", Some("DH_Order"))
                .unwrap(),
            "Drop Index [DH_Order].[ix_a]"
        );
        assert_eq!(
            DatabaseKind::MySql
                .drop_index_sql("ix_a", Some("DH_Order"))
                .unwrap(),
            "Drop Index `ix_a` On `DH_Order`"
        );
        assert_eq!(
            DatabaseKind::Sqlite.drop_index_sql("ix_a", None).unwrap(),
            "Drop Index \"ix_a\""
        );
        assert_eq!(
            DatabaseKind::PostgreSql
                .drop_index_sql("ix_a", Some("DH_Order"))
                .unwrap(),
            "Drop Index \"ix_a\""
        );
        assert!(
            DatabaseKind::ClickHouse
                .drop_index_sql("ix_a", None)
                .is_none()
        );

        // 删除表（供人工/工具使用；自动迁移从不调用）
        assert_eq!(
            DatabaseKind::Sqlite.drop_table_sql("DH_Order"),
            "Drop Table \"DH_Order\""
        );

        // 修改列类型（Full 档）：四组方言与 DH.NCode 各驱动 AlterColumnSQL 对齐
        assert_eq!(
            DatabaseKind::MySql.alter_column_sql(&table, code).unwrap(),
            "Alter Table `DH_Order` Modify Column `Code` varchar(50)"
        );
        assert_eq!(
            DatabaseKind::SqlServer
                .alter_column_sql(&table, code)
                .unwrap(),
            "Alter Table [DH_Order] Alter Column [Code] nvarchar(50) NOT NULL"
        );
        assert_eq!(
            DatabaseKind::PostgreSql
                .alter_column_sql(&table, code)
                .unwrap(),
            "ALTER TABLE \"DH_Order\" ALTER COLUMN \"Code\" TYPE varchar(50)"
        );
        assert_eq!(
            DatabaseKind::Oracle.alter_column_sql(&table, code).unwrap(),
            "Alter Table \"DH_Order\" Modify \"Code\" varchar2(50)"
        );
        assert_eq!(
            DatabaseKind::Db2.alter_column_sql(&table, code).unwrap(),
            "Alter Table \"DH_Order\" Modify \"Code\" VARCHAR2(50)"
        );
        // SQLite 不支持直接改列类型（需重建表）——由人工处理
        assert!(
            DatabaseKind::Sqlite
                .alter_column_sql(&table, code)
                .is_none()
        );
        // Firebird 亦不支持（C# 端 AlterColumnSQL 已注释）
        assert!(
            DatabaseKind::Firebird
                .alter_column_sql(&table, code)
                .is_none()
        );
    }

    #[test]
    fn mysql_ddl_comment_and_decimal_rules() {
        let xml = r#"<EntityModel><Tables><Table Name="T" TableName="T">
          <Columns>
            <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
            <Column Name="Amount" DataType="Decimal" Length="12" Scale="4" Description="金额合计" />
            <Column Name="Ok" DataType="Boolean" Description="是否有效" />
          </Columns>
        </Table></Tables></EntityModel>"#;
        let table = EntityModel::parse(xml).unwrap().tables.remove(0);

        let sql = DatabaseKind::MySql.create_table_sql(&table).remove(0);
        assert!(
            sql.contains("`Amount` decimal(12,4)"),
            "Length 应覆盖 Precision：{sql}"
        );
        assert!(sql.contains("COMMENT '金额合计'"), "{sql}");
        assert!(sql.contains("`Ok` TINYINT"), "{sql}");

        // 其它数据库不生成 MySQL 风格的列注释
        let sqlite = DatabaseKind::Sqlite.create_table_sql(&table).remove(0);
        assert!(!sqlite.contains("COMMENT"), "{sqlite}");
    }

    #[test]
    fn create_table_sqlite_matches_xcode_conventions() {
        let table = sample_table();
        let stmts = DatabaseKind::Sqlite.create_table_sql(&table);
        let sql = &stmts[0];

        assert!(
            sql.contains("\"Id\" integer PRIMARY KEY AUTOINCREMENT"),
            "{sql}"
        );
        assert!(
            sql.contains("\"Code\" nvarchar(50) NOT NULL COLLATE NOCASE"),
            "{sql}"
        );
        assert!(
            sql.contains("\"Title\" text NOT NULL COLLATE NOCASE"),
            "{sql}"
        );
        assert!(sql.contains("\"Amount\" decimal NOT NULL"), "{sql}");
        assert!(sql.contains("\"Ok\" bit NOT NULL"), "{sql}");
        assert!(sql.contains("\"CreateTime\" datetime NOT NULL"), "{sql}");
        // 自增列已内联主键，不再生成表级主键
        assert!(!sql.contains("PRIMARY KEY ("), "{sql}");
    }

    #[test]
    fn create_table_sqlserver_has_table_level_pk_and_identity() {
        let table = sample_table();
        let stmts = DatabaseKind::SqlServer.create_table_sql(&table);
        let sql = &stmts[0];
        assert!(sql.contains("[Id] int IDENTITY(1,1) NOT NULL"), "{sql}");
        assert!(sql.contains("PRIMARY KEY ([Id])"), "{sql}");
    }

    #[test]
    fn index_sql_generation() {
        let mut table = sample_table();
        table.indexes.push(crate::model::IndexMeta {
            name: None,
            columns: vec!["Code".into()],
            unique: true,
        });
        let stmts = DatabaseKind::Sqlite.create_table_sql(&table);
        assert_eq!(stmts.len(), 2);
        assert_eq!(
            stmts[1],
            "CREATE UNIQUE INDEX \"ix_DH_Order_Code\" ON \"DH_Order\" (\"Code\")"
        );
    }

    /// MySQL：`TEXT`/`BLOB` 列建索引必须带前缀长度（否则 ERROR 1170）。
    ///
    /// 场景来自真实模型：`String` 不写 `Length` → `longtext`，
    /// 而模型里又有 `<Index Columns="SessionId" Unique="True" />`。
    #[test]
    fn mysql_text_column_index_uses_prefix_length() {
        let mut table = sample_table();
        // Code：Length=50 → varchar(50)，不需要前缀
        // Title：String 无长度 → longtext，需要前缀
        table.indexes.push(crate::model::IndexMeta {
            name: None,
            columns: vec!["Code".into()],
            unique: true,
        });
        table.indexes.push(crate::model::IndexMeta {
            name: None,
            columns: vec!["Title".into()],
            unique: true,
        });
        let stmts = DatabaseKind::MySql.create_table_sql(&table);
        let joined = stmts.join("\n");
        assert!(joined.contains("(`Code`)"), "变长列不应带前缀：{joined}");
        assert!(
            joined.contains("(`Title`(191))"),
            "longtext 列必须带前缀长度：{joined}"
        );
        // 其它方言不加前缀
        let sqlite = DatabaseKind::Sqlite.create_table_sql(&table).join("\n");
        assert!(sqlite.contains("(\"Title\")"), "{sqlite}");
    }

    /// MySQL：`TEXT`/`BLOB` 列不接受字面量默认值（ERROR 1101），要写成表达式默认。
    #[test]
    fn mysql_text_column_default_uses_expression() {
        let mut col = ColumnMeta {
            name: "UpdateUser".into(),
            column_name: None,
            data_type: DataType::String,
            raw_type: None,
            length: 0,
            precision: 0,
            scale: 0,
            identity: false,
            primary_key: false,
            master: false,
            nullable: false,
            default_value: Some("''".into()),
            description: String::new(),
            enum_type: None,
            data_scale: None,
            map: None,
            show_in: None,
            model: None,
            category: None,
            item_type: None,
            using_cache: None,
            extra: Vec::new(),
        };
        let mysql = DatabaseKind::MySql.add_column_sql(&sample_table(), &col);
        assert!(
            mysql.contains("DEFAULT ('')") || mysql.to_uppercase().contains("DEFAULT ('')"),
            "longtext 默认值必须写成表达式：{mysql}"
        );
        // 有长度的变长列仍是普通字面量默认值
        col.length = 50;
        let mysql = DatabaseKind::MySql.add_column_sql(&sample_table(), &col);
        assert!(mysql.contains("DEFAULT ''"), "{mysql}");
        assert!(!mysql.contains("('')"), "变长列不应包成表达式：{mysql}");
    }

    #[test]
    fn mysql_alter_full_keeps_default_and_on_update() {
        // 目标：MySQL 改列时**默认值与 ON UPDATE 都要照原样拼回去**，
        // 这样「有默认值的列」也能随 `sync_schema` 自动加宽，而不是一律交人工。
        const M: &str = r#"<EntityModel><Tables><Table Name="T" TableName="DH_T">
          <Columns>
            <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" Nullable="False" />
            <Column Name="Code" DataType="String" Length="100" Nullable="False" />
            <Column Name="Updated" DataType="DateTime" Nullable="False" />
          </Columns>
        </Table></Tables></EntityModel>"#;
        let model = EntityModel::parse(M).unwrap();
        let table = model.table("T").unwrap();
        let code = table.column("Code").unwrap();
        let updated = table.column("Updated").unwrap();

        // ① 字面量默认值：按列类型渲染（字符串加引号），可空性照样保留
        let sql = DatabaseKind::MySql
            .alter_column_sql_keep_full(table, code, Some(false), false, Some("abc"), false, false)
            .expect("MySQL 应支持改列");
        assert!(sql.contains("NOT NULL"), "{sql}");
        assert!(sql.contains("DEFAULT 'abc'"), "{sql}");
        assert!(!sql.contains("ON UPDATE"), "{sql}");

        // ② 表达式默认值 + ON UPDATE：原样拼、绝不能加引号
        let sql = DatabaseKind::MySql
            .alter_column_sql_keep_full(
                table,
                updated,
                Some(false),
                false,
                Some("CURRENT_TIMESTAMP"),
                true,
                true,
            )
            .expect("MySQL 应支持改列");
        assert!(sql.contains("DEFAULT CURRENT_TIMESTAMP"), "{sql}");
        assert!(sql.contains("ON UPDATE CURRENT_TIMESTAMP"), "{sql}");
        assert!(!sql.contains("'CURRENT_TIMESTAMP'"), "表达式默认值不能加引号：{sql}");

        // ③ 没有默认值时与旧方法逐字节一致（不引入回归）
        assert_eq!(
            DatabaseKind::MySql.alter_column_sql_keep_full(
                table, code, Some(true), false, None, false, false
            ),
            DatabaseKind::MySql.alter_column_sql_keep(table, code, Some(true), false)
        );

        // ④ 非 MySQL 方言：默认值本就由库保留，不追加任何子句
        assert_eq!(
            DatabaseKind::PostgreSql
                .alter_column_sql_keep_full(table, code, None, false, Some("abc"), false, true),
            DatabaseKind::PostgreSql.alter_column_sql_keep(table, code, None, false)
        );
    }

    #[test]
    fn add_column_with_not_null_without_default_degrades_to_nullable() {
        let table = sample_table();
        let mut col = ColumnMeta {
            name: "Extra".into(),
            column_name: None,
            data_type: DataType::String,
            raw_type: None,
            length: 10,
            precision: 0,
            scale: 0,
            identity: false,
            primary_key: false,
            master: false,
            nullable: false,
            default_value: None,
            description: String::new(),
            enum_type: None,
            data_scale: None,
            map: None,
            show_in: None,
            model: None,
            category: None,
            item_type: None,
            using_cache: None,
            extra: Vec::new(),
        };

        let sql = DatabaseKind::Sqlite.add_column_sql(&table, &col);
        assert!(!sql.contains("NOT NULL"), "{sql}");

        col.default_value = Some("0".into());
        let sql = DatabaseKind::Sqlite.add_column_sql(&table, &col);
        assert!(sql.contains("NOT NULL DEFAULT '0'"), "{sql}");
    }

    #[test]
    fn paging_syntax() {
        let base = "SELECT * FROM \"T\"";
        assert_eq!(
            DatabaseKind::Sqlite.apply_paging(base, "ORDER BY \"Id\"", 20, 10),
            "SELECT * FROM \"T\" ORDER BY \"Id\" LIMIT 10 OFFSET 20"
        );
        assert_eq!(
            DatabaseKind::SqlServer.apply_paging(base, "ORDER BY [Id]", 20, 10),
            "SELECT * FROM \"T\" ORDER BY [Id] OFFSET 20 ROWS FETCH NEXT 10 ROWS ONLY"
        );
        // SQL Server 缺少排序时给出兜底排序，保证语法合法
        assert!(
            DatabaseKind::SqlServer
                .apply_paging(base, "", 0, 10)
                .contains("ORDER BY (SELECT NULL)")
        );
        // Oracle 使用 ROWNUM 双层包装（兼容 11g）
        assert_eq!(
            DatabaseKind::Oracle.apply_paging(base, "ORDER BY \"Id\"", 20, 10),
            "SELECT * FROM (SELECT T0.*, ROWNUM AS rowNumber FROM (SELECT * FROM \"T\" ORDER BY \"Id\") T0) \
             WHERE rowNumber > 20 AND rowNumber <= 30"
        );
    }

    #[test]
    fn sqlserver_row_number_paging_matches_mspagesplit() {
        // SQL Server 2005/2008：ROW_NUMBER 双层包装（对齐 MSPageSplit.RowNumber）
        let base = "SELECT * FROM [DH_Order] WHERE ([Status] = @p0)";
        let sql = DatabaseKind::SqlServer.apply_paging_with_style(
            base,
            "ORDER BY [Id] DESC",
            20,
            10,
            PageStyle::RowNumber,
        );
        assert_eq!(
            sql,
            "SELECT * FROM (SELECT *, row_number() over(Order By [Id] DESC) as rowNumber FROM (SELECT * FROM [DH_Order] WHERE ([Status] = @p0)) AS XCode_T0) AS XCode_T1 WHERE rowNumber BETWEEN 21 And 30"
        );

        // 无排序：OVER 子句给合法兜底，保证语法可用
        let sql =
            DatabaseKind::SqlServer.apply_paging_with_style(base, "", 0, 5, PageStyle::RowNumber);
        assert!(
            sql.contains("row_number() over(Order By (SELECT NULL)) as rowNumber"),
            "{sql}"
        );
        assert!(sql.contains("rowNumber BETWEEN 1 And 5"), "{sql}");

        // 其它库忽略风格（与默认分页一致）
        assert_eq!(
            DatabaseKind::Sqlite.apply_paging_with_style(
                "SELECT * FROM T",
                "ORDER BY \"Id\"",
                5,
                5,
                PageStyle::RowNumber,
            ),
            DatabaseKind::Sqlite.apply_paging("SELECT * FROM T", "ORDER BY \"Id\"", 5, 5)
        );
    }

    #[test]
    fn postgres_serial_identity_and_oracle_sequence() {
        let table = sample_table();
        let sql = DatabaseKind::PostgreSql.create_table_sql(&table).remove(0);
        // 与 DH.NCode 对齐：PostgreSQL 自增使用 serial/serial8（伪类型，替换原类型）
        assert!(sql.contains("\"Id\" serial NOT NULL"), "{sql}");

        let mut t64 = table.clone();
        t64.columns[0].data_type = DataType::Int64;
        let sql64 = DatabaseKind::PostgreSql.create_table_sql(&t64).remove(0);
        assert!(sql64.contains("\"Id\" serial8 NOT NULL"), "{sql64}");

        // Oracle：自增列无内联属性，SQL 跟随建表脚本一起导出
        let stmts = DatabaseKind::Oracle.create_table_sql(&table);
        assert!(
            stmts[0].contains("\"Id\" number(10) NOT NULL"),
            "{}",
            stmts[0]
        );
        assert!(!stmts[0].contains("IDENTITY"), "{}", stmts[0]);
        // NOT NULL 的 DateTime 列补固定默认值（与 DH.NCode 一致），且 DEFAULT 在 NOT NULL 之前
        assert!(
            stmts[0].contains("DEFAULT To_Date('0001-01-01','yyyy-mm-dd') NOT NULL"),
            "{}",
            stmts[0]
        );
        assert_eq!(
            stmts.last().unwrap(),
            "CREATE SEQUENCE \"SEQ_DH_Order\" START WITH 1 INCREMENT BY 1 CACHE 20"
        );
    }

    #[test]
    fn add_column_keyword_differs_by_dialect() {
        let table = sample_table();
        let mut col = table.column("Code").unwrap().clone();
        col.default_value = Some("x".into());

        let sqlite = DatabaseKind::Sqlite.add_column_sql(&table, &col);
        assert!(
            sqlite.starts_with("ALTER TABLE \"DH_Order\" ADD COLUMN \"Code\""),
            "{sqlite}"
        );

        let mssql = DatabaseKind::SqlServer.add_column_sql(&table, &col);
        assert!(
            mssql.starts_with("ALTER TABLE [DH_Order] ADD [Code]"),
            "{mssql}"
        );
        assert!(!mssql.contains("ADD COLUMN"), "{mssql}");

        let oracle = DatabaseKind::Oracle.add_column_sql(&table, &col);
        assert!(
            oracle.starts_with("ALTER TABLE \"DH_Order\" ADD \"Code\""),
            "{oracle}"
        );
        assert!(oracle.contains("DEFAULT 'x' NOT NULL"), "{oracle}");
    }

    #[test]
    fn default_value_rendering() {
        let table = sample_table();
        let mut col = table.column("Ok").unwrap().clone();
        col.default_value = Some("True".into());
        assert!(
            DatabaseKind::Sqlite.create_table_sql(&{
                let mut t = table.clone();
                t.columns = vec![col.clone()];
                t
            })[0]
                .contains("DEFAULT 1")
        );

        let mut s = col.clone();
        s.data_type = DataType::String;
        s.length = 10;
        s.default_value = Some("待处理".into());
        let mut t2 = table.clone();
        t2.columns = vec![s];
        assert!(DatabaseKind::Sqlite.create_table_sql(&t2)[0].contains("DEFAULT '待处理'"));
    }
}
