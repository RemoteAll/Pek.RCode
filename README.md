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
| `dialect` | 各 `DbBase` 子类（SQLite.cs / MySql.cs / …） | ✅ 五种库的类型映射 / DDL / 分页 / 自增 / 标识符与占位符 |
| `session` | `IDbSession` | ✅ 抽象就绪，驱动可插拔 |
| `sqlite` | `SQLite.cs` | ✅ **可执行**（rusqlite 内嵌，无外部依赖） |
| MySQL / SQL Server / PostgreSQL / Oracle | 同名驱动 | 🚧 方言已就绪，**驱动规划中**（可用于生成脚本） |
| `sqlbuild` | `InsertBuilder` / `SelectBuilder` | ✅ INSERT/UPDATE/DELETE/SELECT/COUNT |
| `query` | `WhereExpression` / `PageParameter` | ✅ 链式条件 + 分页/取前 N |
| `dal` | `DAL` / 迁移 Migration | ✅ 连接串解析、结构同步（建表/补列）、实体表操作 |
| `entity` | `Entity` 基类（对象实体） | ✅ `insert / save / update / delete / find / query / count` |
| `codegen` | `xcode` 命令（XCodeTool） | ✅ `Model.xml` → Rust **对象实体**（结构体 + `Entity` 实现 + `new()/Default`） |
| `rcodegen` 工具 | `xcode` 命令行 | ✅ 独立生成工具（`--list / --table / --dry-run / --force`） |

测试：**53 项全部通过**，其中包括生产模型快照固件（7 张真实表，覆盖全部 8 种数据类型）的端到端回归，
以及用真实表（JiLiYu、VerifyCode）生成实体后的编译与运行验证；
另可用环境变量 `RCODE_MODEL` 对完整生产 `Model.xml` 跑全量回归（见下文）。

---

## 二、快速开始

```rust
use pek_rcode::{Dal, EntityModel, Query, Where};

// 1) 复用 C# 项目中的 Model.xml（路径指向实体项目里的 Entity/Model.xml 即可）
let model = EntityModel::load(std::path::Path::new("Model.xml"))?;

// 2) 打开数据库：连接串与 XCode 格式一致
let dal = Dal::open_with_model("Data Source=..\\..\\Data\\DG.db;Provider=SQLite;ShowSql=false", model)?;

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
cargo test          # 53 项测试（固件回归 + 对象实体端到端）
cargo clippy        # 零警告
```

### 测试数据与全量回归

- `tests/fixtures/wms_model_sample.xml`：生产模型快照固件（7 张真实表，覆盖全部 8 种数据类型），默认测试均基于它
- 需要验证**完整生产模型**（176 张表规模）时，用环境变量指向完整 `Model.xml`：

```powershell
$env:RCODE_MODEL = "<你的项目>\Entity\Model.xml"
cargo test full_model      # 解析全量模型 + 全部表同步到临时 SQLite 库验证
```

> 依赖镜像：工程内 `.cargo/config.toml` 已配置 rsproxy。

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
- **已回归验证**：本仓库真实 `Model.xml`（176 表、3088 列、8 种数据类型）解析、建表、写出全部通过

### 多数据库类型映射（节选）

| DataType | SQLite | MySQL | SQL Server | PostgreSQL | Oracle |
|----------|--------|-------|------------|------------|--------|
| Int32 | `int`（自增为 `integer`） | `int` | `int` | `integer` | `number(10)` |
| Int64 | `integer` | `bigint` | `bigint` | `bigint` | `number(19)` |
| Decimal(18,4) | `decimal` | `decimal(18,4)` | `decimal(18,4)` | `numeric(18,4)` | `number(18,4)` |
| String(50) | `nvarchar(50)` | `varchar(50)` | `nvarchar(50)` | `varchar(50)` | `varchar2(50)` |
| String(不限) | `text` | `longtext` | `nvarchar(max)` | `text` | `clob` |
| DateTime | `datetime` | `datetime` | `datetime` | `timestamp` | `timestamp` |
| Boolean | `bit` | `bit` | `bit` | `boolean` | `number(1)` |

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
│   ├── value.rs      值模型（DbValue）与时间文本
│   ├── dialect.rs    多数据库方言（类型/DDL/分页/自增/引用）
│   ├── session.rs    SqlSession 抽象与结果集
│   ├── sqlite.rs     SQLite 驱动
│   ├── sqlbuild.rs   INSERT/UPDATE/DELETE/SELECT/COUNT 组装
│   ├── query.rs      条件与查询描述
│   ├── dal.rs        连接串、Dal、结构同步、实体表操作
│   ├── entity.rs     Entity trait（对象实体的 CRUD/查询默认实现）
│   ├── codegen.rs    Model.xml → Rust 对象实体
│   ├── bin/
│   │   └── rcodegen.rs   实体生成命令行工具
│   └── error.rs      统一错误
└── tests/
    ├── fixtures/wms_model_sample.xml   生产模型快照固件（7 张表 / 8 种类型）
    ├── entity_layer.rs                 对象实体端到端（insert/save/update/delete）
    └── model_e2e.rs                    固件端到端（方言 DDL / SQLite CRUD / 代码生成）
```

---

## 六、路线图（按优先级）

1. **MySQL 驱动**（本项目生产库是阿里云 RDS MySQL）——计划用 `mysql_async` 或 `sqlx`，含 TLS 选项与连接池
2. **SQL Server / PostgreSQL 驱动**（tiberius / tokio-postgres 或 sqlx），复用现有方言层
3. **异步门面**：为 tokio 应用提供 `AsyncDal`（连接池 + 全链路 async），与扫码枪网关等 tokio 服务对接
4. **反向工程**：数据库 → `Model.xml`（对应 XCode `GetTables`），支持历史库生成模型
5. **结构比对增强**：索引差异、类型差异检测与 ALTER 脚本导出（dry-run 输出）
6. **代码生成增强**：枚举类型、审计字段基类、`#[derive(Entity)]` 属性宏（对象实体基础版已就绪）
7. **缓存**：实体缓存与二级缓存（XCode 的 EntityCache 对应物）
8. **高级能力**：批量写、分表（Shards）、TDengine/时序扩展等（按需）

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
