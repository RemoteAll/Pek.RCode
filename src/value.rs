//! 值模型：数据库字段值的运行时表示（对应 DH.NCode 中的 `Object` 字段值与 `DbType` 转换）。
//!
//! 设计目标：
//! - 覆盖 Model.xml 支持的全部数据类型
//! - 与驱动层解耦（SQLite 驱动负责 `DbValue` ↔ 驱动原生值的互相转换）
//! - 提供 SQL 字面量渲染（建表默认值等场景）与诊断友好的字符串输出

use std::fmt;

use chrono::NaiveDateTime;
use rust_decimal::Decimal;

/// 时间文本格式/解析等基础方法已下沉到 DH 基础库（`DH.RustBase` / crate `dhrust`
/// 的 `times` 模块），此处转出以兼容既有调用方：
///
/// - [`format_datetime`]：7 位小数秒的 XCode 数据库格式（与 C# XCode 写入一致）；
/// - [`parse_datetime`]：多格式容错解析（含 ISO 8601 与纯日期）。
pub use dhrust::times::{
    DATETIME_FRACTION_DIGITS, DATETIME_SECONDS_FORMAT, format_datetime, parse_datetime,
};

/// 数据库字段值。
#[derive(Debug, Clone, PartialEq)]
pub enum DbValue {
    /// 空值（DBNull）
    Null,
    /// 布尔
    Bool(bool),
    /// 整数（Byte/Int16/Int32/Int64 统一承载）
    Int(i64),
    /// 浮点（Single/Double）
    Float(f64),
    /// 高精度小数（保留文本精度）
    Decimal(Decimal),
    /// 字符串
    Text(String),
    /// 二进制
    Blob(Vec<u8>),
    /// 时间
    DateTime(NaiveDateTime),
}

impl DbValue {
    /// 是否为空值。
    pub fn is_null(&self) -> bool {
        matches!(self, DbValue::Null)
    }

    /// 转为整数（数值类型直接转换，文本尝试解析）。
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            DbValue::Bool(v) => Some(i64::from(*v)),
            DbValue::Int(v) => Some(*v),
            DbValue::Float(v) => Some(*v as i64),
            DbValue::Decimal(v) => v.to_string().parse::<i64>().ok(),
            DbValue::Text(v) => v.trim().parse::<i64>().ok(),
            _ => None,
        }
    }

    /// 转为浮点。
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            DbValue::Bool(v) => Some(if *v { 1.0 } else { 0.0 }),
            DbValue::Int(v) => Some(*v as f64),
            DbValue::Float(v) => Some(*v),
            DbValue::Decimal(v) => v.to_string().parse::<f64>().ok(),
            DbValue::Text(v) => v.trim().parse::<f64>().ok(),
            _ => None,
        }
    }

    /// 转为布尔（0/1 与 "true"/"false" 均可）。
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            DbValue::Bool(v) => Some(*v),
            DbValue::Int(v) => Some(*v != 0),
            DbValue::Float(v) => Some(*v != 0.0),
            DbValue::Text(v) => match v.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "y" => Some(true),
                "0" | "false" | "no" | "n" => Some(false),
                _ => None,
            },
            _ => None,
        }
    }

    /// 转为 64 位整数之外的常用整数（实体字段映射用）。
    pub fn as_i32(&self) -> Option<i32> {
        self.as_i64().map(|v| v as i32)
    }

    /// 转为 16 位整数。
    pub fn as_i16(&self) -> Option<i16> {
        self.as_i64().map(|v| v as i16)
    }

    /// 转为单字节无符号整数。
    pub fn as_u8(&self) -> Option<u8> {
        self.as_i64().map(|v| v as u8)
    }

    /// 转为单精度浮点。
    pub fn as_f32(&self) -> Option<f32> {
        self.as_f64().map(|v| v as f32)
    }

    /// 转为高精度小数（文本按原样解析，保持精度）。
    pub fn as_decimal(&self) -> Option<Decimal> {
        match self {
            DbValue::Decimal(v) => Some(*v),
            DbValue::Int(v) => Some(Decimal::from(*v)),
            DbValue::Float(v) => v.to_string().parse().ok(),
            DbValue::Text(v) => v.trim().parse().ok(),
            _ => None,
        }
    }

    /// 取得二进制内容引用。
    pub fn as_blob(&self) -> Option<&[u8]> {
        match self {
            DbValue::Blob(v) => Some(v),
            _ => None,
        }
    }

    /// 转为字符串引用（仅文本/小数/时间等文本化表示可用）。
    pub fn as_str(&self) -> Option<&str> {
        match self {
            DbValue::Text(v) => Some(v),
            _ => None,
        }
    }

    /// 转为时间（DateTime 直接返回；文本尝试按 XCode 格式解析）。
    pub fn as_datetime(&self) -> Option<NaiveDateTime> {
        match self {
            DbValue::DateTime(v) => Some(*v),
            DbValue::Text(v) => parse_datetime(v),
            _ => None,
        }
    }

    /// 转文本（用于日志/展示；Null 输出空串）。
    pub fn to_text(&self) -> String {
        match self {
            DbValue::Null => String::new(),
            DbValue::Bool(v) => (if *v { "1" } else { "0" }).into(),
            DbValue::Int(v) => v.to_string(),
            DbValue::Float(v) => v.to_string(),
            DbValue::Decimal(v) => v.to_string(),
            DbValue::Text(v) => v.clone(),
            DbValue::Blob(v) => format!("[blob {} 字节]", v.len()),
            DbValue::DateTime(v) => format_datetime(v),
        }
    }

    /// 渲染为 SQL 字面量（用于 `DEFAULT` 等无法参数化的场景）。
    ///
    /// 注意：字符串会做单引号转义；业务查询请始终使用参数绑定。
    pub fn to_sql_literal(&self) -> String {
        match self {
            DbValue::Null => "NULL".into(),
            DbValue::Bool(v) => (if *v { "1" } else { "0" }).into(),
            DbValue::Int(v) => v.to_string(),
            DbValue::Float(v) => v.to_string(),
            DbValue::Decimal(v) => v.to_string(),
            DbValue::Text(v) => format!("'{}'", v.replace('\'', "''")),
            DbValue::Blob(v) => format!("X'{}'", v.iter().map(|b| format!("{b:02X}")).collect::<String>()),
            DbValue::DateTime(v) => format!("'{}'", format_datetime(v)),
        }
    }
}

impl fmt::Display for DbValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_text())
    }
}

macro_rules! impl_from {
    ($($t:ty => |$v:ident| $body:expr),* $(,)?) => {
        $(
            impl From<$t> for DbValue {
                fn from($v: $t) -> Self { $body }
            }
        )*
    };
}

impl_from! {
    bool => |v| DbValue::Bool(v),
    i8 => |v| DbValue::Int(v as i64),
    i16 => |v| DbValue::Int(v as i64),
    i32 => |v| DbValue::Int(v as i64),
    i64 => |v| DbValue::Int(v),
    u8 => |v| DbValue::Int(v as i64),
    u16 => |v| DbValue::Int(v as i64),
    u32 => |v| DbValue::Int(v as i64),
    usize => |v| DbValue::Int(v as i64),
    f32 => |v| DbValue::Float(v as f64),
    f64 => |v| DbValue::Float(v),
    Decimal => |v| DbValue::Decimal(v),
    String => |v| DbValue::Text(v),
    &str => |v| DbValue::Text(v.to_string()),
    Vec<u8> => |v| DbValue::Blob(v),
    &[u8] => |v| DbValue::Blob(v.to_vec()),
    NaiveDateTime => |v| DbValue::DateTime(v),
}

impl<T: Into<DbValue>> From<Option<T>> for DbValue {
    fn from(value: Option<T>) -> Self {
        match value {
            Some(v) => v.into(),
            None => DbValue::Null,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    #[test]
    fn conversions_between_kinds() {
        assert_eq!(DbValue::from(42).as_i64(), Some(42));
        assert_eq!(DbValue::from("42").as_i64(), Some(42));
        assert_eq!(DbValue::from(true).as_bool(), Some(true));
        assert_eq!(DbValue::from(1).as_bool(), Some(true));
        assert_eq!(DbValue::from("yes").as_bool(), Some(true));
        assert_eq!(DbValue::from(1.5).as_f64(), Some(1.5));
        assert_eq!(DbValue::Null.as_i64(), None);
        assert!(DbValue::Null.is_null());
    }

    #[test]
    fn entity_field_conversions() {
        // 实体映射使用的窄化转换
        assert_eq!(DbValue::from(300).as_i32(), Some(300));
        assert_eq!(DbValue::from(7).as_i16(), Some(7));
        assert_eq!(DbValue::from(255).as_u8(), Some(255));
        assert_eq!(DbValue::from(1.5).as_f32(), Some(1.5f32));
        assert_eq!(DbValue::from("12.34").as_decimal().unwrap().to_string(), "12.34");
        assert_eq!(DbValue::from(12).as_decimal().unwrap().to_string(), "12");
        assert_eq!(DbValue::from(vec![1u8, 2]).as_blob(), Some(&[1u8, 2][..]));
        assert_eq!(DbValue::Text("abc".into()).as_blob(), None);
    }

    #[test]
    fn decimal_keeps_precision() {
        let d: Decimal = "1234567890.123456789".parse().unwrap();
        let v = DbValue::from(d);
        assert_eq!(v.to_text(), "1234567890.123456789");
    }

    #[test]
    fn datetime_roundtrip_with_csharp_format() {
        // C# XCode 写入 SQLite 的典型文本
        let text = "2026-09-26 18:01:02.1230000";
        let dt = parse_datetime(text).expect("应能解析 7 位小数秒");
        assert_eq!(format_datetime(&dt), text);

        // 无小数秒与纯日期也应兼容
        assert!(parse_datetime("2026-09-26 18:01:02").is_some());
        assert_eq!(
            parse_datetime("2026-09-26").unwrap(),
            NaiveDate::from_ymd_opt(2026, 9, 26).unwrap().and_hms_opt(0, 0, 0).unwrap()
        );
        assert!(parse_datetime("").is_none());
    }

    #[test]
    fn sql_literals_are_escaped() {
        assert_eq!(DbValue::from("a'b").to_sql_literal(), "'a''b'");
        assert_eq!(DbValue::from(3).to_sql_literal(), "3");
        assert_eq!(DbValue::from(true).to_sql_literal(), "1");
        assert_eq!(DbValue::Null.to_sql_literal(), "NULL");
        assert_eq!(DbValue::from(vec![0x0A, 0xFFu8]).to_sql_literal(), "X'0AFF'");
    }

    #[test]
    fn option_into_dbvalue() {
        let v: DbValue = Some(5i32).into();
        assert_eq!(v, DbValue::Int(5));
        let v: DbValue = Option::<i32>::None.into();
        assert_eq!(v, DbValue::Null);
    }
}
