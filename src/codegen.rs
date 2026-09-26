//! 实体代码生成：由 `Model.xml` 生成 Rust 实体结构体。
//!
//! 对应 DH.NCode 的 `xcode` 命令（XCodeTool）：C# 侧生成实体类，本模块生成 Rust 结构体，
//! 供迁移期两边共用同一份数据模型文件。

use crate::model::{ColumnMeta, EntityModel, TableMeta};
use crate::types::DataType;

/// Rust 关键字（作为字段名时需要加 `r#` 前缀）。
const KEYWORDS: [&str; 39] = [
    "as", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern", "false", "fn",
    "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return",
    "self", "static", "struct", "super", "trait", "true", "type", "unsafe", "use", "where", "while",
    "async", "await", "box", "yield",
];

/// 表名 → 生成文件名（`OrderItem` → `order_item.rs`）。
pub fn file_name(table: &TableMeta) -> String {
    format!("{}.rs", to_snake_case(&table.name))
}

/// 为整份模型生成实体文件清单：`(文件名, 代码)`。
pub fn generate_all(model: &EntityModel) -> Vec<(String, String)> {
    model
        .tables
        .iter()
        .map(|t| (file_name(t), generate(t)))
        .collect()
}

/// 生成单个表的实体结构体代码。
pub fn generate(table: &TableMeta) -> String {
    let display = if table.description.is_empty() {
        &table.name
    } else {
        &table.description
    };
    let table_name = table.effective_table_name();

    let mut out = String::with_capacity(2048);
    out.push_str(&format!(
        "//! {display}（{table_name}）对象实体。\n//!\n//! 由 pek-rcode 从 Model.xml 自动生成，请勿手工修改；修改模型后重新生成。\n//! 依赖：pek-rcode、chrono、rust_decimal。\n//!\n//! 对象化用法（与 C# 侧 Entity 一致）：\n//! ```ignore\n//! let mut e = {name}::new();\n//! e.insert(&dal, session.as_mut())?;   // 插入并回写自增主键\n//! e.save(&dal, session.as_mut())?;     // 主键为空则新增，否则更新\n//! let one = {name}::find(&dal, session.as_mut(), &[e.id.into()])?;\n//! ```\n\n",
        name = table.name
    ));
    out.push_str("use pek_rcode::{DbRow, DbValue, Entity, Result};\n\n");
    out.push_str(&format!(
        "/// {display}\n///\n/// 表名：`{table_name}`\n#[derive(Debug, Clone, PartialEq)]\npub struct {} {{\n",
        table.name
    ));

    for col in &table.columns {
        let field = safe_field_name(&to_snake_case(&col.name));
        let mut ty = col.data_type.rust_type().to_string();
        if col.nullable {
            ty = format!("Option<{ty}>");
        }

        let mut tags = Vec::new();
        if col.primary_key {
            tags.push("主键");
        }
        if col.identity {
            tags.push("自增");
        }
        let tag_text = if tags.is_empty() {
            String::new()
        } else {
            format!("（{}）", tags.join("、"))
        };

        let doc = if col.description.is_empty() {
            format!("{} 列", col.name)
        } else {
            clean_doc(&col.description)
        };
        out.push_str(&format!("    /// {doc}{tag_text}\n    pub {field}: {ty},\n"));
    }

    out.push_str("}\n\n");

    // —— 常量、构造函数与默认值 ——
    out.push_str(&format!(
        "impl {name} {{\n    /// 数据库表名。\n    pub const TABLE_NAME: &'static str = \"{table_name}\";\n\n    /// 按列类型默认值创建新实体（等价于 C# 的 `new {name}()`；自增主键为 0 表示未入库）。\n    pub fn new() -> Self {{\n        Self {{\n",
        name = table.name,
        table_name = table_name
    ));
    for col in &table.columns {
        let field = safe_field_name(&to_snake_case(&col.name));
        out.push_str(&format!("            {field}: {},\n", default_expr(col)));
    }
    out.push_str("        }\n    }\n}\n\n");
    out.push_str(&format!(
        "impl Default for {name} {{\n    fn default() -> Self {{\n        Self::new()\n    }}\n}}\n\n",
        name = table.name
    ));

    // —— 对象实体实现（增删改查由 Entity 默认方法提供）——
    let mut methods = Vec::new();

    methods.push("fn table() -> &'static str {\n        Self::TABLE_NAME\n    }".to_string());

    let columns: Vec<String> = table
        .columns
        .iter()
        .map(|c| format!("\"{}\"", table.effective_column_name(c)))
        .collect();
    methods.push(format!(
        "fn columns() -> &'static [&'static str] {{\n        &[{}]\n    }}",
        columns.join(", ")
    ));

    let pks: Vec<String> = table
        .columns
        .iter()
        .filter(|c| c.primary_key)
        .map(|c| format!("\"{}\"", table.effective_column_name(c)))
        .collect();
    methods.push(format!(
        "fn primary_keys() -> &'static [&'static str] {{\n        &[{}]\n    }}",
        pks.join(", ")
    ));

    if let Some(identity) = table.identity() {
        methods.push(format!(
            "fn identity_column() -> Option<&'static str> {{\n        Some(\"{}\")\n    }}",
            table.effective_column_name(identity)
        ));
    }

    let mut field_lines = String::new();
    for col in &table.columns {
        let field = safe_field_name(&to_snake_case(&col.name));
        let name = table.effective_column_name(col);
        let expr = if needs_clone(col.data_type) {
            format!("self.{field}.clone().into()")
        } else {
            format!("self.{field}.into()")
        };
        field_lines.push_str(&format!("            (\"{name}\", {expr}),\n"));
    }
    methods.push(format!(
        "fn to_fields(&self) -> Vec<(&'static str, DbValue)> {{\n        vec![\n{field_lines}        ]\n    }}"
    ));

    let mut row_lines = String::new();
    for col in &table.columns {
        let field = safe_field_name(&to_snake_case(&col.name));
        let name = table.effective_column_name(col);
        row_lines.push_str(&format!("            {field}: {},\n", from_row_expr(col, name)));
    }
    methods.push(format!(
        "fn from_row(row: &DbRow) -> Result<Self> {{\n        Ok(Self {{\n{row_lines}        }})\n    }}"
    ));

    if let Some(identity) = table.identity()
        && identity.data_type.is_integer()
    {
        let field = safe_field_name(&to_snake_case(&identity.name));
        let cast = match identity.data_type {
            DataType::Int64 => "value",
            DataType::Int32 => "value as i32",
            DataType::Int16 => "value as i16",
            DataType::Byte => "value as u8",
            _ => "value as i32",
        };
        methods.push(format!(
            "fn set_identity(&mut self, value: i64) -> Result<()> {{\n        self.{field} = {cast};\n        Ok(())\n    }}"
        ));
    }

    out.push_str(&format!(
        "impl Entity for {name} {{\n    {body}\n}}\n",
        name = table.name,
        body = methods.join("\n\n    ")
    ));

    out
}

/// 字段是否为字符串/二进制（非 Copy，输出时需 clone）。
fn needs_clone(t: DataType) -> bool {
    matches!(t, DataType::String | DataType::Binary)
}

/// `new()` 中每个字段的默认值表达式。
fn default_expr(col: &ColumnMeta) -> String {
    if col.nullable {
        return "None".into();
    }
    match col.data_type {
        DataType::Boolean => "false".into(),
        DataType::Byte | DataType::Int16 | DataType::Int32 | DataType::Int64 => "0".into(),
        DataType::Single | DataType::Double => "0.0".into(),
        DataType::Decimal => "Default::default()".into(),
        DataType::String => "String::new()".into(),
        DataType::DateTime => "chrono::DateTime::UNIX_EPOCH.naive_utc()".into(),
        DataType::Binary => "Vec::new()".into(),
    }
}

/// `from_row` 中每个字段的取值表达式（NULL/转换失败时使用列类型默认值）。
fn from_row_expr(col: &ColumnMeta, name: &str) -> String {
    if col.nullable {
        return match col.data_type {
            DataType::String => {
                format!("row.get_by_name(\"{name}\").and_then(|v| (!v.is_null()).then(|| v.to_text()))")
            }
            DataType::Binary => {
                format!("row.get_by_name(\"{name}\").and_then(|v| v.as_blob().map(<[u8]>::to_vec))")
            }
            other => format!(
                "row.get_by_name(\"{name}\").and_then({})",
                value_converter(other)
            ),
        };
    }

    match col.data_type {
        DataType::String => {
            format!("row.get_by_name(\"{name}\").map(DbValue::to_text).unwrap_or_default()")
        }
        DataType::Binary => format!(
            "row.get_by_name(\"{name}\").and_then(|v| v.as_blob().map(<[u8]>::to_vec)).unwrap_or_default()"
        ),
        DataType::DateTime => format!(
            "row.get_by_name(\"{name}\").and_then(DbValue::as_datetime).unwrap_or_else(|| chrono::DateTime::UNIX_EPOCH.naive_utc())"
        ),
        other => format!(
            "row.get_by_name(\"{name}\").and_then({}).unwrap_or_default()",
            value_converter(other)
        ),
    }
}

/// 数据类型 → `DbValue` 转换方法路径。
fn value_converter(t: DataType) -> &'static str {
    match t {
        DataType::Boolean => "DbValue::as_bool",
        DataType::Byte => "DbValue::as_u8",
        DataType::Int16 => "DbValue::as_i16",
        DataType::Int32 => "DbValue::as_i32",
        DataType::Int64 => "DbValue::as_i64",
        DataType::Single => "DbValue::as_f32",
        DataType::Double => "DbValue::as_f64",
        DataType::Decimal => "DbValue::as_decimal",
        DataType::DateTime => "DbValue::as_datetime",
        DataType::String | DataType::Binary => unreachable!("字符串/二进制由调用方特殊处理"),
    }
}

/// PascalCase → snake_case（兼容缩写字段，如 `CreateUserID` → `create_user_id`）。
pub fn to_snake_case(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::with_capacity(name.len() + 4);

    for (i, &c) in chars.iter().enumerate() {
        if c.is_ascii_uppercase() {
            let prev = i.checked_sub(1).map(|j| chars[j]);
            let next = chars.get(i + 1).copied();
            let need_sep = i > 0
                && match prev {
                    Some(p) if p.is_ascii_lowercase() || p.is_ascii_digit() => true,
                    // 连续大写中，仅当后面是小写时切分（如 SId → s_id，IP → ip）
                    Some(p) if p.is_ascii_uppercase() => next.is_some_and(|n| n.is_ascii_lowercase()),
                    _ => false,
                };
            if need_sep {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }

    // 处理非法开头的字段名（数字开头）
    if out.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out
}

/// 字段名冲突处理：关键字加 `r#` 前缀。
fn safe_field_name(name: &str) -> String {
    if KEYWORDS.contains(&name) {
        format!("r#{name}")
    } else {
        name.to_string()
    }
}

/// 清理文档注释内容（去换行，避免破坏 doc 结构）。
fn clean_doc(text: &str) -> String {
    text.replace(['\r', '\n'], " ").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODEL: &str = r#"<EntityModel><Tables><Table Name="OrderItem" TableName="DH_OrderItem" Description="订单明细">
      <Columns>
        <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" Description="编号" />
        <Column Name="OrderID" DataType="Int32" Description="订单编号" />
        <Column Name="SId" DataType="Int64" Nullable="True" />
        <Column Name="Amount" DataType="Decimal" Precision="18" Scale="4" />
        <Column Name="CreateTime" DataType="DateTime" />
        <Column Name="Type" DataType="String" Length="20" />
        <Column Name="Remark" DataType="String" Nullable="True" Description="备注" />
      </Columns>
    </Table></Tables></EntityModel>"#;

    #[test]
    fn snake_case_conversion() {
        assert_eq!(to_snake_case("CreateUserID"), "create_user_id");
        assert_eq!(to_snake_case("SId"), "s_id");
        assert_eq!(to_snake_case("Md5"), "md5");
        assert_eq!(to_snake_case("FileUrl"), "file_url");
        assert_eq!(to_snake_case("Ex1"), "ex1");
        assert_eq!(to_snake_case("IP"), "ip");
        assert_eq!(to_snake_case("HType"), "h_type");
        assert_eq!(to_snake_case("Mac"), "mac");
    }

    #[test]
    fn generate_entity_code() {
        let model = EntityModel::parse(MODEL).unwrap();
        let table = &model.tables[0];
        assert_eq!(file_name(table), "order_item.rs");

        let code = generate(table);

        // 结构体与字段
        assert!(code.contains("pub struct OrderItem {"), "{code}");
        assert!(code.contains("pub id: i32"), "{code}");
        assert!(code.contains("pub order_id: i32"), "{code}");
        assert!(code.contains("pub s_id: Option<i64>"), "可空字段应加 Option：{code}");
        assert!(code.contains("pub amount: rust_decimal::Decimal"), "{code}");
        assert!(code.contains("pub create_time: chrono::NaiveDateTime"), "{code}");
        assert!(code.contains("pub r#type: String"), "关键字字段名应转义：{code}");
        assert!(code.contains("pub const TABLE_NAME: &'static str = \"DH_OrderItem\";"), "{code}");
        assert!(code.contains("/// 编号（主键、自增）"), "{code}");

        // 对象实体实现
        assert!(code.contains("use pek_rcode::{DbRow, DbValue, Entity, Result};"), "{code}");
        assert!(code.contains("impl Entity for OrderItem {"), "{code}");
        assert!(code.contains("fn primary_keys() -> &'static [&'static str] {\n        &[\"Id\"]\n    }"), "{code}");
        assert!(code.contains("fn identity_column() -> Option<&'static str> {\n        Some(\"Id\")\n    }"), "{code}");
        assert!(code.contains("(\"Id\", self.id.into())"), "{code}");
        assert!(code.contains("(\"Type\", self.r#type.clone().into())"), "String 字段应 clone：{code}");
        assert!(code.contains("(\"SId\", self.s_id.into())"), "Option<i64> 为 Copy，无需 clone：{code}");
        assert!(
            code.contains("id: row.get_by_name(\"Id\").and_then(DbValue::as_i32).unwrap_or_default()"),
            "{code}"
        );
        assert!(
            code.contains("remark: row.get_by_name(\"Remark\").and_then(|v| (!v.is_null()).then(|| v.to_text()))"),
            "可空字符串取值：{code}"
        );
        assert!(
            code.contains("fn set_identity(&mut self, value: i64) -> Result<()> {\n        self.id = value as i32;"),
            "{code}"
        );

        // 构造与默认值
        assert!(code.contains("pub fn new() -> Self {"), "{code}");
        assert!(code.contains("impl Default for OrderItem {"), "{code}");
        assert!(code.contains("create_time: chrono::DateTime::UNIX_EPOCH.naive_utc(),"), "{code}");
    }

    #[test]
    fn generate_all_covers_model() {
        let model = EntityModel::parse(MODEL).unwrap();
        let files = generate_all(&model);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0, "order_item.rs");
    }
}
