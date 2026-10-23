# MySQL / MariaDB 调优指南（Pek.RCode · 物联网场景）

> 适用：Rust 侧用 `pek_rcode`（provider=mysql）写 MySQL/MariaDB 的物联网服务。
> 本文所有数字都是**本机实测**（MariaDB 11.8.9，本地回环，10 列文本表，release 构建），
> 复现命令见文末；服务端参数部分标注了「与 C# DH.NCode 的关系」，混跑时请一起看。

---

## 一、实测基线（先看这个，决定优化方向）

| 场景 | 单行/次耗时 | 吞吐 |
|------|------------:|-----:|
| `SELECT 1`（纯网络往返基线） | 92 µs | 10,833 op/s |
| 单行 INSERT · **事务外**（每次 autocommit 刷盘） | **677 µs** | **1,477 行/s** |
| 单行 INSERT · 事务内 | 109 µs | 9,196 行/s |
| 批量 INSERT 100/批（事务内） | 5,457 ns | 183,242 行/s |
| 批量 INSERT 200/批 | **4,264 ns** | 234,496 行/s |
| 批量 INSERT 500/批（**框架默认**） | 3,861~6,570 ns | 152,199~259,000 行/s |
| 批量 INSERT 1000/批 | 4,752 ns | 210,420 行/s |
| 批量 INSERT 2000/批 | 5,307 ns | 188,446 行/s |
| 批量 INSERT 5000/批（旧默认） | 7,687 ns | 130,091 行/s |
| 按主键 UPDATE（事务内） | 80 µs | 12,440 op/s |
| 按主键 SELECT（事务内） | 75 µs | 13,357 op/s |
| 条件 SELECT LIMIT 10 | 90 µs | 11,050 op/s |

**读出来的三件事：**

1. **单语句 ~90 µs 几乎全是网络往返**（本地回环！），框架侧只有 ~1 µs —— 优化 SQL 字符串对 MySQL 意义不大，
   **减少往返次数**才是关键。远程网络（RTT 0.5~2 ms）时这个差距会再放大 5~20 倍。
2. **事务外单行写入只有 1,477 行/秒**（每次 COMMIT 一次 fsync ≈ 570 µs）；**批量 + 事务是 259,000 行/秒（175×）**。
3. **批大小不是越大越好**：5000/批 反而比 500/批 慢一倍（单语句序列化/解析成本反超往返摊销）。
   框架已**按方言给默认值**：MySQL = 500，其余 = 5000（对齐 C#）。

### 1.1 往返经济学（每行几次往返 = 每行多少 µs）

以本地回环 **~104 µs/往返** 计（远程按 RTT 等比放大）：

| 写法 | 往返/行 | 实测 ns/行 | 行/秒 | 结论 |
|------|--------:|-----------:|------:|------|
| 逐条 `save`/`update` | 1~2 | 104,000~212,000 | 4.7k~9.6k | ❌ IoT 不可用 |
| `EntityQueue::upsert` 逐条（**旧实现**） | 2.3 | **212,285** | 4,711 | ❌ |
| `EntityQueue::upsert` 批量（**现在**） | 0.1 | **9,476** | **105,524** | ✅ 22× |
| `save_batch` 已存在行（**旧实现**逐条 update） | 1.1 | **104,534** | 9,566 | ❌ |
| `save_batch` 已存在行（**现在**多行 Upsert） | 0.1 | **10,686** | **93,581** | ✅ 9.8× |
| `Entity::upsert_batch`（多行 `ON DUPLICATE KEY`） | 0.1 | 6,306 | 158,580 | ✅ 最优 |
| `insert_batch` / `write_batch`（500/批） | 0.002 | ~4,000 | ~230,000 | ✅ 最优 |

> **结论：IoT 状态上报/设备影子，一律用「批量 Upsert」而不是逐条 save。**
> 框架已把 `EntityQueue` 的 Upsert 段与 `save_batch` 的已存在行都改成多行 Upsert（每批 1 次往返）。

---

## 二、写入路径的三条铁律

| # | 做法 | 效果（实测） |
|---|------|-------------|
| 1 | **一律批量**：`insert_batch` / `write_batch` / `EntityQueue`（`insert_static` + `flush`） | 1,477 → **259,000 行/秒** |
| 2 | **批量包在一个事务里**（`begin` → 多次 `insert_batch` → `commit`） | 每次 COMMIT 一次刷盘，事务内含 N 批 |
| 3 | **服务端刷盘策略**放宽（见下节 `innodb_flush_log_at_trx_commit=2`） | 掉电最多丢 1 秒；写入吞吐再上一个台阶 |

> 反例：每行 `entity.save()`（2 次往返 + 1 次刷盘）在 IoT 规模下必然成为瓶颈。

---

## 三、服务端参数（`my.cnf` / `my.ini`，**重启生效**）

### 3.0 无法修改 `my.cnf` 时（托管数据库 / 云数据库 / 运维冻结）

很多环境拿不到 `my.cnf`。此时**服务端参数只能接受**，把优化全部放到客户端 —— 下面这些是仍然有效的手段：

| 手段 | 效果 | 说明 |
|------|------|------|
| **批量写**（`insert_batch` / `upsert_batch` / `EntityQueue`） | 1,477 → **143,000~230,000 行/秒** | 把「每行一次 COMMIT 刷盘」变成「每 500 行一次」，是本场景**唯一的大杠杆** |
| **连接串 `InitCommand=`** | 会话级参数可调 | `SET SESSION innodb_lock_wait_timeout=5`、`SET SESSION sql_mode=...`、`SET SESSION transaction_isolation='READ-COMMITTED'` 等**会话级**参数即便没有 my.cnf 权限也能改 |
| **连接串 `PoolMaxSize`** | 避免打满数据库 | 按 `max_connections / 实例数 × 0.8` 设；默认 1000 极易撞上限 |
| **连接串 `PoolIdleTime` / `PoolMinSize`** | 避免反复建连 | 空闲超时默认 30s 对低频 IoT 太短（每请求重建连 ≈ 3~4 次往返）；建议 300s + `PoolMinSize=n` 预热 |
| **连接串 `StatementCacheSize`** | 避免多出 prepare 往返 | 默认 256；表多/SQL 形态多时调大 |
| **`SslMode=None`（内网）** | 省 TLS 握手与加解密 | 公网必须保留 `Required`/`VerifyFull` |
| **表设计**（见 §6） | 顺序主键、索引最小化 | 与 my.cnf 无关，始终有效 |
| **大事务**：`begin` → 多批 → `commit` | 把 570 µs 的 fsync 摊到整批 | 但事务别跨网络调用、别超过几秒 |

> 只有「一次 COMMIT 的 fsync 成本」无法在客户端消除 —— 它只能被**批量摊薄**（500 行/批时摊到 1.1 µs/行）。

### 3.1 写入吞吐（IoT 最关键，**能改 my.cnf 时**）

```ini
[mysqld]
# 每秒刷一次 redolog（0=不刷，1=每次提交刷，2=每秒刷）——2 是 IoT 常用折中：
# 掉电最多丢 1 秒已提交事务，但写入吞吐提升数倍（单行写入 677µs → 接近事务内水平）
innodb_flush_log_at_trx_commit = 2

# binlog 同步策略（有 binlog 时）：1000 = 每 1000 个事务刷一次；纯上报库可直接 log_bin=OFF
sync_binlog = 1000

# redolog 大小与缓冲：IoT 写入密集，日志文件太小会频繁 checkpoint
innodb_log_file_size = 1G          # 单文件；总大小 = 该值 × innodb_log_files_in_group
innodb_log_buffer_size = 64M

# Linux 上绕过 OS page cache（减少双缓冲内存占用与 fsync 抖动；Windows 不支持）
innodb_flush_method = O_DIRECT

# SSD：提高后台刷脏能力
innodb_io_capacity = 2000
innodb_io_capacity_max = 4000
```

### 3.2 内存与连接

```ini
innodb_buffer_pool_size = 8G        # 物理内存的 50~70%（专用库）；命中率决定读性能
innodb_buffer_pool_instances = 8    # ≥8G 时建议 4~8 个实例，降低争用
max_connections = 500               # 与客户端连接池上限匹配（见 §5）
table_open_cache = 4000
innodb_open_files = 4000
innodb_lock_wait_timeout = 10       # 行锁等待；IoT 短事务下可到 5（客户端也可会话级设）
```

### 3.3 与 C# DH.NCode 混跑注意

- 以上都是**服务端**参数，与客户端语言无关 —— C# 与 Rust 共享同一实例时收益相同；
- `sql_mode` 保持默认（含 `STRICT_TRANS_TABLES`）即可；如需放宽请两端一致，避免一侧报错一侧静默截断；
- 时间精度：MySQL `DATETIME` 默认 0 位小数秒（C# 生成表结构同样是 `DATETIME`），
  Rust 侧写入的毫秒/微秒会被截断；需要毫秒请显式建 `datetime(3)` 列（会让表结构与 C# 生成表不同）。

---

## 四、客户端（Pek.RCode）配置

### 4.1 连接串

```text
Server=10.0.0.5;Port=3306;Database=iot;Uid=app;Pwd=***;provider=mysql;
SslMode=None;Timeout=15;MaxPoolSize=64;
StatementCacheSize=256;
InitCommand=SET SESSION innodb_lock_wait_timeout=5;SET SESSION transaction_isolation='READ-COMMITTED'
```

| 键 | 默认 | 说明 |
|----|------|------|
| `Server` / `Port` / `Database` / `Uid` / `Pwd` | — / 3306 | 与 C# 同名 |
| `SslMode` | `Preferred` | `None`/`Preferred`/`Required`/`VerifyCA`/`VerifyFull` |
| `Timeout` | 15s | 连接超时 |
| `MaxPoolSize` | 1000 | **连接池空闲上限**，不是并发上限；MySQL `max_connections` 通常远小于它，务必调小 |
| `StatementCacheSize` | **256** | 预编译语句缓存（每连接）；表多、SQL 形态多时调大，容量不足会多出 prepare 往返 |
| `InitCommand` | 空 | 会话初始化命令（分号分隔），连接建立后执行；适合 `SET SESSION ...` 级调优 |
| `Charset` | `utf8mb4` | 会话字符集 |

### 4.2 批量写入 API

```rust
// ① 表级（推荐）：批大小默认按方言（MySQL 500），一次调用完成分块 + 多行 VALUES
let rows: Vec<Vec<(&str, DbValue)>> = items.iter().map(|it| it.fields()).collect();
session.begin()?;
table.write_batch(session.as_mut(), &rows, BatchWriteMode::Insert, None)?;  // None = 方言默认
session.commit()?;

// ② 实体级：insert_batch / save_batch / upsert_batch（自动处理自增列、拦截器、分表路由）
//    - 新增为主 → insert_batch；存在则覆盖全部列 → upsert_batch（1 次往返/批）
//    - save_batch 内部对「已存在行」也走多行 Upsert（自增表可能有自增间隙，行内容一致）
Order::insert_batch(&dal, session.as_mut(), &list, None)?;
Order::upsert_batch(&dal, session.as_mut(), &list, None)?;

// ③ 队列级（高吞吐日志/上报）：入队凑批 + 由调用方定时 flush
//    Insert 段与 Upsert 段都已合并为多行语句（每批 1 次往返）
queue.insert_static(&dal, session.as_mut(), &fields)?;   // 静态列名，零列名分配
queue.upsert_tracked_static(&dal, session.as_mut(), &pk, &fields, None)?;
queue.flush(&dal, session.as_mut())?;                    // 达到批大小也会自动 flush
```

**务必避免**：`for item in list { entity.save(...) }`（每行 2 次往返 + 1 次刷盘）。

### 4.3 事务

- 批量写入**建议显式事务**（多批一次提交）；
- 事务保持短（IoT 消息处理内完成），不要跨网络调用；
- 单条语句本身是原子的；批量分块失败不会自动回滚已执行分块 —— 需要原子性就自己包事务。

### 4.4 读取

- **按主键查**：`Entity::find`（单对象缓存，`MaxCacheCount>0` 时）；
- **配置/字典小表**：`find_cached_by`（整表缓存，零 SQL）；
- **批量点查**：`BatchFinder`（把 N 次点查合并成少量 `IN`）—— 单次往返 90 µs，合并收益巨大；
- 宽表**只取需要的列**（`Query::select`），减少传输与解码。

---

## 五、连接池

| 项 | 建议 |
|----|------|
| `MaxPoolSize` | 每进程 16~64（IoT 服务多实例部署时按总量算）；必须 ≤ MySQL `max_connections` / 实例数 |
| 会话使用 | 一个请求/消息占一个会话，**不要跨线程共享会话** |
| 长连接 | 池内复用（框架已按线程分片，8 线程争用劣化仅 1.28×） |

> 常见故障：`MaxPoolSize` 保持默认 1000，多实例部署把 MySQL 打满 → 连接被拒。
> 计算式：`实例数 × 每实例 MaxPoolSize < max_connections × 0.8`。

---

## 六、表设计（写入性能的决定因素）

| 项 | 做法 | 原因 |
|----|------|------|
| 主键 | **自增 `int/bigint`** 或雪花 `bigint` | 顺序写入，避免页分裂；随机 UUID 主键在 IoT 写入下是灾难 |
| 索引 | 只建必要的；高频过滤列建组合索引（最左前缀） | 每个二级索引都是一次额外写入 |
| 大字段 | 拆到独立表（或 `TEXT/BLOB` 单独表） | 避免行溢出与 buffer pool 污染 |
| 时间列 | 需要毫秒用 `datetime(3)`；只到秒用 `datetime`（与 C# 一致） | 精度越高占用越大 |
| 分表 | 按天/月分表用 `DataScale="timeShard:..."` | 单表过大时写入/查询都会退化；框架自动路由 |
| 只写表 | 日志/事件表标 `InsertOnly="true"` | 跳过存在性查询，直接插入 |

---

## 七、诊断

```sql
-- 当前连接与慢 SQL
SHOW FULL PROCESSLIST;
SHOW GLOBAL STATUS LIKE 'Threads_%';
SHOW GLOBAL STATUS LIKE 'Innodb_log_waits';        -- >0 说明 redolog 缓冲不足
SHOW ENGINE INNODB STATUS\G                          -- 看 LATEST DETECTED DEADLOCK / 检查点

-- 打开慢查询日志（超过 200ms 的语句）
SET GLOBAL slow_query_log = ON;
SET GLOBAL long_query_time = 0.2;
```

---

## 八、复现本文数据

```powershell
# 1) 起本地绿色 MariaDB（数据在 target/mysql-test，可一键清理）
pwsh scripts/mysql-test-env.ps1 start

# 2) 跑基准（写/读/批大小/事务对比）
$env:RCODE_MYSQL = "Server=127.0.0.1;Port=3399;Database=rcode_test;Uid=root;Pwd=root123;provider=mysql;SslMode=None"
cargo test --release --test mysql_perf -- --ignored --nocapture

# 3) 功能端到端（含 InitCommand/StatementCacheSize 验证）
cargo test --test mysql_e2e

# 3b) 往返经济学对比（逐条 vs 批量 Upsert）
cargo test --release --test mysql_perf_roundtrip -- --ignored --nocapture

# 4) 清理
pwsh scripts/mysql-test-env.ps1 clean
```
