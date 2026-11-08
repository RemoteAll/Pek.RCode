//! 数据类型系统：与 DH.NCode/XCode 的 `DataType` 名称保持兼容。
//!
//! Model.xml 中 `Column@DataType` 使用 CLR 类型名（`Int32`/`String`/`DateTime` 等），
//! 本模块提供同名的 Rust 枚举与互转；各数据库的具体字段类型见 [`crate::dialect`]。

use crate::error::{Error, Result};

/// 字段数据类型（名称与 C# CLR 类型一一对应）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DataType {
    /// 布尔（C# `Boolean`）
    Boolean,
    /// 单字节整数（C# `Byte`）
    Byte,
    /// 16 位整数（C# `Int16`）
    Int16,
    /// 32 位整数（C# `Int32`）
    Int32,
    /// 64 位整数（C# `Int64`）
    Int64,
    /// 单精度浮点（C# `Single`）
    Single,
    /// 双精度浮点（C# `Double`）
    Double,
    /// 高精度小数（C# `Decimal`）
    Decimal,
    /// 字符串（C# `String`）
    String,
    /// 时间（C# `DateTime`）
    DateTime,
    /// 二进制（C# `Byte[]`）
    Binary,
    /// JSON 文本（MySQL `JSON` / PostgreSQL `jsonb` / DuckDB `JSON`）
    ///
    /// 建表时映射到各数据库的 JSON 类型；无原生 JSON 类型的库退化为文本大字段。
    /// 读写值仍是 [`crate::value::DbValue::Text`]（JSON 原文），字段仍以 `String` 承载，
    /// 因此**与 `String` 在值语义上完全等价**，区别只在于建表类型与反向工程识别。
    Json,
}

impl DataType {
    /// 全部类型（用于遍历/自检）。
    pub const ALL: [DataType; 12] = [
        DataType::Boolean,
        DataType::Byte,
        DataType::Int16,
        DataType::Int32,
        DataType::Int64,
        DataType::Single,
        DataType::Double,
        DataType::Decimal,
        DataType::String,
        DataType::DateTime,
        DataType::Binary,
        DataType::Json,
    ];

    /// 标准名称（与 Model.xml 中的 `DataType` 属性一致）。
    pub fn name(&self) -> &'static str {
        match self {
            DataType::Boolean => "Boolean",
            DataType::Byte => "Byte",
            DataType::Int16 => "Int16",
            DataType::Int32 => "Int32",
            DataType::Int64 => "Int64",
            DataType::Single => "Single",
            DataType::Double => "Double",
            DataType::Decimal => "Decimal",
            DataType::String => "String",
            DataType::DateTime => "DateTime",
            DataType::Binary => "Binary",
            DataType::Json => "Json",
        }
    }

    /// 从名称解析（宽松：忽略大小写，兼容常见别名）。
    pub fn from_name(name: &str) -> Result<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "boolean" | "bool" => Ok(DataType::Boolean),
            "byte" => Ok(DataType::Byte),
            "int16" | "short" => Ok(DataType::Int16),
            "int32" | "int" => Ok(DataType::Int32),
            "int64" | "long" | "integer" => Ok(DataType::Int64),
            "single" | "float" => Ok(DataType::Single),
            "double" => Ok(DataType::Double),
            "decimal" | "currency" => Ok(DataType::Decimal),
            "string" | "text" => Ok(DataType::String),
            "datetime" | "date" | "timestamp" => Ok(DataType::DateTime),
            "binary" | "byte[]" | "blob" => Ok(DataType::Binary),
            "json" | "jsonb" => Ok(DataType::Json),
            other => Err(Error::Model(format!(
                "未知的数据类型 \"{other}\"（支持：Boolean/Byte/Int16/Int32/Int64/Single/Double/Decimal/String/DateTime/Binary/Json）"
            ))),
        }
    }

    /// 是否整数族（含 Byte/Int16/Int32/Int64）。
    pub fn is_integer(&self) -> bool {
        matches!(
            self,
            DataType::Byte | DataType::Int16 | DataType::Int32 | DataType::Int64
        )
    }

    /// 是否数值族。
    pub fn is_numeric(&self) -> bool {
        self.is_integer()
            || matches!(
                self,
                DataType::Single | DataType::Double | DataType::Decimal
            )
    }

    /// 对应的 Rust 基础类型名（用于代码生成与文档，不含 `Option`）。
    pub fn rust_type(&self) -> &'static str {
        match self {
            DataType::Boolean => "bool",
            DataType::Byte => "u8",
            DataType::Int16 => "i16",
            DataType::Int32 => "i32",
            DataType::Int64 => "i64",
            DataType::Single => "f32",
            DataType::Double => "f64",
            DataType::Decimal => "rust_decimal::Decimal",
            DataType::String => "String",
            DataType::DateTime => "chrono::NaiveDateTime",
            DataType::Binary => "Vec<u8>",
            // JSON 原文即字符串，实体字段类型与 `String` 一致
            DataType::Json => "String",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_roundtrip() {
        for t in DataType::ALL {
            assert_eq!(DataType::from_name(t.name()).unwrap(), t);
        }
    }

    #[test]
    fn tolerant_aliases() {
        assert_eq!(DataType::from_name("int64").unwrap(), DataType::Int64);
        assert_eq!(DataType::from_name("Long").unwrap(), DataType::Int64);
        assert_eq!(DataType::from_name("text").unwrap(), DataType::String);
        assert_eq!(DataType::from_name("byte[]").unwrap(), DataType::Binary);
        assert_eq!(DataType::from_name("json").unwrap(), DataType::Json);
        assert_eq!(DataType::from_name("JSONB").unwrap(), DataType::Json);
        assert!(DataType::from_name("money2").is_err());
    }

    #[test]
    fn json_behaves_like_string_for_values() {
        // JSON 与 String 值语义一致（实体字段同为 `String`），便于 C# 侧按 String 消费
        assert_eq!(DataType::Json.rust_type(), DataType::String.rust_type());
        assert_eq!(DataType::Json.name(), "Json");
        assert!(!DataType::Json.is_numeric());
    }

    #[test]
    fn numeric_classification() {
        assert!(DataType::Int32.is_integer());
        assert!(DataType::Decimal.is_numeric());
        assert!(!DataType::String.is_numeric());
    }
}
