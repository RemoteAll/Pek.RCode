# pek-rcode — DH.NCode（XCode ORM）的 Rust 实现

Pek 生态的 Rust 数据中间件（独立项目）：让 C#/.NET 项目（DH.NCode 技术栈）能够**渐进迁移**到 Rust——
两边**共用同一份 `Model.xml`**、**共用同一个数据库**，业务可以一个用例一个用例地从 C# 搬到 Rust，而不是推倒重来。

```
┌─────────────────────────┐        ┌─────────────────────────┐
│  C# 现有系统（DH.NCode） │        │  Rust 新代码（Pek.RCode）  │
│  Entity / DAL / CRUD    │  共享  │  model / dal / CRUD     │
└───────────┬─────────────┘        └───────────┬─────────────┘
            │        Model.xml（唯一事实来源）    │
            └───────────────┬───────────────────┘
                            ▼
                   数据库（SQLite / MySQL / …）
```

---

## 一、当前能力（v0.1.0）

| 模块 | 对应 DH.NCode | 状态 |
|------|---------------|------|
| `model` | `Model.xml` / `EntityModel` | ✅ 解析 + 写出（往返一致） |
| `types` | `DataType`（CLR 类型名） | ✅ `Boolean/Byte/Int16/Int32/Int64/Single/Double/Decimal/String/DateTime/Binary` |
| `value` | 字段值 / `DbType` 转换 | ✅ 含 Decimal 精确值、XCode 格式时间文本（7 位小数秒） |
| `dialect` | 各 `DbBase` 子类（SQLite.cs / MySql.cs / …） | ✅ **16 种库**的类型映射 / DDL / 分页 / 自增 / 标识符与占位符 |
| `session` | `IDbSession` | ✅ 抽象就绪，驱动可插拔 |
| `sqlite` | `SQLite.cs` | ✅ **可执行**（rusqlite 内嵌，无外部依赖） |
| `mysql` | `MySql.cs` | ✅ **可执行**（mysql crate，纯 Rust；连接串与 XCode 一致；TLS 已启用：Preferred 缺省可回退，Required/VerifyCA/VerifyFull 强制） |
| `mssql` | `SqlServer.cs` | ✅ **可执行**（tiberius，纯 Rust TDS；同步接口内部维护专用 tokio 运行时） |
| `postgres` | `PostgreSQL.cs` | ✅ **可执行**（postgres crate；HighGo/金仓/VastBase 同协议直接复用） |
| `oracle` | `Oracle.cs` | ✅ **可执行**（oracle crate / OCI；运行时需 Instant Client；自增用序列 `SEQ_{表名}`） |
| `duckdb` | `DuckDb.cs` | ✅ **可执行**（duckdb crate 内嵌引擎；`--features duckdb`，需 CMake 工具链） |
| `clickhouse` | `ClickHouse.cs` | ✅ **可执行**（HTTP 接口 `:8123`，`TSVWithNamesAndTypes`） |
| `tdengine` | `TDengine.cs` | ✅ **可执行**（REST 接口 `:6041`） |
| `influxdb` | `Influx.cs` | ✅ **可执行**（1.x 行协议写入 + InfluxQL 查询） |
| `hana` | `SapHana.cs` | ✅ **可执行**（hdbconnect，原生 hdb:// 连接） |
| `firebird` | `Firebird.cs` | ✅ **可执行**（rsfbclient 动态加载 fbclient.dll；Remote/Embedded 双模式） |
| `odbc` | `Db2.cs` / `DaMeng.cs` / `Iris.cs` / `Access.cs` | ✅ **可执行**（odbc-api 桥：DB2 / 达梦 / IRIS / Access） |
| `mongodb` | `MongoDb.cs` | ✅ **可执行**（SQL 子集翻译为文档操作；无独立查询语言） |
| `http` / `rt` | — | 内部公共层：HTTP 驱动共用传输层、异步驱动共用 tokio 运行时 |
| `sqlbuild` | `InsertBuilder` / `SelectBuilder` | ✅ INSERT/UPDATE/DELETE/SELECT/COUNT |
| `query` | `WhereExpression` / `PageParameter` | ✅ 链式条件 + 分页/取前 N |
| `dal` | `DAL` / 迁移 Migration | ✅ 连接串解析、结构同步（建表/补列/**补索引**）、结构比对 `diff_schema`（差异报告 + ALTER 导出）、实体表操作 |
| `pool` | `ConnectionPool` | ✅ 会话连接池（按连接串共享；Min=CPU(2–8)/Max=1000/空闲 30s；`Pooling=false` 关闭） |
| `async_dal` | `IAsyncDbSession` / `*Async` | ✅ 异步门面（tokio `spawn_blocking` 包装全驱动；`run`/`with_session` 覆盖全部同步 API） |
| `catalog` | 各 `DbBase.OnGetTables` 元数据 | ✅ 全驱动目录读取（表/列/索引；MongoDB 除外；Access 走 ODBC 元数据） |
| `cache` | `XCode.Cache`（`Meta.Cache` / `Meta.SingleCache`） | ✅ 整表实体缓存 + 单对象缓存（默认 60s 过期；写入自动失效）；✅ Redis 版本号（feature `redis`，底层为 **Pek.RRedis** 自研客户端） |
| `reverse` | `DAL.GetTables`（反向工程） | ✅ 数据库 → `EntityModel` / `Model.xml`（**全部驱动**、含索引/唯一约束；`rcodegen --conn`） |
| `entity` | `Entity` 基类（对象实体） | ✅ `insert / save / update / delete / find / query / count`；`AuditExt` 审计字段访问；`#[derive(Entity)]` 宏 |
| `db_service` | `Services`（`DbServer` / `DbClient`） | ✅ 远程服务层 + HTTP 客户端；`/Db/Query` 为 **DbTable v3 二进制**（与 C# `DbClient` 双向互通，黄金样本逐字节验证）；配套 `provider=network` 驱动与 `examples/dbserver` 参考宿主 |
| `backup` | `DAL_Backup` / `DbPackage` | ✅ 单表备份/恢复（DbTable v3 文件、`.gz` 自动压缩）、多表 zip 包（`{连接名}.xml` + `{实体名}.table`）、跨库同步 `sync_table`/`sync_all`；**与 C# 备份文件互认** |
| `meta` | `DbMetaData` / `IMetaData` | ✅ 建库/删库/存在性、建表/删表、列增/改/删、索引建/删、表列注释（按方言，语句逐一对齐各驱动覆写） |
| `navigation` | `Navigation*` / `DataRowEntityAccessor` | ✅ 导航注册表（HasOne/HasMany）+ 装载 `load_one`/`load_many` + 行集→实体（`Entity::load`，同一能力面） |
| `simulation` | `Common/DataSimulation` | ✅ 造数压测（随机整型/字符串/时间 + 分批事务 + TPS） |
| `codegen` | `xcode` 命令（XCodeTool） | ✅ `Model.xml` → Rust **对象实体**（结构体 + `Entity` 实现 + `new()/Default`） |
| `rcodegen` 工具 | `xcode` 命令行 | ✅ 独立生成工具（`--list / --table / --dry-run / --force`；`--conn` 反向工程：库 → `Model.xml`） |

测试：**238 项全部通过**（库单测 211 + 集成 17 + 文档测试 10；`--features duckdb` 全量 245 项（另含 DuckDB 内嵌引擎全链路用例），`--features redis` 全量 239 项（另含 Redis 版本号用例，`RCODE_REDIS` 门控），`--no-default-features --features tls-rustls` 全量 238 项（rustls TLS 后端），`--no-default-features` 全量 236 项（完全不含 TLS 依赖）；
MySQL / PostgreSQL / SQL Server / Oracle / network 端到端用例在有真实库/服务时自动启用），
其中包括生产模型快照固件（7 张真实表，覆盖全部 8 种数据类型）的端到端回归、
**对象实体（Entity）在 SQLite 与 MySQL / PostgreSQL / SQL Server / Oracle 各条链路的端到端用例**
（远端库侧用与 `rcodegen` 输出同构的实体，覆盖 insert/save 新增与更新双分支/find/query/count/delete/事务）、
以及用真实表（JiLiYu、VerifyCode）生成实体后的编译与运行验证；
DuckDB 在 `--features duckdb` 下用**内嵌真实引擎**跑通建序列/建表/增删改查/事务回滚全链路；
**实体缓存 / 单对象缓存**（命中、失效、过期重载）与**反向工程**（建库 → 反向 → 逐列对比 roundtrip）均有行为用例；
其余网络型数据库（ClickHouse/TDengine/InfluxDB/Hana/Firebird/ODBC 系列/MongoDB）提供值转换与语句翻译单测；
Redis 版本号用例（`--features redis`）底层为 **Pek.RRedis** 自研客户端（与 C# 同一 Redis 实例、同一字节格式）；
另可用环境变量 `RCODE_MODEL` 对完整生产 `Model.xml` 跑全量回归（见下文）。

### 与 DH.NCode 支持范围的对照

DH.NCode 内置的全部数据库驱动均已接入（provider 名称与 XCode 链接串一致）：

| 状态 | 数据库 |
|------|--------|
| ✅ 已接入驱动 | SQLite、MySQL/MariaDB、SQL Server、PostgreSQL（含 HighGo/瀚高、KingBase/金仓、VastBase/海量）、Oracle、DuckDB、ClickHouse、TDengine、InfluxDB、SAP HANA、Firebird、DB2、达梦（DaMeng）、IRIS、Access、MongoDB、NovaDb（复用 MySQL 协议）、**network**（XCode 远程服务协议：SQL 转发到远端 DbServer，服务端可为 C# `DbServer` 或本仓 `examples/dbserver`） |
| ⚠️ 边界 | `sqlce`（SSCE 已停止维护且无可行运行时；建议迁移 SQLite） |

---

## 二、快速开始

```rust
use pek_rcode::{Dal, EntityModel, Query, Where};

// 1) 复用 C# 项目中的 Model.xml（路径指向实体项目里的 Entity/Model.xml 即可）
let model = EntityModel::load(std::path::Path::new("Model.xml"))?;

// 2) 打开数据库：连接串与 XCode 格式一致（SQLite / MySQL 均可）
let dal = Dal::open_with_model("Data Source=..\\..\\Data\\DG.db;Provider=SQLite;ShowSql=false", model)?;
// 或 MySQL：
// let dal = Dal::open_with_model("Server=localhost;Port=3306;Database=mes;Uid=root;Pwd=***;provider=mysql;SslMode=None", model)?;

// 3) 同步结构：只做增量（建表 / 补列），不改不删
let report = dal.sync_schema()?;
println!("{report}");

// 4) 实体级增删改查
let table = dal.table("VerifyCode")?;
let mut session = dal.open_session()?;

let _ = table.insert(session.as_mut(), &[
    ("Key", "k-001".into()),
    ("Code", "123456".into()),
    ("CreateTime", chrono::Local::now().naive_local().into()),
])?;

let row = table.find_by_pk(session.as_mut(), &["k-001".into()])?;
println!("{row:?}");

let filter = Where::new().like("Code", "123%");
let total = table.count(session.as_mut(), Some(&filter))?;
let page = table.query(session.as_mut(), &Query::new().filter(filter).page(1, 20))?;
println!("共 {total} 条，本页 {} 条", page.len());
# Ok::<(), pek_rcode::Error>(())
```

运行测试与构建：

```powershell
cd G:\Code\Pek.Rust\Pek.RCode
cargo test          # 默认测试（含 SQLite + 固件回归 + 对象实体端到端）
cargo test --features duckdb   # 额外交付 DuckDB 内嵌引擎的完整用例
cargo test --features redis    # 分布式缓存版本号（基于 Pek.RRedis；RCODE_REDIS 指向真实 Redis 时实机验证）
cargo test --no-default-features --features tls-rustls   # rustls TLS 后端（PEM 客户端证书）
cargo clippy        # 零警告
```

### 测试数据与全量回归

- `tests/fixtures/wms_model_sample.xml`：生产模型快照固件（7 张真实表，覆盖全部 8 种数据类型），默认测试均基于它
- `tests/fixtures/dbtable_v3_sample.bin`：DbTable v3 二进制**黄金样本**（由真实 C# NewLife.Core `DbTable.ToPacket()` 生成；Rust 编解码与之逐字节双向验证）
- 需要验证**完整生产模型**（176 张表规模）时，用环境变量指向完整 `Model.xml`：

```powershell
$env:RCODE_MODEL = "<你的项目>\Entity\Model.xml"
cargo test full_model      # 解析全量模型 + 全部表同步到临时 SQLite 库验证
```

### 真实 SQLite 历史库兼容性验证（副本）

用生产历史库的**副本**验证与 C#/.NET 端“共库”的能力：既存表全部可读、增量同步零破坏、抽样 CRUD：

```powershell
Copy-Item <生产库> $env:TEMP\DG-live-copy.db -Force
$env:RCODE_LIVE_DB = "$env:TEMP\DG-live-copy.db"   # 测试内置防呆：拒绝 BinWeb 生产路径
$env:RCODE_MODEL   = "<你的项目>\Entity\Model.xml"
cargo test --test live_sqlite_e2e -- --nocapture
```

实测结果（2026-09-27，本仓库历史业务库副本）：库中既存 52 张表全部可读；
增量同步新建 **168 张表**、补充 **8 个列**（`DH_WmsOrder.*`）；同步前后 52 张表行数**完全一致**（数据零破坏）；
模型 **176/176** 张表就位；自增主键与字符串主键两条 CRUD 往返均通过；
反向工程读回 220 张表，模型 176 张表全部覆盖。

### 反向工程与实体缓存

**反向工程**（对应 C# 的 `DAL.GetTables`）：数据库结构 → `EntityModel`，可写出 `Model.xml`，
再由 `codegen` 生成实体，形成“库 → 模型 → 实体”闭环：

```rust
let dal = Dal::open("Data Source=..\\Data\\DG.db;Provider=SQLite")?;
let model = dal.read_model()?;      // 读取全部表/列/主键/自增/可空/默认值
std::fs::write("Model.xml", model.to_xml())?;
```

```powershell
# 命令行用法（rcodegen）
rcodegen --conn "Data Source=..\Data\DG.db;Provider=SQLite" --list          # 列出库表
rcodegen --conn "Data Source=..\Data\DG.db;Provider=SQLite" --out Model.xml # 生成 Model.xml
```

- 当前支持 SQLite（`pragma_table_info` 表值函数 + `sqlite_master`，自增按 `AUTOINCREMENT` 识别）；
  其它数据库返回可操作提示，可按需扩展
- 实测：对历史库副本（220 表）一键反向生成 250KB `Model.xml`，产物可直接作为 `rcodegen --model` 的输入；
  固件模型“建库 → 反向 → 逐表逐列对比”roundtrip 全等（类型/主键/自增/可空/字符串长度）

**实体缓存**（对应 C# 的 `Meta.Cache` / `Meta.SingleCache`）：

```rust
let cache = dal.entity_cache("JiLiYu")?;                 // 整表缓存（读多写少）
let rows  = cache.entities(&dal, session.as_mut())?;     // 首次加载整表，之后走内存
let one   = cache.find_by_pk("Id", &1.into());

let single = dal.single_cache("VerifyCode")?;            // 单对象缓存（按主键点查）
let item   = single.get(&dal, session.as_mut(), &["k-001".into()])?;
```

- 默认过期 60 秒；任何写入（表句柄/实体层）都会**立即失效**对应表缓存，下次访问自动重载
- 与 C# 版的差异：C# 过期后“返回旧数据 + 异步更新”，Rust 版首版为“过期后同步重载”（语义更直观）

### MySQL 使用与集成测试

连接串与 XCode 完全兼容（键名：`Server`/`Port`/`Database`/`Uid`/`Pwd`/`SslMode`/`Charset`/`Timeout`），
驱动基于纯 Rust 的 `mysql` crate：

```rust
let dal = Dal::open_with_model(
    "Server=10.0.0.8;Port=3306;Database=mes;Uid=app;Pwd=***;provider=mysql;SslMode=None", model)?;
dal.sync_schema()?;   // 增量建表/补列（information_schema 探测）
```

- TLS（native-tls）：`SslMode=None/Disabled` 明文；`Preferred`（缺省）先试 TLS、服务器不支持时回退明文；`Required` 强制加密（不校验证书）；`VerifyCA`/`VerifyFull` 逐级校验；根证书用 `SslCa`/`CertificateFile`；客户端证书（`SslCert`/`SslKey`，PEM）暂不支持（会明确报错）
- MySQL 方言对齐 DH.NCode：布尔 `TINYINT`、字段说明生成列 `COMMENT`、`DECIMAL` 的 Length 覆盖 Precision
- 真实库端到端测试（默认自动跳过；只操作 `rcode_test_` 前缀的专用表，结束即清理）：

```powershell
$env:RCODE_MYSQL = "Server=127.0.0.1;Port=3306;Database=rcode_test;Uid=root;Pwd=root;provider=mysql;SslMode=None"
cargo test --test mysql_e2e
```

### PostgreSQL / SQL Server / Oracle 使用与集成测试

三个驱动的连接串同样与 XCode 兼容，按 `provider` 分发（`highgo`/`kingbase`/`vastbase` 自动走 PostgreSQL 驱动）：

```rust
// PostgreSQL（也用于瀚高/金仓/海量）
let dal = Dal::open_with_model(
    "Server=10.0.0.9;Port=5432;Database=mes;Uid=app;Pwd=***;provider=postgresql", model)?;
// SQL Server（缺省 Encrypt=Required + TrustServerCertificate=true，可直连自签证书实例）
let dal = Dal::open_with_model(
    "Server=10.0.0.5;Port=1433;Database=mes;Uid=sa;Pwd=***;provider=sqlserver;Encrypt=false", model)?;
// Oracle（EZConnect 或 TNS 别名；需 Instant Client）
let dal = Dal::open_with_model(
    "Server=10.0.0.6;Port=1521;ServiceName=xepdb1;Uid=dbuser;Pwd=***;provider=oracle", model)?;
```

- 自增回写：PostgreSQL 用 `INSERT ... RETURNING`（对齐 DH.NCode 的 `RETURNING *`）；SQL Server 用 `SCOPE_IDENTITY()`；
  Oracle 用序列 `SEQ_{表名}`（建表/同步结构时自动创建，插入时写 `NEXTVAL`、随后读 `CURRVAL`）
- 事务：PostgreSQL/MySQL/SQL Server 显式 `BEGIN`；Oracle 隐式事务（`begin()` 为空操作）
- 真实库端到端测试（各库默认自动跳过；只创建/删除 `rcode_test_` 前缀的专用对象）：

```powershell
$env:RCODE_POSTGRES = "Server=127.0.0.1;Port=5432;Database=rcode_test;Uid=postgres;Pwd=***;provider=postgresql"
$env:RCODE_MSSQL    = "Server=127.0.0.1;Port=1433;Database=rcode_test;Uid=sa;Pwd=***;provider=sqlserver;Encrypt=false"
$env:RCODE_ORACLE   = "Server=127.0.0.1;Port=1521;ServiceName=xepdb1;Uid=rcode;Pwd=***;provider=oracle"
cargo test --test remote_e2e
```

> TLS：MySQL/PostgreSQL 已内置 native-tls（缺省 Preferred/Prefer：能 TLS 就 TLS、服务器不支持回退明文；`Require/VerifyCA/VerifyFull` 强制校验，根证书分别用 `SslCa`/`Root Certificate`）；Oracle 支持 `Protocol=tcps`（TLS 由 OCI 客户端/钱包管理）；SQL Server 可自选 `Encrypt`，默认加密+信任自签证书。
> TLS 后端可切换：`--no-default-features --features tls-rustls` 换用 rustls（**PEM 客户端证书**：MySQL `SslCert`/`SslKey`、PostgreSQL `SSL Certificate`/`SSL Key`；该后端下 `VerifyCA` 与 `VerifyFull` 均校验证书链与主机名，且与 `tls-native` 特性互斥）。
> 依赖镜像：工程内 `.cargo/config.toml` 已配置 rsproxy。

### 其它数据库（DH.NCode 全量驱动）

DH.NCode 内置的其余数据库均已接入，按 `provider` 分发：

| provider | 连接串要点 | 说明 |
|----------|------------|------|
| `duckdb` | `Data Source=mes.duckdb`（或 `:memory:`） | 内嵌引擎（`--features duckdb`，需 CMake）；自增=序列+`RETURNING` |
| `clickhouse` | `Server=..;Port=8123;Database=..` | HTTP 接口（`TSVWithNamesAndTypes`）；无事务；`UPDATE/DELETE` 走 `ALTER TABLE ... UPDATE` 语义仍受服务端限制 |
| `tdengine` | `Server=..;Port=6041;Database=..` | REST 接口；无事务 |
| `influxdb` | `Server=..;Port=8086;Database=..`（1.x） | 写入自动生成行协议；查询走 InfluxQL；不支持 UPDATE |
| `hana` | `Server=..;Port=30015;Uid=..;Pwd=..` | hdbconnect 原生协议（`hdb://`） |
| `firebird` | `Server=（缺省则内嵌）;Database=xx.fdb;Uid=SYSDBA;Pwd=..` | 运行时动态加载 `fbclient.dll`（可用 `FBCLIENT_LIB_DIR` 指定） |
| `db2` / `dameng` / `iris` / `access` | `Driver={...};...` 直通 ODBC；或 XCode 风格模板（缺省巴适） | 经 odbc-api 桥接本机 ODBC 驱动 |
| `mongodb` | `Server=..;Port=27017;Database=..`（或 `Uri=mongodb://...`） | SQL 子集翻译为文档操作；无 DDL（`sync_schema` 自动跳过） |
| `nova` | 同 MySQL | NovaDb 走 MySQL 协议（声明为 MySql 驱动） |
| `network` | `Server=http://..;Database=连接名;Password=令牌` | SQL 转发到远端 XCode DbServer（C# `DbServer` 或本仓 `examples/dbserver`）；打开即登录探明远端类型（占位符/分页按远端方言）；无事务转发、无本地结构迁移 |
| `sqlce` | — | 明确不支持（SSCE 已停止维护） |

- 这些库的方言（类型/DDL/分页/自增/引用）与连接串解析均已有单测；网络型驱动另单测覆盖值转换与语句翻译
- DuckDB 使用提示：① `:memory:` 内存库为“每连接独立”，多连接流程（先 `sync_schema` 再 `open_session`）请用**文件库**；
  ② 文件库同一文件不允许并存两个连接（含同进程），操作需串行（与 SQLite 的多进程并发方案不同）
- DuckDB 因内嵌无需外部实例，直接在每台机器跑 `cargo test --features duckdb` 即可（含内嵌引擎全链路用例）
- 其余网络库的端到端用例在本机有实例时可按环境变量门控启用（见 `tests/remote_e2e.rs`）
- **`network`** 端到端（Rust ↔ Rust 或 C# ↕ Rust）：先 `cargo run --example dbserver -- "Data Source=demo.db;Provider=SQLite" 3305 tk123`，
  再设 `RCODE_NETWORK="Server=http://127.0.0.1:3305;Database=Demo;Password=tk123;provider=network"` 跑 `cargo test --test remote_e2e network`；
  服务端也可换成 C# `DbServer`（`Service.Tokens["tk123"] = ["Demo"]`）

### 实体生成工具（对应 C# 的 `xcode` 命令）

```powershell
# 1) 先看模型里有哪些表
cargo run --bin rcodegen -- --model <你的项目>\Entity\Model.xml --list

# 2) 生成全部表的实体到指定目录（默认取模型 Output 配置，否则 ./entities）
cargo run --bin rcodegen -- --model <你的项目>\Entity\Model.xml --out src\entities

# 3) 只生成指定表；--dry-run 预览不写盘；--force 覆盖非本工具生成的文件
cargo run --bin rcodegen -- --model <你的项目>\Entity\Model.xml --out src\entities --table JiLiYu,VerifyCode
```

生成的每个表一个文件（如 `ji_li_yu.rs`），带“自动生成”标记：

- 默认**只覆盖带标记的文件**，手写代码不会被误删（需 `--force` 才能覆盖其它文件）
- 内容不变的不会重写（重复执行无副作用，适合进 CI）

### 对象实体用法（与 C# 侧 Entity 一致）

生成文件自带 `Entity` 实现（对象化增删改查），用法与 C# 的 `entity.Save()` 对应：

```rust
let mut order = Order::new();          // 等价 C# 的 new Order()
order.code = Some("HLT-001".into());
order.insert(&dal, session.as_mut())?; // 插入并回写自增主键
order.save(&dal, session.as_mut())?;   // Id=0 新增，否则按主键更新

let one  = Order::find(&dal, session.as_mut(), &[order.id.into()])?;      // 按主键查（对应 FindByID）
let list = Order::query(&dal, session.as_mut(),
    &Query::new().filter(Where::new().like("Code", "HLT%")).page(1, 20))?; // 条件 + 分页
let n    = Order::count(&dal, session.as_mut(), None)?;
order.delete(&dal, session.as_mut())?;
```

> 生成产物已用真实模型（`JiLiYu`、`VerifyCode`）做过编译与运行验证。

---

## 三、Model.xml 兼容性（迁移的关键）

- **同一份文件双端共用**：Rust 直接解析 C# 项目现有的 `Model.xml`（`https://newlifex.com/Model202509.xsd` 命名空间），无需转换
- **语义对齐**（已按 DH.NCode 源码逐一核对）：
  - `Nullable` 缺省 `false` → 建表追加 `NOT NULL`（与 XCode 一致；审计列需显式赋值）
  - `TableName` 缺省等于 `Name`；`ColumnName` 缺省等于 `Name`
  - SQLite 自增必须为主键：`INTEGER PRIMARY KEY AUTOINCREMENT`
  - SQLite 字符串列生成 `COLLATE NOCASE`（与 XCode 保持一致，保证大小写不敏感检索）
  - 未知属性 / 未知元素一律忽略，向后兼容新版本 XSD
- **可写出**：`EntityModel::to_xml()` 输出同规范 XML（解析 → 写出 → 解析 完全一致），可交回 C# 侧使用
- **已回归验证**：本仓库真实 `Model.xml`（176 表、3088 列、8 种数据类型）解析、建表、写出全部通过；
  **历史库副本共库验证**（2026-09-27）：52 张既存表增量同步零破坏（新建 168 表 / 补 8 列），176/176 表就位，CRUD 往返通过

### 多数据库类型映射（节选）

| DataType | SQLite | MySQL | SQL Server | PostgreSQL | Oracle |
|----------|--------|-------|------------|------------|--------|
| Int32 | `int`（自增为 `integer`） | `int` | `int` | `integer` | `number(10)` |
| Int64 | `integer` | `bigint` | `bigint` | `bigint` | `number(19)` |
| Decimal(18,4) | `decimal` | `decimal(18,4)` | `decimal(18,4)` | `numeric(18,4)` | `number(18,4)` |
| String(50) | `nvarchar(50)` | `varchar(50)` | `nvarchar(50)` | `varchar(50)` | `varchar2(50)` |
| String(不限) | `text` | `longtext` | `nvarchar(max)` | `text` | `clob` |
| DateTime | `datetime` | `datetime` | `datetime` | `timestamp` | `timestamp` |
| Boolean | `bit` | `TINYINT` | `bit` | `boolean` | `number(1)` |

其余方言差异（分页 `LIMIT/OFFSET` vs `OFFSET..FETCH`、占位符 `?` / `@pN` / `$N` / `:pN`、
标识符引用 `` ` `` / `[]` / `""`、自增 `AUTO_INCREMENT` / `IDENTITY(1,1)` / `GENERATED BY DEFAULT AS IDENTITY`）
均由 `dialect` 统一处理，并有针对性单测。

---

## 四、安全与可靠性

- **全参数绑定**：查询与写入不拼接字面量；`Where` 支持 `In/Between/Like/IsNull` 等，空 `IN` 集合会降级为恒真/恒假而非非法 SQL
- **标识符转义**：表名/列名按方言引用（内部引号双写），阻断标识符注入
- **迁移只增量**：`sync_schema` 仅建表与补列；补列时若为 `NOT NULL` 且无默认值会自动降级为可空，避免存量数据违约
- **与 C# 共存**：SQLite 打开时启用 WAL + 忙等待（5s），多进程读写与 C# 端互不阻塞

---

## 五、工程结构

```
Pek.RCode/
├── src/
│   ├── lib.rs        入口与概念对照（crate 文档）
│   ├── model.rs      Model.xml 解析 / 写出 / 子集
│   ├── types.rs      数据类型系统
│   ├── value.rs      值模型（DbValue）与时间文本（含 RFC3339 兼容）
│   ├── dialect.rs    多数据库方言（16 种库的类型/DDL/分页/自增/引用）
│   ├── session.rs    SqlSession 抽象与结果集
│   ├── sqlite.rs     SQLite 驱动
│   ├── mysql.rs      MySQL 驱动（XCode 连接串 / information_schema / LAST_INSERT_ID）
│   ├── mssql.rs      SQL Server 驱动（tiberius）
│   ├── postgres.rs   PostgreSQL 驱动（含 HighGo/金仓/VastBase）
│   ├── oracle.rs     Oracle 驱动（序列 SEQ_{表名}）
│   ├── duckdb.rs     DuckDB 驱动（内嵌，`--features duckdb`）
│   ├── clickhouse.rs ClickHouse 驱动（HTTP）
│   ├── tdengine.rs   TDengine 驱动（REST）
│   ├── influxdb.rs   InfluxDB 驱动（行协议 + InfluxQL）
│   ├── hana.rs       SAP HANA 驱动（hdbconnect）
│   ├── firebird.rs   Firebird 驱动（rsfbclient 动态加载）
│   ├── odbc.rs       ODBC 桥（DB2/达梦/IRIS/Access）
│   ├── mongodb.rs    MongoDB 驱动（SQL 子集翻译）
│   ├── http.rs       HTTP 驱动公共层（ureq3 / 字面量内联 / Base64）
│   ├── rt.rs         共享 tokio 运行时（异步驱动内部 block_on）
│   ├── sqlbuild.rs   INSERT/UPDATE/DELETE/SELECT/COUNT 组装
│   ├── query.rs      条件与查询描述
│   ├── dal.rs        连接串、Dal、结构同步、实体表操作
│   ├── cache.rs      实体缓存 / 单对象缓存（对应 Meta.Cache / Meta.SingleCache）
│   ├── reverse.rs    反向工程：数据库结构 → EntityModel / Model.xml（对应 DAL.GetTables）
│   ├── entity.rs     Entity trait（对象实体的 CRUD/查询默认实现）
│   ├── db_service.rs 远程服务层 + HTTP 客户端（DbServer/DbClient，DbTable v3 二进制互通）
│   ├── network.rs    provider=network 驱动（转发 SQL、登录探明远端类型、远端表结构探测）
│   ├── backup.rs     数据备份/恢复/同步（DbPackage 文件格式，与 C# 互认）
│   ├── meta.rs       在线库管理（建/删/存库，表/列/索引/注释 DDL）
│   ├── navigation.rs 导航属性注册表与装载（HasOne/HasMany）
│   ├── codegen.rs    Model.xml → Rust 对象实体
│   ├── bin/
│   │   └── rcodegen.rs   实体生成命令行工具
│   └── error.rs      统一错误
├── examples/
│   └── dbserver.rs   参考服务端宿主（DbService → 极简 HTTP，对齐 C# DbServer）
└── tests/
    ├── fixtures/wms_model_sample.xml   生产模型快照固件（7 张表 / 8 种类型）
    ├── fixtures/dbtable_v3_sample.bin  DbTable v3 二进制黄金样本（真实 C# NewLife.Core 生成）
    ├── entity_layer.rs                 对象实体端到端（insert/save/update/delete）
    ├── model_e2e.rs                    固件端到端（方言 DDL / SQLite CRUD / 代码生成）
    ├── mysql_e2e.rs                    MySQL 真实库端到端（RCODE_MYSQL 门控）
    ├── remote_e2e.rs                   PostgreSQL / SQL Server / Oracle / network 端到端（环境变量门控）
    ├── reverse_e2e.rs                  反向工程 roundtrip（固件建库 → 反向 → 逐列对比 → 再生成实体）
    └── live_sqlite_e2e.rs              真实 SQLite 历史库副本兼容性验证（RCODE_LIVE_DB + RCODE_MODEL 门控）
```

---

## 六、补迁移进度（按“完整功能迁移、可直接切换 C# 项目”标准）

**已完成（2026-09-27 补迁移批次）**：

1. **TLS** ✅ MySQL/PostgreSQL 内置 native-tls（缺省 Preferred/Prefer 可回退；Required/Require/VerifyCA/VerifyFull 分级校验；根证书 `SslCa`/`CertificateFile`/`Root Certificate`）；Oracle 支持 `Protocol=tcps`；**PEM 客户端证书** ✅ 新增 `tls-rustls` 后端（`--no-default-features --features tls-rustls`；MySQL `SslCert`/`SslKey`、PG `SSL Certificate`/`SSL Key` 生效）
2. **连接池** ✅ `pool`（对齐 C# `ConnectionPool`：Min=CPU(2–8)/Max=1000/空闲 30s；`Pooling=false` 关闭；`pool_stats`/`clear_pool`）
3. **多库反向工程与结构比对** ✅ `catalog` 覆盖除 MongoDB 外全部驱动（含索引/唯一约束）；`sync_schema` 为既存表补建缺失索引；新增 `diff_schema`（缺失/多余的表/列/索引 + 类型差异报告 + ALTER 脚本导出）
4. **异步门面** ✅ `async_dal`（tokio `spawn_blocking` 统一包装全驱动的同步内核；`run`/`with_session` 可覆盖全部同步 API，含表/实体操作）
5. **代码生成增强** ✅ 枚举字段映射（membership 已知枚举 → 真实 Rust 枚举；未知枚举按整型并注明）；`AuditExt` 审计字段访问；`#[derive(Entity)]` 属性宏（crate `pek-rcode-derive`）
6. **分布式缓存失效适配** ✅ `RedisVersionStore`（feature `redis`；底层为 **Pek.RRedis**（DH.NRedis 的 Rust 实现）自研客户端，连接串/键名与 C# 一致，跨语言可互相感知失效）
7. **DataSimulation** ✅ `simulation`（随机造数 + 分批事务 + TPS 统计）
8. **基础库下沉** ✅ 时间文本格式/解析、MD5 摘要、文本文件读写（BOM 兼容）等基础方法下沉到 **DH.RustBase**（crate `dhrust` 0.1.4）；Pek.RCode 与 Pek.RRedis 均已 path 依赖复用（不再各自内联实现）
9. **远程服务协议互通** ✅ `/Db/Query` 采用 NewLife **DbTable v3 二进制**（`dbtable` 模块：7 位压缩整数、大端浮点、Decimal 四元组、DateTime 刻度、`System.Byte[]`/`Guid`）；`DbService::query_packet` 输出报文、`DbClient::query_rowset` 自动识别二进制（C# `DbServer`）与 JSON（Rust 宿主）应答；黄金样本由**真实 C# NewLife.Core** 生成，编码**逐字节一致**、双向互读验证通过
10. **MSPageSplit（可选能力）** ✅ `PageStyle::RowNumber`（`Query::page_style`）：SQL Server 2005/2008 的 `ROW_NUMBER()` 双层分页（对齐 `MSPageSplit.RowNumber`，含无排序兜底）；DH.NCode 现行默认仍为 2012+ `OFFSET..FETCH`，Rust 默认行为与其保持一致
11. **`provider=network` 远程驱动** ✅ `network` 模块（对齐 `Database/Network.cs`：`Server`/`Database`/`Password` → `DbClient`，`Dal::open` 登录探明远端类型后委托其格式化/分页）；SQL 转发占位符改写为远端命名式（`@p0`/`:p0`/`?p0`，与 C# `FormatParameterName`/`ConvertParameters` 一致）、插入走远端 `Db/InsertAndGetIdentity`、`sync_schema` 不建表（对齐 `NetworkMetaData` 空实现）、事务明确拒绝；另附 `examples/dbserver` 参考宿主；**实机联调**：Rust ↔ Rust 与 **Rust ↔ 真实 C# `DbServer`**（本机 DH.NCode net10.0 产物）全链路双向通过（登录探明类型含 NewLife 数字枚举、转发建表、实体增删改查、自增回写、分页、事务拒绝；NULL 参数内联与表探测已按实测校正）；两处 **C# 侧已知缺陷**（已对拍定性）：① `ToPacket()` 对同列跨行混合存储类型（如 decimal 整数值行存 INTEGER）会抛 `InvalidCastException`（C# 源码注释已标注该问题）；② `DbClient.GetTablesAsync` 以 `GetAsync<String>` 请求返回 JSON 数组的 `Db/GetTables`，**C#↔C# 对拍亦抛** `Unable to convert to type [System.String]!`——Rust 服务端应答形状与 C# 服务端一致，不受影响
12. **`DAL_Backup`（备份/恢复/同步）** ✅ `backup` 模块：单表备份到 DbTable v3 文件（`.gz` 自动 GZip）、多表 zip 包（`{连接名}.xml` 模型 + `{实体名}.table`）、`restore`/`restore_all`（表名可从包内推导、`set_schema` 自动建表）、跨库 `sync_table`/`sync_all`；表头列名为实体属性名、行数上限 i32、NULL 折叠为类型默认值，均与 C# 一致；**C#↔Rust 双向实测互认**（C# `DbPackage` 导出 → Rust 恢复、Rust 备份 → C# 恢复，逐值核对一致，`RCODE_BACKUP_IMPORT`/`RCODE_BACKUP_EXPORT` 门控用例）
13. **`DbMetaData` 在线库管理** ✅ `meta` 模块：建库/删库/存在性（文件库=文件操作；SQL 库按方言语句与元数据查询，逐一对齐各驱动覆写）、建表/删表（Firebird 连带序列）、列增/改/删、索引建/删、表列注释（`Comment On`/`Alter .. Comment`/`sp_addextendedproperty`）；无能力库返回 `false`（对齐 C# 空语句）
14. **导航属性与行访问器** ✅ `navigation` 模块：`NavigationRegistry`（HasOne/HasMany，本地或进程级）+ `load_one`/`load_many` + `Entity::load`/`from_rows`（行集→实体，对应 `DataRowEntityAccessor.LoadData`）；C# 的 LINQ `Include`/反射注值在 Rust 无对应机制，以“注册表 + 显式装载”为对等能力面

**完整性审计缺口已全部落地（2026-09-27）**：

- 四项新发现（network 驱动 / DAL_Backup / DbMetaData / 导航与访问器）均已实现并测试（见上 11–14），无待补项

**边界确认（非待办）**：

- `HtmlBuilder`（Razor 页面）与 CubeBuilder/CustomBuilder（C# 框架产物）由 C# 侧 XCodeTool 继续使用；`Model.xml` 双端共用不受影响，Rust 侧以实体/模型/接口/搜索生成为对等能力

---

## 七、迁移建议（C# → Rust 渐进路径）

1. **只读先行**：Rust 侧先承接报表、统计、日志消费等只读任务（与 C# 共用 `DG.db`，WAL 并发无冲突）
2. **小写入试点**：从独立表（如设备日志、扫码记录）开始迁移写路径，验证双端行为一致
3. **保持模型唯一来源**：`Model.xml` 仍由 C# 侧维护（xcode 命令继续使用），Rust 每次构建读取同一文件
4. **切换业务用例**：一个用例（Controller/Action）一个用例地迁移，写路径尽量整体切换，避免双写一致性问题
5. **CI 建议**：在流水线中对目标库执行 `sync_schema()`（增量安全），并对比模型与库结构差异

---

## 八、许可

MIT
