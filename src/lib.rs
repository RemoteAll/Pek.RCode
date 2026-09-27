//! # pek-rcode —— DH.NCode（XCode ORM）的 Rust 实现
//!
//! 目标：让现有 C#/.NET 项目（DH.NCode 技术栈）能够**渐进迁移**到 Rust——
//! 两边共用同一份 `Model.xml` 数据模型，逐步把业务从 C# 搬到 Rust。
//!
//! ## 概念对照
//!
//! | C# / DH.NCode | Rust / pek-rcode |
//! |---------------|-----------------|
//! | `Model.xml` / `EntityModel` | [`model::EntityModel`]（[`EntityModel::load`](model::EntityModel::load) / [`EntityModel::parse`](model::EntityModel::parse)） |
//! | `IDataColumn` / `DataType` | [`model::ColumnMeta`] / [`types::DataType`] |
//! | `DAL` / 连接串 | [`dal::Dal`] / [`dal::ConnectionString`] |
//! | `IDbSession` | [`session::SqlSession`] |
//! | `DbBase` 子类（SQLite.cs / SqlServer.cs …） | [`dialect::DatabaseKind`]（类型映射/分页/DDL/自增） |
//! | `InsertBuilder` / `SelectBuilder` | [`sqlbuild`] |
//! | `WhereExpression` | [`query::Where`] |
//! | `PageParameter` | [`query::Query`]（`page` / `take`） |
//! | 迁移 Migration（建表/加列） | [`Dal::sync_schema`](dal::Dal::sync_schema) |
//! | `xcode` 命令（XCodeTool 代码生成） | [`codegen`]（`generate` / `generate_all`） |
//!
//! ## 支持的数据库
//!
//! | 数据库 | 方言（SQL 生成） | 驱动（执行） |
//! |--------|------------------|--------------|
//! | SQLite | ✅ | ✅（rusqlite，内嵌） |
//! | MySQL | ✅ | ✅（mysql crate，纯 Rust；未启用 TLS，SslMode=None 可用） |
//! | SQL Server | ✅ | ✅（tiberius，纯 Rust TDS；`Encrypt`/`TrustServerCertificate` 可配） |
//! | PostgreSQL | ✅ | ✅（postgres crate；HighGo/KingBase/VastBase 同协议复用） |
//! | Oracle | ✅ | ✅（oracle crate/OCI，运行时需 Instant Client；自增用序列 `SEQ_{表名}`） |
//!
//! DH.NCode 的其它数据库（Access/ClickHouse/DaMeng/DB2/DuckDB/Firebird/Hana/InfluxDB/IRIS/MongoDB/SqlCe/TDengine）
//! 在路线图中：方言与连接串解析已就绪，就近可直接生成脚本，后续按需求接入驱动。
//!
//! ## 快速开始
//!
//! ```no_run
//! use pek_rcode::{dal::Dal, model::EntityModel};
//!
//! # fn main() -> pek_rcode::Result<()> {
//! // 1) 复用 C# 项目中的 Model.xml
//! let model = EntityModel::load(std::path::Path::new("Model.xml"))?;
//!
//! // 2) 打开数据库（连接串与 XCode 一致）
//! let dal = Dal::open_with_model("Data Source=demo.db;Provider=SQLite", model)?;
//!
//! // 3) 同步结构（建表 / 补列，增量且不破坏已有数据）
//! let report = dal.sync_schema()?;
//! println!("{report}");
//!
//! // 4) 实体级增删改查
//! let table = dal.table("Order")?;
//! let mut session = dal.open_session()?;
//! let id = table.insert(session.as_mut(), &[("Code", "HLT-001".into())])?;
//! let row = table.find_by_pk(session.as_mut(), &[id.into()])?;
//! println!("{row:?}");
//! # Ok(())
//! # }
//! ```
//!
//! ## 安全说明
//!
//! - 所有查询/写入均使用参数绑定；列名、表名经方言引用转义
//! - `sync_schema` 只做增量补齐（建表/加列），不修改、不删除已有对象

pub mod codegen;
pub mod dal;
pub mod dialect;
pub mod entity;
mod error;
pub mod model;
pub mod mssql;
pub mod mysql;
pub mod oracle;
pub mod postgres;
pub mod query;
pub mod session;
pub mod sqlbuild;
pub mod sqlite;
pub mod types;
pub mod value;

pub use error::{Error, Result};

// 常用类型直达
pub use dal::Dal;
pub use dialect::DatabaseKind;
pub use entity::Entity;
pub use model::{ColumnMeta, EntityModel, IndexMeta, TableMeta};
pub use query::{Query, Where};
pub use session::{DbRow, RowSet, SqlSession};
pub use types::DataType;
pub use value::DbValue;

