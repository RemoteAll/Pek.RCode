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
}

impl DataType {
    /// 全部类型（用于遍历/自检）。
    pub const ALL: [DataType; 11] = [
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
            other => Err(Error::Model(format!(
                "未知的数据类型 \"{other}\"（支持：Boolean/Byte/Int16/Int32/Int64/Single/Double/Decimal/String/DateTime/Binary）"
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
        self.is_integer() || matches!(self, DataType::Single | DataType::Double | DataType::Decimal)
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
        assert!(DataType::from_name("money2").is_err());
    }

    #[test]
    fn numeric_classification() {
        assert!(DataType::Int32.is_integer());
        assert!(DataType::Decimal.is_numeric());
        assert!(!DataType::String.is_numeric());
    }
}
