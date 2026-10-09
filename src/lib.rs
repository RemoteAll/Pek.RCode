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
//! | 迁移 Migration（`Off`/`ReadOnly`/`On`/`Full` 四档） | [`migration::Migration`] + [`Dal::sync_schema`](dal::Dal::sync_schema) |
//! | `XCode.Cache`（`Meta.Cache` / `Meta.SingleCache`） | [`cache::EntityCache`] / [`cache::SingleCache`]（写入自动失效） |
//! | `DAL.GetTables`（反向工程） | [`reverse`]（[`Dal::read_model`](dal::Dal::read_model)） |
//! | `Shards` / `EntitySplit`（分表） | [`shards`]（`TimeShardPolicy` + `TableRef::query_sharded` / `Entity::insert_sharded` 等，表名与 C# 共用） |
//! | `EntityExtension.Insert(list)`（批量写入） | [`entity::Entity::insert_batch`] / [`insert_batch_sharded`](entity::Entity::insert_batch_sharded) / [`delete_batch_sharded`](entity::Entity::delete_batch_sharded)（多行 `VALUES` + 分片分组，跨库自动路由） |
//! | `DAL.AddConnStr` / `DAL.Create`（分库连接解析） | [`shards::register_connection`]（跨库路由；未注册的连接名按 C# 规则自动落 SQLite 库） |
//! | `Snowflake`（雪花 Id） | [`snowflake`]（[`Snowflake`](snowflake::Snowflake)，位结构与 C# 互通） |
//! | `xcode` 命令（XCodeTool 代码生成） | [`codegen`]（`generate` / `generate_all`） |
//!
//! ## 支持的数据库
//!
//! | 数据库 | 方言（SQL 生成） | 驱动（执行） |
//! |--------|------------------|--------------|
//! | SQLite | ✅ | ✅（rusqlite，内嵌） |
//! | MySQL | ✅ | ✅（mysql crate，纯 Rust；native-tls：Preferred 缺省可回退、Required/VerifyCA/VerifyFull 强制） |
//! | SQL Server | ✅ | ✅（tiberius，纯 Rust TDS；`Encrypt`/`TrustServerCertificate` 可配） |
//! | PostgreSQL | ✅ | ✅（postgres crate；native-tls 同上；HighGo/KingBase/VastBase 同协议复用） |
//! | Oracle | ✅ | ✅（oracle crate/OCI，运行时需 Instant Client；自增用序列 `SEQ_{表名}`） |
//! | DuckDB | ✅ | ✅（duckdb crate 内嵌；`--features duckdb`，需 CMake 工具链） |
//! | ClickHouse | ✅ | ✅（HTTP `:8123`，`TSVWithNamesAndTypes`） |
//! | TDengine | ✅ | ✅（REST `:6041`） |
//! | InfluxDB | ✅ | ✅（1.x 行协议写入 + InfluxQL 查询） |
//! | SAP HANA | ✅ | ✅（hdbconnect） |
//! | Firebird | ✅ | ✅（rsfbclient 动态加载 fbclient.dll；Remote/Embedded） |
//! | DB2 / 达梦 / IRIS / Access | ✅ | ✅（odbc-api 桥接本机 ODBC 驱动） |
//! | MongoDB | ✅ | ✅（SQL 子集翻译为文档操作；无 DDL） |
//! | NovaDb | ✅ | ✅（复用 MySQL 驱动） |
//!
//! `network` 为远程驱动（登录远端探明类型后转发 SQL，见 `network` 模块）；
//! `sqlce` 明确不支持（SSCE 运行时已停更）。
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
//! - `sync_schema` 默认（`On`）只做增量补齐（建表/加列/补索引）；修改列类型与删除多余列/索引
//!   仅 [`Migration::Full`] 档允许，且模型外的表从不自动删除（档位语义与 DH.NCode 一致）

#[cfg(all(feature = "tls-native", feature = "tls-rustls"))]
compile_error!(
    "特性 `tls-native` 与 `tls-rustls` 互斥：rustls 后端请用 `--no-default-features --features tls-rustls`"
);

pub mod async_dal;
pub mod backup;
pub mod batch;
pub mod cache;
pub mod catalog;
#[cfg(feature = "driver-clickhouse")]
pub mod clickhouse;
pub mod codegen;
pub mod common;
pub mod dal;
pub mod data_access;
#[cfg(feature = "database")]
pub mod database;
pub mod db_admin;
pub mod db_service;
pub mod dbtable;
pub mod dialect;
pub mod dirty;
#[cfg(feature = "driver-pack")]
pub mod driver_pack;
#[cfg(feature = "duckdb")]
pub mod duckdb;
pub mod entity;
pub mod entity_queue;
mod error;
#[cfg(feature = "driver-firebird")]
pub mod firebird;
#[cfg(feature = "driver-hana")]
pub mod hana;
pub mod http;
#[cfg(feature = "driver-influxdb")]
pub mod influxdb;
pub mod interceptor;
pub mod membership;
pub mod meta;
pub mod migration;
pub mod model;
#[cfg(feature = "driver-mongodb")]
pub mod mongodb;
#[cfg(feature = "driver-sqlserver")]
pub mod mssql;
#[cfg(feature = "driver-mysql")]
pub mod mysql;
pub mod navigation;
pub mod network;
#[cfg(feature = "driver-odbc")]
pub mod odbc;
#[cfg(feature = "driver-oracle")]
pub mod oracle;
pub mod pool;
#[cfg(feature = "driver-postgresql")]
pub mod postgres;
pub mod query;
pub mod reverse;
pub mod session;
pub mod shards;
pub mod show_in;
pub mod simulation;
pub mod snowflake;
pub mod sqlbuild;
pub mod sqlite;
pub mod sql_template;
pub mod statistics;
pub mod store;
#[cfg(feature = "driver-tdengine")]
pub mod tdengine;
pub mod transaction;
pub mod transform;
pub mod tree;
pub mod types;
pub mod value;

// 同步驱动共用 tokio 运行时的桥接（tiberius/mongodb 的 block_on）：仅在对应驱动启用时编译
#[cfg(any(feature = "driver-sqlserver", feature = "driver-mongodb"))]
mod rt;

pub use error::{Error, Result};

// 常用类型直达
pub use dal::Dal;
pub use dialect::DatabaseKind;
pub use entity::Entity;
pub use migration::Migration;
pub use model::{ColumnMeta, EntityModel, IndexMeta, TableMeta};
pub use query::{Query, Where};
pub use session::{DbRow, RowSet, SqlSession};
pub use types::DataType;
pub use value::DbValue;

