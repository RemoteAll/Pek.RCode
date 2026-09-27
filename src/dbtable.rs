//! DbTable 二进制编解码（对齐 NewLife `DbTable` 的 v3 二进制格式）。
//!
//! C# 端 `DbController.Query` 返回 `DbTable.ToPacket()`，`DbClient.QueryAsync` 以
//! `Accept: application/octet-stream` 接收响应体后调用 `DbTable.Read(IPacket)`；
//! `ApiHelper.ProcessResponse` 对 `IPacket` 返回类型直接把响应体字节包装成 Packet
//! （无 JSON 信封）。本模块等价实现该格式（`Binary { FullTime=true, EncodeInt=true }`，
//! 未启用小端，故定长浮点为**大端**）：
//!
//! ```text
//! [14]  "NewLifeDbTable"（幻数，ASCII）
//! [1]   版本 = 3
//! [1]   保留标记 = 0
//! varint  列数 n
//! 每列：varint 列名长度 + UTF-8 列名 + 1 字节 .NET TypeCode
//!       （TypeCode=Object 时再跟 varint 类型全名，如 `System.Byte[]`）
//! [4]   行数（小端 Int32）
//! 行数据：逐行逐列按列类型编码
//! ```
//!
//! 类型编码（与 NewLife `Binary` 一致）：
//!
//! | 类型 | 编码 |
//! |------|------|
//! | `Boolean`/`Byte`/`SByte`/`Char` | 1 字节 |
//! | 整数族（`Int16`…`UInt64`） | 7 位压缩（LEB128）变长，负数为补码（Int32 负值 5 字节、Int64 负值最多 10 字节） |
//! | `Single`/`Double` | 4/8 字节**大端** |
//! | `Decimal` | 4 个 7 位压缩 Int32（低位、中位、高位、标志：`scale<<16 \| 符号位`） |
//! | `DateTime` | 7 位压缩 Int64，`DateTime.ToBinary()` 语义（100ns 刻度，高 2 位为 Kind） |
//! | `String` | varint 字节长度 + UTF-8 |
//! | `Byte[]`（Object） | varint 长度 + 原始字节 |
//! | `Guid`（Object） | 16 字节原始（.NET 混合端序） |
//!
//! **空值语义与 C# 一致**：`DbTable` 从库读取时就把 DBNull 折叠为类型默认值
//! （Int→0、String→""、DateTime→MinValue、Blob→空），二进制流中不保留 NULL 标记；
//! 本模块编码 NULL 时同样写类型默认值，解码时还原为默认值而非 `DbValue::Null`。
//!
//! 编码时列类型按列内值推断（NULL 忽略）：Blob > DateTime > Decimal > Float >
//! Text > Int（能装进 Int32 则用 Int32，否则 Int64）> Bool；整列皆 NULL 按 String。

use chrono::{NaiveDate, NaiveDateTime, TimeDelta, Timelike};
use rust_decimal::Decimal;

use crate::error::{Error, Result};
use crate::session::RowSet;
use crate::value::DbValue;

/// 幻数（ASCII，14 字节）。
pub const MAGIC: &[u8] = b"NewLifeDbTable";

/// 支持的最高版本（对齐 C# `DbTable._Ver`）。
pub const VERSION: u8 = 3;

/// .NET `TypeCode` 数值（仅覆盖本模块用到的取值）。
mod code {
    pub const OBJECT: u8 = 1;
    pub const BOOLEAN: u8 = 3;
    pub const CHAR: u8 = 4;
    pub const SBYTE: u8 = 5;
    pub const BYTE: u8 = 6;
    pub const INT16: u8 = 7;
    pub const UINT16: u8 = 8;
    pub const INT32: u8 = 9;
    pub const UINT32: u8 = 10;
    pub const INT64: u8 = 11;
    pub const UINT64: u8 = 12;
    pub const SINGLE: u8 = 13;
    pub const DOUBLE: u8 = 14;
    pub const DECIMAL: u8 = 15;
    pub const DATETIME: u8 = 16;
    pub const STRING: u8 = 18;
}

/// 二进制列类型（编码时推断；解码时来自报文头 TypeCode）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColumnType {
    Boolean,
    Byte,
    SByte,
    Char,
    Int16,
    UInt16,
    Int32,
    UInt32,
    Int64,
    UInt64,
    Single,
    Double,
    Decimal,
    DateTime,
    Text,
    Blob,
    Guid,
}

impl ColumnType {
    /// 报文头 TypeCode。
    fn type_code(self) -> u8 {
        match self {
            ColumnType::Boolean => code::BOOLEAN,
            ColumnType::Byte => code::BYTE,
            ColumnType::SByte => code::SBYTE,
            ColumnType::Char => code::CHAR,
            ColumnType::Int16 => code::INT16,
            ColumnType::UInt16 => code::UINT16,
            ColumnType::Int32 => code::INT32,
            ColumnType::UInt32 => code::UINT32,
            ColumnType::Int64 => code::INT64,
            ColumnType::UInt64 => code::UINT64,
            ColumnType::Single => code::SINGLE,
            ColumnType::Double => code::DOUBLE,
            ColumnType::Decimal => code::DECIMAL,
            ColumnType::DateTime => code::DATETIME,
            ColumnType::Text => code::STRING,
            ColumnType::Blob | ColumnType::Guid => code::OBJECT,
        }
    }

    /// Object 类型的全名（其余类型返回 None）。
    fn object_type_name(self) -> Option<&'static str> {
        match self {
            ColumnType::Blob => Some("System.Byte[]"),
            ColumnType::Guid => Some("System.Guid"),
            _ => None,
        }
    }
}

/// 判断字节数组是否为 DbTable 二进制报文（幻数匹配）。
/// <param name="bytes">响应体字节</param>
/// <returns>是否为 DbTable 二进制</returns>
pub fn is_dbtable(bytes: &[u8]) -> bool {
    bytes.starts_with(MAGIC)
}

// ==================== 编码 ====================

/// 将结果集编码为 DbTable v3 二进制报文。
/// <param name="set">结果集</param>
/// <returns>字节数组（HTTP 响应体）</returns>
pub fn encode_rowset(set: &RowSet) -> Vec<u8> {
    let columns = set.columns.as_ref();
    let types: Vec<ColumnType> = (0..columns.len())
        .map(|index| infer_column(set, index))
        .collect();

    let mut w = Writer::new();
    // 头部：幻数、版本、标记
    w.bytes(MAGIC);
    w.byte(VERSION);
    w.byte(0);

    // 列定义
    w.encoded_i32(columns.len() as i32);
    for (index, name) in columns.iter().enumerate() {
        w.string(name);
        let kind = types[index];
        w.byte(kind.type_code());
        if let Some(type_name) = kind.object_type_name() {
            w.string(type_name);
        }
    }

    // 行数（原始 4 字节小端，对齐 C# `Total.GetBytes()`）
    w.bytes(&(set.rows.len() as i32).to_le_bytes());

    // 行数据
    for row in &set.rows {
        let values = row.values();
        for (index, kind) in types.iter().enumerate() {
            write_cell(&mut w, *kind, values.get(index).unwrap_or(&DbValue::Null));
        }
    }

    w.buf
}

/// 按列内实际值推断列类型（NULL 忽略，见模块文档）。
fn infer_column(set: &RowSet, index: usize) -> ColumnType {
    let mut has_bool = false;
    let mut has_int = false;
    let mut min_int = i64::MAX;
    let mut max_int = i64::MIN;
    let mut has_float = false;
    let mut has_decimal = false;
    let mut has_text = false;

    for row in &set.rows {
        match row.values().get(index) {
            Some(DbValue::Bool(_)) => has_bool = true,
            Some(DbValue::Int(v)) => {
                has_int = true;
                min_int = min_int.min(*v);
                max_int = max_int.max(*v);
            }
            Some(DbValue::Float(_)) => has_float = true,
            Some(DbValue::Decimal(_)) => has_decimal = true,
            Some(DbValue::Text(_)) => has_text = true,
            Some(DbValue::Blob(_)) => return ColumnType::Blob,
            Some(DbValue::DateTime(_)) => return ColumnType::DateTime,
            _ => {}
        }
    }

    if has_decimal {
        ColumnType::Decimal
    } else if has_float {
        ColumnType::Double
    } else if has_text {
        ColumnType::Text
    } else if has_int {
        if min_int >= i32::MIN as i64 && max_int <= i32::MAX as i64 {
            ColumnType::Int32
        } else {
            ColumnType::Int64
        }
    } else if has_bool {
        ColumnType::Boolean
    } else {
        // 整列皆 NULL：按空文本编码（解码端得到空字符串）
        ColumnType::Text
    }
}

/// 写入一个单元格（NULL 写类型默认值，与 C# `DbTable` 语义一致）。
fn write_cell(w: &mut Writer, kind: ColumnType, value: &DbValue) {
    match kind {
        ColumnType::Boolean => w.byte(u8::from(matches!(value, DbValue::Bool(true)))),
        ColumnType::Byte => w.byte(value.as_i64().unwrap_or(0) as u8),
        ColumnType::SByte => w.byte(value.as_i64().unwrap_or(0) as i8 as u8),
        ColumnType::Char => w.byte(value.as_i64().unwrap_or(0) as u8),
        ColumnType::Int16 => w.encoded_u64(value.as_i64().unwrap_or(0) as i16 as u16 as u64),
        ColumnType::UInt16 => w.encoded_u64(value.as_i64().unwrap_or(0) as u16 as u64),
        ColumnType::Int32 => w.encoded_i32(value.as_i64().unwrap_or(0) as i32),
        ColumnType::UInt32 => w.encoded_u64(value.as_i64().unwrap_or(0) as u32 as u64),
        ColumnType::Int64 => w.encoded_i64(value.as_i64().unwrap_or(0)),
        ColumnType::UInt64 => w.encoded_u64(value.as_i64().unwrap_or(0) as u64),
        ColumnType::Single => {
            let v = value.as_f64().unwrap_or(0.0) as f32;
            w.bytes(&v.to_be_bytes());
        }
        ColumnType::Double => {
            let v = value.as_f64().unwrap_or(0.0);
            w.bytes(&v.to_be_bytes());
        }
        ColumnType::Decimal => {
            let (lo, mid, hi, flags) = value
                .as_decimal()
                .map(decimal_bits)
                .unwrap_or((0, 0, 0, 0));
            w.encoded_i32(lo as i32);
            w.encoded_i32(mid as i32);
            w.encoded_i32(hi as i32);
            w.encoded_i32(flags);
        }
        ColumnType::DateTime => {
            let ticks = match value {
                DbValue::DateTime(dt) => naive_to_ticks(dt).unwrap_or(0),
                _ => 0,
            };
            w.encoded_i64(ticks);
        }
        ColumnType::Text => {
            let text = match value {
                DbValue::Text(s) => s.clone(),
                DbValue::Bool(b) => (if *b { "True" } else { "False" }).to_string(),
                DbValue::Int(v) => v.to_string(),
                DbValue::Float(v) => v.to_string(),
                DbValue::Decimal(v) => v.to_string(),
                DbValue::DateTime(v) => crate::value::format_datetime(v),
                DbValue::Blob(b) => crate::http::to_hex(b),
                DbValue::Null => String::new(),
            };
            w.string(&text);
        }
        ColumnType::Blob => {
            let bytes: &[u8] = match value {
                DbValue::Blob(b) => b,
                _ => &[],
            };
            w.sized_bytes(bytes);
        }
        ColumnType::Guid => {
            // 编码端不会推断出 Guid；防御性写 16 字节零值
            w.bytes(&[0u8; 16]);
        }
    }
}

/// `Decimal` → .NET `Decimal.GetBits` 四元组（lo、mid、hi、flags）。
fn decimal_bits(value: Decimal) -> (u32, u32, u32, i32) {
    let negative = value.is_sign_negative();
    let mantissa = value.mantissa().unsigned_abs();
    let lo = mantissa as u32;
    let mid = (mantissa >> 32) as u32;
    let hi = (mantissa >> 64) as u32;
    let scale = value.scale() as i32;
    let flags = (scale << 16) | if negative { i32::MIN } else { 0 };
    (lo, mid, hi, flags)
}

// ==================== 解码 ====================

/// 将 DbTable v3 二进制报文解码为结果集。
/// <param name="bytes">响应体字节</param>
/// <returns>结果集</returns>
pub fn decode_rowset(bytes: &[u8]) -> Result<RowSet> {
    let mut r = Reader::new(bytes);

    let magic = r.take(MAGIC.len())?;
    if magic != MAGIC {
        return Err(Error::Db("不是 DbTable 二进制报文（幻数不匹配）".into()));
    }

    let version = r.byte()?;
    if version > VERSION {
        return Err(Error::Db(format!(
            "DbTable 版本 {version} 高于本实现支持的版本 {VERSION}"
        )));
    }
    // v3 起使用 FullTime（8 字节刻度）；更早版本 DateTime 为 1970 起的秒数
    let full_time = version >= 3;

    let _flag = r.byte()?;

    let count = r.encoded_u32()? as usize;
    let mut columns = Vec::with_capacity(count);
    let mut types = Vec::with_capacity(count);
    for _ in 0..count {
        columns.push(r.string()?);
        let type_code = r.byte()?;
        let kind = match type_code {
            code::BOOLEAN => ColumnType::Boolean,
            code::BYTE => ColumnType::Byte,
            code::SBYTE => ColumnType::SByte,
            code::CHAR => ColumnType::Char,
            code::INT16 => ColumnType::Int16,
            code::UINT16 => ColumnType::UInt16,
            code::INT32 => ColumnType::Int32,
            code::UINT32 => ColumnType::UInt32,
            code::INT64 => ColumnType::Int64,
            code::UINT64 => ColumnType::UInt64,
            code::SINGLE => ColumnType::Single,
            code::DOUBLE => ColumnType::Double,
            code::DECIMAL => ColumnType::Decimal,
            code::DATETIME => ColumnType::DateTime,
            code::STRING => ColumnType::Text,
            code::OBJECT => {
                let name = if version >= 2 { r.string()? } else { String::new() };
                match name.as_str() {
                    "System.Byte[]" => ColumnType::Blob,
                    "System.Guid" => ColumnType::Guid,
                    other => {
                        return Err(Error::Db(format!(
                            "暂不支持的 DbTable 列类型：System.Object({other})"
                        )));
                    }
                }
            }
            other => {
                return Err(Error::Db(format!("未知的 DbTable 列 TypeCode：{other}")));
            }
        };
        types.push(kind);
    }

    let row_bytes = r.take(4)?;
    let total = i32::from_le_bytes([row_bytes[0], row_bytes[1], row_bytes[2], row_bytes[3]]);

    let mut set = RowSet::new(columns);
    for _ in 0..total.max(0) {
        let mut values = Vec::with_capacity(types.len());
        for kind in &types {
            values.push(read_cell(&mut r, *kind, full_time)?);
        }
        set.push(values);
    }
    Ok(set)
}

/// 读取一个单元格。
fn read_cell(r: &mut Reader<'_>, kind: ColumnType, full_time: bool) -> Result<DbValue> {
    Ok(match kind {
        ColumnType::Boolean => DbValue::Bool(r.byte()? > 0),
        ColumnType::Byte => DbValue::Int(r.byte()? as i64),
        ColumnType::SByte => DbValue::Int(r.byte()? as i8 as i64),
        ColumnType::Char => DbValue::Text(char::from(r.byte()?).to_string()),
        ColumnType::Int16 => DbValue::Int(r.encoded_u32()? as u16 as i16 as i64),
        ColumnType::UInt16 => DbValue::Int(r.encoded_u32()? as u16 as i64),
        ColumnType::Int32 => DbValue::Int(r.encoded_u32()? as i32 as i64),
        ColumnType::UInt32 => DbValue::Int(r.encoded_u32()? as i64),
        ColumnType::Int64 => DbValue::Int(r.encoded_u64()? as i64),
        ColumnType::UInt64 => {
            let v = r.encoded_u64()?;
            if v <= i64::MAX as u64 {
                DbValue::Int(v as i64)
            } else {
                DbValue::Decimal(Decimal::from(v))
            }
        }
        ColumnType::Single => {
            let bytes = r.take(4)?;
            DbValue::Float(f32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as f64)
        }
        ColumnType::Double => {
            let bytes = r.take(8)?;
            DbValue::Float(f64::from_be_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            ]))
        }
        ColumnType::Decimal => {
            let lo = r.encoded_u32()?;
            let mid = r.encoded_u32()?;
            let hi = r.encoded_u32()?;
            let flags = r.encoded_u32()? as i32;
            let negative = flags < 0;
            let scale = ((flags >> 16) & 0x7F) as u32;
            if scale > 28 {
                return Err(Error::Db(format!("Decimal 标度非法：{scale}")));
            }
            DbValue::Decimal(Decimal::from_parts(lo, mid, hi, negative, scale))
        }
        ColumnType::DateTime => {
            if full_time {
                DbValue::DateTime(ticks_to_naive(r.encoded_u64()? as i64)?)
            } else {
                // v1/v2：1970 起的秒数（无小数秒）
                let seconds = r.encoded_u32()? as i64;
                let base = NaiveDate::from_ymd_opt(1970, 1, 1)
                    .and_then(|d| d.and_hms_opt(0, 0, 0))
                    .unwrap();
                DbValue::DateTime(
                    base.checked_add_signed(TimeDelta::try_seconds(seconds).ok_or_else(|| {
                        Error::Db("DateTime 秒数超出范围".into())
                    })?)
                    .ok_or_else(|| Error::Db("DateTime 秒数超出范围".into()))?,
                )
            }
        }
        ColumnType::Text => DbValue::Text(r.string()?),
        ColumnType::Blob => DbValue::Blob(r.sized_bytes()?),
        ColumnType::Guid => DbValue::Text(guid_to_text(r.take(16)?)),
    })
}

/// .NET `Guid(byte[16])`（混合端序）→ 标准文本形式。
fn guid_to_text(bytes: &[u8]) -> String {
    let data1 = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let data2 = u16::from_le_bytes([bytes[4], bytes[5]]);
    let data3 = u16::from_le_bytes([bytes[6], bytes[7]]);
    format!(
        "{data1:08x}-{data2:04x}-{data3:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
    )
}

// ==================== 刻度换算 ====================

/// 刻度起点：0001-01-01 00:00:00（.NET `DateTime.MinValue`）。
fn epoch_naive() -> NaiveDateTime {
    NaiveDate::from_ymd_opt(1, 1, 1)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .expect("0001-01-01 必为合法时间")
}

/// `NaiveDateTime` → `DateTime.ToBinary()` 刻度（Kind=Unspecified，即纯 100ns 刻度）。
fn naive_to_ticks(value: &NaiveDateTime) -> Result<i64> {
    let base = epoch_naive();
    let days = value.date().signed_duration_since(base.date()).num_days();
    let seconds = days
        .checked_mul(86_400)
        .and_then(|s| s.checked_add(value.time().num_seconds_from_midnight() as i64))
        .ok_or_else(|| Error::Db("时间超出 DbTable 刻度范围".into()))?;
    let nanos = value.and_utc().timestamp_subsec_nanos();
    seconds
        .checked_mul(10_000_000)
        .and_then(|t| t.checked_add((nanos / 100) as i64))
        .ok_or_else(|| Error::Db("时间超出 DbTable 刻度范围".into()))
}

/// `DateTime` 刻度 → `NaiveDateTime`（去掉高 2 位 Kind 标记）。
fn ticks_to_naive(ticks: i64) -> Result<NaiveDateTime> {
    let ticks = ticks & 0x3FFF_FFFF_FFFF_FFFF;
    let seconds = ticks.div_euclid(10_000_000);
    let nanos = ticks.rem_euclid(10_000_000) * 100;
    epoch_naive()
        .checked_add_signed(
            TimeDelta::try_seconds(seconds).ok_or_else(|| Error::Db("时间刻度超出范围".into()))?,
        )
        .and_then(|dt| dt.checked_add_signed(TimeDelta::nanoseconds(nanos)))
        .ok_or_else(|| Error::Db("时间刻度超出范围".into()))
}

// ==================== 写入器 / 读取器 ====================

/// NewLife Binary 语义的最小写入器（EncodeInt + 大端浮点）。
struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    fn new() -> Self {
        Self { buf: Vec::with_capacity(256) }
    }

    fn byte(&mut self, value: u8) {
        self.buf.push(value);
    }

    fn bytes(&mut self, value: &[u8]) {
        self.buf.extend_from_slice(value);
    }

    /// 7 位压缩无符号整数（LEB128，低 7 位在前，与 C# `WriteEncoded` 位序一致）。
    fn encoded_u64(&mut self, mut num: u64) {
        loop {
            if num >= 0x80 {
                self.byte((num as u8) | 0x80);
                num >>= 7;
            } else {
                self.byte(num as u8);
                break;
            }
        }
    }

    fn encoded_i32(&mut self, value: i32) {
        self.encoded_u64(value as u32 as u64);
    }

    fn encoded_i64(&mut self, value: i64) {
        self.encoded_u64(value as u64);
    }

    /// varint 长度 + UTF-8（对齐 C# `Write(String)`）。
    fn string(&mut self, value: &str) {
        self.sized_bytes(value.as_bytes());
    }

    /// varint 长度 + 原始字节（对齐 C# `Write(Byte[])`）。
    fn sized_bytes(&mut self, value: &[u8]) {
        self.encoded_i32(value.len() as i32);
        self.bytes(value);
    }
}

/// NewLife Binary 语义的最小读取器。
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn byte(&mut self) -> Result<u8> {
        let value = *self
            .data
            .get(self.pos)
            .ok_or_else(|| Error::Db("DbTable 报文意外结束".into()))?;
        self.pos += 1;
        Ok(value)
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(count)
            .filter(|end| *end <= self.data.len())
            .ok_or_else(|| Error::Db("DbTable 报文意外结束".into()))?;
        let slice = &self.data[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    /// 7 位压缩 → u32（对齐 C# `ReadEncodedInt32` 的累积语义）。
    fn encoded_u32(&mut self) -> Result<u32> {
        let mut result = 0u32;
        let mut shift = 0u32;
        loop {
            let byte = self.byte()?;
            result = result.wrapping_add(((byte & 0x7F) as u32) << shift);
            if byte & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
            if shift >= 32 {
                return Err(Error::Db("7 位压缩整数超出 32 位范围".into()));
            }
        }
    }

    /// 7 位压缩 → u64（对齐 C# `ReadEncodedInt64`）。
    fn encoded_u64(&mut self) -> Result<u64> {
        let mut result = 0u64;
        let mut shift = 0u32;
        loop {
            let byte = self.byte()?;
            result = result.wrapping_add(((byte & 0x7F) as u64) << shift);
            if byte & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
            if shift >= 64 {
                return Err(Error::Db("7 位压缩整数超出 64 位范围".into()));
            }
        }
    }

    /// varint 长度 + UTF-8。
    fn string(&mut self) -> Result<String> {
        let bytes = self.sized_bytes()?;
        String::from_utf8(bytes).map_err(|e| Error::Db(format!("DbTable 字符串不是合法 UTF-8：{e}")))
    }

    /// varint 长度 + 原始字节。
    fn sized_bytes(&mut self) -> Result<Vec<u8>> {
        let len = self.encoded_u32()? as usize;
        Ok(self.take(len)?.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDate;
    use rust_decimal::Decimal;

    use super::*;

    /// 与 C# 侧样本一致的表（`target/tmp/dbtable-verify` 的 `Program.cs`，由
    /// 真实 NewLife.Core `DbTable.ToPacket()` 生成黄金样本）。
    fn sample_set() -> RowSet {
        let dt = NaiveDate::from_ymd_opt(2026, 9, 27)
            .unwrap()
            .and_hms_milli_opt(12, 34, 56, 789)
            .unwrap();
        let mut set = RowSet::new(vec![
            "Id".into(),
            "Name".into(),
            "Flag".into(),
            "Rate".into(),
            "Amount".into(),
            "Created".into(),
            "Payload".into(),
        ]);
        set.push(vec![
            DbValue::Int(7),
            DbValue::Text("你好 NewLife".into()),
            DbValue::Bool(true),
            DbValue::Float(1.25),
            DbValue::Decimal("-1234567890.123456789".parse().unwrap()),
            DbValue::DateTime(dt),
            DbValue::Blob(vec![0, 1, 2, 254, 255]),
        ]);
        set
    }

    #[test]
    fn csharp_generated_fixture_matches_byte_for_byte() {
        // 黄金样本由真实 C# NewLife.Core 生成（tests/fixtures/dbtable_v3_sample.bin）
        let fixture = include_bytes!("../tests/fixtures/dbtable_v3_sample.bin");

        // 编码逐字节一致 ⇒ C# `DbClient` 可解析本实现发出的报文
        assert_eq!(encode_rowset(&sample_set()).as_slice(), fixture.as_slice());

        // 同一份字节的解码结果与样本值一致 ⇒ 本实现可解析 C# `DbServer` 的应答
        let back = decode_rowset(fixture).unwrap();
        assert_eq!(back.columns.as_ref(), sample_set().columns.as_ref());
        assert_eq!(back.rows[0].values(), sample_set().rows[0].values());
    }

    /// 环境变量 `RCODE_DBTABLE_EXPORT` 指向路径时导出样本（供 C# 工具 `read` 反向交叉验证）。
    #[test]
    fn export_sample_when_env_set() {
        let Ok(path) = std::env::var("RCODE_DBTABLE_EXPORT") else {
            return;
        };
        std::fs::write(path, encode_rowset(&sample_set())).unwrap();
    }

    /// 手工构造的黄金字节（2 列 × 1 行）：
    /// `Id`(Int32)=7、`Name`(String)="ab"。
    fn golden_bytes() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&[VERSION, 0]);
        bytes.extend_from_slice(&[0x02]); // 列数 2
        bytes.extend_from_slice(&[0x02, b'I', b'd', code::INT32]);
        bytes.extend_from_slice(&[0x04, b'N', b'a', b'm', b'e', code::STRING]);
        bytes.extend_from_slice(&1i32.to_le_bytes()); // 行数 1（小端）
        bytes.extend_from_slice(&[0x07]); // Id = 7
        bytes.extend_from_slice(&[0x02, b'a', b'b']); // "ab"
        bytes
    }

    #[test]
    fn golden_bytes_encode_and_decode() {
        let mut set = RowSet::new(vec!["Id".into(), "Name".into()]);
        set.push(vec![DbValue::Int(7), DbValue::Text("ab".into())]);

        // 编码与黄金字节逐字节一致（含 Int32 推断与 varint 细节）
        assert_eq!(encode_rowset(&set), golden_bytes());

        // 解码
        let back = decode_rowset(&golden_bytes()).unwrap();
        assert_eq!(back.columns.as_ref(), &vec!["Id".to_string(), "Name".to_string()]);
        assert_eq!(back.len(), 1);
        assert_eq!(back.rows[0].get(0), Some(&DbValue::Int(7)));
        assert_eq!(back.rows[0].get(1), Some(&DbValue::Text("ab".into())));
        assert!(is_dbtable(&golden_bytes()));
    }

    #[test]
    fn all_types_roundtrip() {
        let dt = NaiveDate::from_ymd_opt(2026, 9, 27)
            .unwrap()
            .and_hms_micro_opt(12, 34, 56, 789_012)
            .unwrap();
        let decimal: Decimal = "-1234567890.123456789".parse().unwrap();

        let mut set = RowSet::new(vec![
            "Flag".into(),
            "Small".into(),
            "Big".into(),
            "Rate".into(),
            "Amount".into(),
            "Created".into(),
            "Title".into(),
            "Payload".into(),
        ]);
        set.push(vec![
            DbValue::Bool(true),
            DbValue::Int(42),
            DbValue::Int(9_000_000_000),
            DbValue::Float(1.25),
            DbValue::Decimal(decimal),
            DbValue::DateTime(dt),
            DbValue::Text("你好 NewLife".into()),
            DbValue::Blob(vec![0, 1, 2, 254, 255]),
        ]);

        let bytes = encode_rowset(&set);
        let back = decode_rowset(&bytes).unwrap();

        assert_eq!(back.columns.as_ref(), set.columns.as_ref());
        assert_eq!(back.len(), 1);
        assert_eq!(back.rows[0].values(), set.rows[0].values());
    }

    #[test]
    fn nulls_follow_csharp_defaults() {
        // 与 C# DbTable 一致：NULL 折叠为类型默认值（不保留 Null）
        let dt = NaiveDate::from_ymd_opt(2026, 9, 27)
            .unwrap()
            .and_hms_opt(1, 2, 3)
            .unwrap();
        let mut set = RowSet::new(vec!["N".into(), "S".into(), "B".into(), "D".into()]);
        // 第一行全 NULL（类型由第二行推断）
        set.push(vec![DbValue::Null, DbValue::Null, DbValue::Null, DbValue::Null]);
        set.push(vec![
            DbValue::Int(5),
            DbValue::Text("x".into()),
            DbValue::Blob(vec![9]),
            DbValue::DateTime(dt),
        ]);

        let back = decode_rowset(&encode_rowset(&set)).unwrap();
        // NULL 行 → 类型默认值
        assert_eq!(back.rows[0].get(0), Some(&DbValue::Int(0)));
        assert_eq!(back.rows[0].get(1), Some(&DbValue::Text(String::new())));
        assert_eq!(back.rows[0].get(2), Some(&DbValue::Blob(Vec::new())));
        assert_eq!(back.rows[0].get(3), Some(&DbValue::DateTime(epoch_naive())));
        // 有值行不受影响
        assert_eq!(back.rows[1].get(0), Some(&DbValue::Int(5)));
        assert_eq!(back.rows[1].get(3), Some(&DbValue::DateTime(dt)));
    }

    #[test]
    fn malformed_packets_are_rejected() {
        // 幻数错误
        assert!(decode_rowset(b"not a dbtable").is_err());

        // 版本过高
        let mut bytes = golden_bytes();
        bytes[MAGIC.len()] = VERSION + 1;
        assert!(decode_rowset(&bytes).is_err());

        // 截断
        assert!(decode_rowset(&golden_bytes()[..20]).is_err());

        // 未知 Object 类型
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&[VERSION, 0]);
        bytes.push(1); // 列数 1
        bytes.push(0x01); // 列名长度 1
        bytes.push(b'X');
        bytes.push(code::OBJECT); // System.TimeSpan 不支持
        bytes.push(0x0F);
        bytes.extend_from_slice(b"System.TimeSpan");
        bytes.extend_from_slice(&0i32.to_le_bytes());
        let err = decode_rowset(&bytes).unwrap_err().to_string();
        assert!(err.contains("System.TimeSpan"), "{err}");
    }

    #[test]
    fn guid_decodes_as_mixed_endian_text() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&[VERSION, 0]);
        bytes.push(1); // 1 列
        bytes.push(0x02);
        bytes.extend_from_slice(b"Id");
        bytes.push(code::OBJECT);
        bytes.push(b"System.Guid".len() as u8);
        bytes.extend_from_slice(b"System.Guid");
        bytes.extend_from_slice(&1i32.to_le_bytes());
        bytes.extend_from_slice(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]);

        let set = decode_rowset(&bytes).unwrap();
        assert_eq!(
            set.rows[0].get(0),
            Some(&DbValue::Text("03020100-0504-0706-0809-0a0b0c0d0e0f".into()))
        );
    }

    #[test]
    fn datetime_ticks_match_dotnet_epoch() {
        // 0001-01-01 → 0 刻度；2026-09-27 12:34:56.7890120 手工核算
        assert_eq!(naive_to_ticks(&epoch_naive()).unwrap(), 0);

        let dt = NaiveDate::from_ymd_opt(1970, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        // .NET DateTime(1970,1,1).Ticks == 621355968000000000
        assert_eq!(naive_to_ticks(&dt).unwrap(), 621_355_968_000_000_000);
        assert_eq!(ticks_to_naive(621_355_968_000_000_000).unwrap(), dt);
    }
}
