//! 实体代码生成：由 `Model.xml` 生成 Rust 实体结构体。
//!
//! 对应 DH.NCode 的 `xcode` 命令（XCodeTool）：C# 侧生成实体类，本模块生成 Rust 结构体，
//! 供迁移期两边共用同一份数据模型文件。

use crate::model::{ColumnMeta, EntityModel, TableMeta};
use crate::show_in::{ShowInOption, TriState};
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
        let mut ty = match col.enum_type.as_deref().and_then(known_enum) {
            Some((path, _)) => path.to_string(),
            None => col.data_type.rust_type().to_string(),
        };
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
        if let Some(enum_type) = col.enum_type.as_deref()
            && known_enum(enum_type).is_some()
        {
            tags.push("枚举");
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
        // 未知枚举类型：按整型生成并注明对应的 C# 枚举
        let enum_note = match col.enum_type.as_deref() {
            Some(enum_type) if known_enum(enum_type).is_none() => {
                format!("（对应 C# 枚举 {enum_type}，此处按整型生成）")
            }
            _ => String::new(),
        };
        out.push_str(&format!(
            "    /// {doc}{tag_text}{enum_note}\n    pub {field}: {ty},\n"
        ));
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
    out.push_str("        }\n    }\n");

    // —— 分表策略（DataScale 含 `timeShard:` 时生成，对应 C# 静态构造的 Meta.ShardPolicy）——
    if let Some((column, table_policy, conn_policy)) = time_shard_spec(table) {
        let field_name = table.effective_column_name(column);
        out.push_str(
            "\n    /// 分表策略（对应 C# 静态构造中的 `Meta.ShardPolicy = new TimeShardPolicy(...)`）。\n    ///\n    /// 由 `DataScale=\"timeShard:...\"` 生成；用法见 `pek_rcode::shards` 模块文档。\n    pub fn shard_policy() -> pek_rcode::shards::TimeShardPolicy {\n",
        );
        out.push_str(&format!(
            "        pek_rcode::shards::TimeShardPolicy::new(\"{field_name}\")\n"
        ));
        // 与 C# `BuildShardPolicy` 一致：把 DataScale 段包装成 `{0}_{1:段}` 模板
        if !table_policy.is_empty() {
            out.push_str(&format!(
                "            .with_table_policy(\"{{0}}_{{1:{table_policy}}}\")\n"
            ));
        }
        if !conn_policy.is_empty() {
            out.push_str(&format!(
                "            .with_conn_policy(\"{{0}}_{{1:{conn_policy}}}\")\n"
            ));
        }
        // 步进推断与 C# `BuildShardPolicy` 一致（HH→小时、dd→天、MM→30 天、yy→365 天）
        let haystack = format!("{table_policy}{conn_policy}");
        let step = if haystack.contains("HH") {
            "            .with_step_hours(1)\n"
        } else if haystack.contains("dd") {
            "            .with_step_days(1)\n"
        } else if haystack.contains("MM") {
            "            .with_step_days(30)\n"
        } else if haystack.contains("yy") {
            "            .with_step_days(365)\n"
        } else {
            "            .with_step_days(1)\n"
        };
        out.push_str(step);
        if column.data_type == DataType::Int64 {
            // 雪花 Id 分表：使用进程级共享雪花实例（对应 C# `Factory.Snow`）
            out.push_str("            .with_snow(pek_rcode::snowflake::shared())\n");
        }
        out.push_str("    }\n");
    }

    out.push_str("}\n\n");
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
        let expr = if col.enum_type.as_deref().and_then(known_enum).is_some() {
            // 成员枚举：以 i32 存取（与 C# 枚举的底层类型一致）
            if col.nullable {
                format!("self.{field}.map(|v| v as i32).into()")
            } else {
                format!("(self.{field} as i32).into()")
            }
        } else if needs_clone(col.data_type) {
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

    // 分表实体：生成按列回写（雪花主键自动生成后回写实体，对齐 C# AutoFillSnowIdPrimaryKey 的对象同步）
    if time_shard_spec(table).is_some() {
        let mut arms = String::new();
        for col in &table.columns {
            let field = safe_field_name(&to_snake_case(&col.name));
            let name = table.effective_column_name(col);
            arms.push_str(&format!(
                "        if column.eq_ignore_ascii_case(\"{name}\") {{\n            self.{field} = {};\n            return Ok(true);\n        }}\n",
                set_field_expr(col)
            ));
        }
        methods.push(format!(
            "fn set_field(&mut self, column: &str, value: DbValue) -> Result<bool> {{\n{arms}        Ok(false)\n    }}"
        ));
    }

    out.push_str(&format!(
        "impl Entity for {name} {{\n    {body}\n}}\n",
        name = table.name,
        body = methods.join("\n\n    ")
    ));

    out
}

/// 已知成员枚举 → Rust 枚举（返回 `(类型路径, 默认成员表达式)`）。
///
/// 对应 C# 的 `Column.Type` 枚举引用；已知枚举来自 [`crate::membership`]（与 DH.NCode 数值一致）。
fn known_enum(enum_type: &str) -> Option<(&'static str, &'static str)> {
    let short = enum_type.rsplit('.').next().unwrap_or(enum_type);
    match short {
        "SexKinds" => Some((
            "pek_rcode::membership::SexKinds",
            "pek_rcode::membership::SexKinds::Unknown",
        )),
        "MenuTypes" => Some((
            "pek_rcode::membership::MenuTypes",
            "pek_rcode::membership::MenuTypes::Directory",
        )),
        "RoleTypes" => Some((
            "pek_rcode::membership::RoleTypes",
            "pek_rcode::membership::RoleTypes::Normal",
        )),
        "TenantTypes" => Some((
            "pek_rcode::membership::TenantTypes",
            "pek_rcode::membership::TenantTypes::Free",
        )),
        "DepartmentTypes" => Some((
            "pek_rcode::membership::DepartmentTypes",
            "pek_rcode::membership::DepartmentTypes::Company",
        )),
        "ParameterKinds" => Some((
            "pek_rcode::membership::ParameterKinds",
            "pek_rcode::membership::ParameterKinds::Normal",
        )),
        "DataScopes" | "DataScope" => Some((
            "pek_rcode::membership::DataScope",
            "pek_rcode::membership::DataScope::Default",
        )),
        _ => None,
    }
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
    if let Some((_, default_member)) = col.enum_type.as_deref().and_then(known_enum) {
        return default_member.into();
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
    // 成员枚举：i32 → 枚举（未知值取默认成员）
    if let Some((path, default_member)) = col.enum_type.as_deref().and_then(known_enum) {
        return if col.nullable {
            format!(
                "row.get_by_name(\"{name}\").and_then(DbValue::as_i32).and_then({path}::from_i32)"
            )
        } else {
            format!(
                "row.get_by_name(\"{name}\").and_then(DbValue::as_i32).and_then({path}::from_i32).unwrap_or({default_member})"
            )
        };
    }
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

/// 分表列解析（`DataScale` = `timeShard:表模板[:连接模板]`，对应 C# `ScaleColumn` / `BuildShardPolicy`）。
fn time_shard_spec(table: &TableMeta) -> Option<(&ColumnMeta, String, String)> {
    let column = table.columns.iter().find(|c| {
        c.data_scale
            .as_deref()
            .is_some_and(|v| v.to_ascii_lowercase().starts_with("timeshard:"))
    })?;
    let spec = column.data_scale.as_deref().unwrap_or("");
    let parts: Vec<&str> = spec.split(':').collect();
    let table_policy = parts.get(1).copied().unwrap_or("").to_string();
    let conn_policy = parts.get(2).copied().unwrap_or("").to_string();
    Some((column, table_policy, conn_policy))
}

/// `set_field` 中每个字段的赋值表达式（`value: DbValue` → 字段类型）。
fn set_field_expr(col: &ColumnMeta) -> String {
    // 成员枚举：i32 → 枚举（未知值取默认成员）
    if let Some((path, default_member)) = col.enum_type.as_deref().and_then(known_enum) {
        return if col.nullable {
            format!("value.as_i32().and_then({path}::from_i32)")
        } else {
            format!("value.as_i32().and_then({path}::from_i32).unwrap_or({default_member})")
        };
    }
    if col.nullable {
        return match col.data_type {
            DataType::String => "(!value.is_null()).then(|| value.to_text())".into(),
            DataType::Binary => "value.as_blob().map(<[u8]>::to_vec)".into(),
            other => format!("value.{}()", value_converter(other).trim_start_matches("DbValue::")),
        };
    }
    match col.data_type {
        DataType::String => "value.to_text()".into(),
        DataType::Binary => "value.as_blob().map(<[u8]>::to_vec).unwrap_or_default()".into(),
        DataType::DateTime => {
            "value.as_datetime().unwrap_or_else(|| chrono::DateTime::UNIX_EPOCH.naive_utc())".into()
        }
        other => format!(
            "value.{}().unwrap_or_default()",
            value_converter(other).trim_start_matches("DbValue::")
        ),
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

/// 列的 Rust 字段类型（可空则包裹 `Option<...>`；与实体生成规则一致）。
fn column_rust_type(col: &ColumnMeta) -> String {
    let mut ty = col.data_type.rust_type().to_string();
    if col.nullable {
        ty = format!("Option<{ty}>");
    }
    ty
}

/// 是否跳过模型类/接口生成（`Model="False"`，对应 DH.NCode 的 `Properties["Model"]`）。
fn model_excluded(col: &ColumnMeta) -> bool {
    col.model.as_deref() == Some("False")
}

/// 首字母小写（参数名用）。
fn lower_first(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

// ================= 搜索条件（对应 DH.NCode 的 `Code/SearchBuilder.cs`） =================

/// 搜索参数（对应 DH.NCode 的 `ParameterModel`）。
#[derive(Debug, Clone, PartialEq)]
pub struct SearchParameter {
    /// 字段名（模型列名）
    pub name: String,
    /// 参数名（小驼峰）
    pub parameter_name: String,
    /// 类型名（`extend` 为 false 时去掉路径前缀）
    pub type_name: String,
    /// 是否可空（布尔恒为可空，与 C# 一致）
    pub nullable: bool,
}

/// 搜索功能构建器（对应 DH.NCode 的 `SearchBuilder`）。
///
/// 提供“可用于搜索的字段列表”（[`Self::columns`]）与“搜索参数列表”（[`Self::parameters`]），
/// 供搜索表单 / 高级查询模板使用；字段筛选规则与 C# 版一致：
/// - 索引列参与（按表字段顺序，跳过主键/自增/Master）
/// - `ShowIn` 的 `Search` 三态可显式 Show/Hide
/// - `DataScale`（数据时间）、整数枚举/映射、布尔（`enable`/`isDeleted` 置尾）自动参与
/// - 选中数据时间字段（雪花 ID 优先）后由 `start`/`end` 参数承担，本身从列表移除
pub struct SearchBuilder<'a> {
    /// 数据表
    pub table: &'a TableMeta,
    /// 可空模式（可选文本列生成 `Option<...>`）
    pub nullable: bool,
}

impl<'a> SearchBuilder<'a> {
    /// 创建（默认启用可空模式）。
    pub fn new(table: &'a TableMeta) -> Self {
        Self {
            table,
            nullable: true,
        }
    }

    /// 获取可用于搜索的字段列表（对齐 C# `GetColumns()`）。
    pub fn columns(&self) -> Vec<&'a ColumnMeta> {
        self.analyze().0
    }

    /// 数据时间字段（雪花 ID 优先，其次时间列；用于 `start`/`end` 参数）。
    pub fn data_time(&self) -> Option<&'a ColumnMeta> {
        self.analyze().1
    }

    /// 获取搜索参数列表（对齐 C# `GetParameters()`；`extend` 控制类型名是否含路径前缀）。
    pub fn parameters(&self, extend: bool) -> Vec<SearchParameter> {
        let mut result = Vec::new();
        for col in self.columns() {
            let mut type_name = col.data_type.rust_type().to_string();
            if !extend {
                type_name = type_name.rsplit("::").next().unwrap_or(&type_name).to_string();
            }
            result.push(SearchParameter {
                name: col.name.clone(),
                parameter_name: lower_first(&col.name),
                type_name,
                nullable: col.nullable || col.data_type == DataType::Boolean,
            });
        }
        // 数据时间字段由 start/end 参数承担（对应 C# 的 includeTime）
        if self.data_time().is_some() {
            for name in ["start", "end"] {
                result.push(SearchParameter {
                    name: name.to_string(),
                    parameter_name: name.to_string(),
                    type_name: "NaiveDateTime".to_string(),
                    nullable: true,
                });
            }
        }
        result
    }

    /// 统一分析：返回（搜索字段、数据时间字段）。
    fn analyze(&self) -> (Vec<&'a ColumnMeta>, Option<&'a ColumnMeta>) {
        let table = self.table;
        let mut columns: Vec<&ColumnMeta> = Vec::new();

        // 1) 索引列参与（按表字段顺序）
        let mut index_columns: Vec<&str> = Vec::new();
        for index in &table.indexes {
            for name in &index.columns {
                if !index_columns.iter().any(|c| c.eq_ignore_ascii_case(name)) {
                    index_columns.push(name);
                }
            }
        }
        if !index_columns.is_empty() {
            for col in &table.columns {
                if col.primary_key || col.identity || col.master {
                    continue;
                }
                let show = ShowInOption::parse(col.show_in.as_deref().unwrap_or_default());
                if show.search_hide() {
                    continue;
                }
                let matched = index_columns.iter().any(|n| n.eq_ignore_ascii_case(&col.name))
                    || col
                        .column_name
                        .as_deref()
                        .is_some_and(|cn| index_columns.iter().any(|n| n.eq_ignore_ascii_case(cn)));
                if matched {
                    columns.push(col);
                }
            }
        }

        // 2) 特殊字段（ShowIn 显式 / 数据时间 / 整数枚举与映射 / 布尔）
        for col in &table.columns {
            if columns.iter().any(|c| c.name.eq_ignore_ascii_case(&col.name)) {
                continue;
            }
            let show = ShowInOption::parse(col.show_in.as_deref().unwrap_or_default());
            match show.search {
                TriState::Show => {
                    columns.push(col);
                    continue;
                }
                TriState::Hide => continue,
                TriState::Auto => {}
            }
            if col.data_scale.as_deref().is_some_and(|v| !v.is_empty()) {
                // 数据时间字段
                columns.push(col);
            } else if col.data_type.is_integer() && (col.enum_type.is_some() || col.map.is_some()) {
                // 整数枚举 / 带 Type 属性 / 有映射
                columns.push(col);
            } else if col.data_type == DataType::Boolean
                && !col.name.eq_ignore_ascii_case("enable")
                && !col.name.eq_ignore_ascii_case("isDeleted")
            {
                columns.push(col);
            }
        }

        // 3) enable / isDeleted 置于最后
        for col in &table.columns {
            if columns.iter().any(|c| c.name.eq_ignore_ascii_case(&col.name)) {
                continue;
            }
            if col.data_type == DataType::Boolean
                && (col.name.eq_ignore_ascii_case("enable") || col.name.eq_ignore_ascii_case("isDeleted"))
            {
                columns.push(col);
            }
        }

        if columns.is_empty() {
            return (columns, None);
        }

        // 4) 数据时间字段：DataScale(time*) > DateTime > UpdateTime/CreateTime；雪花 ID 优先
        let time_column = columns
            .iter()
            .find(|c| {
                c.data_scale
                    .as_deref()
                    .is_some_and(|v| v.to_ascii_lowercase().starts_with("time"))
            })
            .copied()
            .or_else(|| columns.iter().find(|c| c.data_type == DataType::DateTime).copied())
            .or_else(|| {
                table.columns.iter().find(|c| {
                    c.name.eq_ignore_ascii_case("UpdateTime") || c.name.eq_ignore_ascii_case("CreateTime")
                })
            });
        let snow_column = columns
            .iter()
            .find(|c| c.primary_key && !c.identity && c.data_type == DataType::Int64)
            .copied();

        if let Some(time) = time_column {
            columns.retain(|c| !c.name.eq_ignore_ascii_case(&time.name));
        }
        columns.retain(|c| !c.name.eq_ignore_ascii_case("key") && !c.name.eq_ignore_ascii_case("page"));
        if snow_column.is_some() || time_column.is_some() {
            columns.retain(|c| !c.name.eq_ignore_ascii_case("start") && !c.name.eq_ignore_ascii_case("end"));
        }

        (columns, snow_column.or(time_column))
    }
}

// ================= 业务扩展（对应 DH.NCode 的 `{Entity}.Biz.cs` 双文件模式） =================

/// 业务扩展文件名（`OrderItem` → `order_item_biz.rs`）。
///
/// 与实体文件（[`file_name`]）成对：实体每次生成全量覆盖，业务扩展**永不覆盖**、只做分析合并。
pub fn biz_file_name(table: &TableMeta) -> String {
    format!("{}_biz.rs", to_snake_case(&table.name))
}

/// 生成单表的业务扩展骨架（`{snake}_biz.rs`）。
///
/// 对应 DH.NCode 的 `{Entity}.Biz.cs`：生成器只负责创建骨架与补齐缺失区块，
/// **绝不覆盖已有内容**（分析合并见 [`merge_biz`]）。
pub fn generate_biz(table: &TableMeta) -> String {
    let display = if table.description.is_empty() {
        &table.name
    } else {
        &table.description
    };
    let table_name = table.effective_table_name();
    let snake = to_snake_case(&table.name);
    let name = &table.name;

    let mut out = String::with_capacity(1024);
    out.push_str(&format!(
        "//! {display}业务扩展（{table_name}）—— 本文件不会被 rcodegen 覆盖。\n//!\n//! 重新生成只做“分析合并”：已有内容全部保留，缺失的区块（// #region）自动补回。\n//! 自定义方法请写在区块内；生成器不会删除或重写任何手写内容。\n//! 文件头的 use 导入如暂时用不到可自行删除（合并只维护区块）。\n\n"
    ));
    out.push_str(&format!("use super::{snake}::{name};\n\n"));

    // 区块 1：扩展方法
    out.push_str(&format!(
        "// #region 扩展方法\n/// 自定义业务方法（示例代码已注释，可删除本段注释后编写）。\nimpl {name} {{\n    // /// 按业务编码查询（示例，取消注释即可使用；返回首个匹配）。\n    // pub fn find_by_code(\n    //     dal: &pek_rcode::Dal,\n    //     session: &mut dyn pek_rcode::SqlSession,\n    //     code: &str,\n    // ) -> pek_rcode::Result<Option<Self>> {{\n    //     let query = pek_rcode::Query::new().filter(pek_rcode::Where::new().eq(\"Code\", code)).take(1);\n    //     Ok(Self::query(dal, session, &query)?.into_iter().next())\n    // }}\n}}\n// #endregion 扩展方法\n\n"
    ));

    // 区块 2：扩展查询
    out.push_str(
        "// #region 扩展查询\n// 常用条件查询 / 辅助函数（如批量查找、分页封装）。\n// #endregion 扩展查询\n",
    );
    out
}

/// 业务扩展文件的“分析合并”：已有内容全部保留，骨架中缺失的区块自动补回。
///
/// 区块以 `// #region 名称` / `// #endregion` 标记（rust-analyzer 支持折叠）：
/// - 旧文件中已存在的同名区块：**经不修改**（保留手写内容）
/// - 骨架中新增而旧文件缺失的区块：按骨架顺序追加到文件末尾
/// - 支持未来骨架升级（新增区块自动补进旧文件，对应 DH.NCode Biz 的 `Merge`）
/// <param name="existing">旧文件内容</param>
/// <param name="generated">新生成的骨架</param>
/// <returns>合并后的内容；无缺失区块时返回 None（无需写盘）</returns>
pub fn merge_biz(existing: &str, generated: &str) -> Option<String> {
    let existing_regions = parse_regions(existing);
    let generated_regions = parse_regions(generated);
    let missing: Vec<&(String, String)> = generated_regions
        .iter()
        .filter(|(name, _)| !existing_regions.iter().any(|(n, _)| n == name))
        .collect();
    if missing.is_empty() {
        return None;
    }

    let mut out = existing.trim_end().to_string();
    out.push('\n');
    for (_, block) in missing {
        out.push('\n');
        out.push_str(block.trim_end());
        out.push('\n');
    }
    Some(out)
}

/// 解析 `// #region 名称` … `// #endregion` 区块（扁平结构；未闭合的区块丢弃）。
fn parse_regions(text: &str) -> Vec<(String, String)> {
    let mut regions = Vec::new();
    let mut current: Option<(String, Vec<&str>)> = None;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("// #region") {
            // 出现新的开始标记：丢弃上一个未闭合块
            current = Some((rest.trim().to_string(), vec![line]));
        } else if trimmed.starts_with("// #endregion") {
            if let Some((name, mut lines)) = current.take() {
                lines.push(line);
                regions.push((name, lines.join("\n")));
            }
        } else if let Some((_, lines)) = current.as_mut() {
            lines.push(line);
        }
    }
    regions
}

// ================= 模型类（对应 DH.NCode 的 `Code/ModelBuilder.cs`） =================

/// 模型类文件名（`OrderItem` → `order_item_model.rs`）。
pub fn model_file_name(table: &TableMeta) -> String {
    format!("{}_model.rs", to_snake_case(&table.name))
}

/// 接口文件名（`OrderItem` → `order_item_interface.rs`）。
pub fn interface_file_name(table: &TableMeta) -> String {
    format!("{}_interface.rs", to_snake_case(&table.name))
}

/// 生成单表的简易模型类（对应 `ModelBuilder`；跳过 `Model="False"` 的列）。
///
/// 模型类用于数据传输（与实体结构体分离），提供 `new()`/`Default` 与 `from_row()`。
pub fn generate_model(table: &TableMeta) -> String {
    let display = if table.description.is_empty() {
        &table.name
    } else {
        &table.description
    };
    let table_name = table.effective_table_name();
    let columns: Vec<&ColumnMeta> = table.columns.iter().filter(|c| !model_excluded(c)).collect();

    let mut out = String::with_capacity(2048);
    out.push_str(&format!(
        "//! {display}（{table_name}）模型类。\n//!\n//! 由 pek-rcode 从 Model.xml 自动生成，请勿手工修改；修改模型后重新生成。\n\n"
    ));
    out.push_str("use chrono::NaiveDateTime;\nuse rust_decimal::Decimal;\nuse pek_rcode::session::DbRow;\nuse pek_rcode::value::DbValue;\n\n");
    out.push_str(&format!(
        "/// {display} 模型（数据传输用）\n#[derive(Debug, Clone, PartialEq)]\npub struct {}Model {{\n",
        table.name
    ));
    for col in &columns {
        let field = safe_field_name(&to_snake_case(&col.name));
        let ty = column_rust_type(col);
        let doc = if col.description.is_empty() {
            format!("{} 列", col.name)
        } else {
            clean_doc(&col.description)
        };
        out.push_str(&format!("    /// {doc}\n    pub {field}: {ty},\n"));
    }
    out.push_str("}\n\n");

    out.push_str(&format!(
        "impl {name}Model {{\n    /// 按列类型默认值创建。\n    pub fn new() -> Self {{\n        Self {{\n",
        name = table.name
    ));
    for col in &columns {
        let field = safe_field_name(&to_snake_case(&col.name));
        out.push_str(&format!("            {field}: {},\n", default_expr(col)));
    }
    out.push_str("        }\n    }\n\n    /// 从数据行填充。\n    pub fn from_row(row: &DbRow) -> Self {\n        Self {\n");
    for col in &columns {
        let field = safe_field_name(&to_snake_case(&col.name));
        let name = table.effective_column_name(col);
        out.push_str(&format!("            {field}: {},\n", from_row_expr(col, name)));
    }
    out.push_str("        }\n    }\n}\n\n");
    out.push_str(&format!(
        "impl Default for {name}Model {{\n    fn default() -> Self {{\n        Self::new()\n    }}\n}}\n",
        name = table.name
    ));
    out
}

/// 全量生成模型类（文件名 → 代码）。
pub fn generate_all_models(model: &EntityModel) -> Vec<(String, String)> {
    model
        .tables
        .iter()
        .map(|table| (model_file_name(table), generate_model(table)))
        .collect()
}

// ================= 实体接口（对应 DH.NCode 的 `Code/InterfaceBuilder.cs`） =================

/// 生成单表的实体接口 trait（对应 `InterfaceBuilder`；跳过 `Model="False"` 的列）。
///
/// 生成 `pub trait I{Name}`（每列一对 getter/setter）及对实体结构体的实现。
pub fn generate_interface(table: &TableMeta) -> String {
    let display = if table.description.is_empty() {
        &table.name
    } else {
        &table.description
    };
    let table_name = table.effective_table_name();
    let columns: Vec<&ColumnMeta> = table.columns.iter().filter(|c| !model_excluded(c)).collect();

    let mut out = String::with_capacity(2048);
    out.push_str(&format!(
        "//! {display}（{table_name}）实体接口。\n//!\n//! 由 pek-rcode 从 Model.xml 自动生成，请勿手工修改。\n\n"
    ));
    out.push_str("use chrono::NaiveDateTime;\nuse rust_decimal::Decimal;\n\n");
    out.push_str(&format!("/// {display} 实体接口\npub trait I{name} {{\n", name = table.name));
    for col in &columns {
        let field = safe_field_name(&to_snake_case(&col.name));
        let setter = format!("set_{}", to_snake_case(&col.name));
        let ty = column_rust_type(col);
        let doc = if col.description.is_empty() {
            format!("{} 列", col.name)
        } else {
            clean_doc(&col.description)
        };
        out.push_str(&format!(
            "    /// 获取 {doc}\n    fn {field}(&self) -> &{ty};\n    /// 设置 {doc}\n    fn {setter}(&mut self, value: {ty});\n"
        ));
    }
    out.push_str("}\n\n");

    out.push_str(&format!("impl I{name} for {name} {{\n", name = table.name));
    for col in &columns {
        let field = safe_field_name(&to_snake_case(&col.name));
        let setter = format!("set_{}", to_snake_case(&col.name));
        let ty = column_rust_type(col);
        out.push_str(&format!(
            "    fn {field}(&self) -> &{ty} {{\n        &self.{field}\n    }}\n    fn {setter}(&mut self, value: {ty}) {{\n        self.{field} = value;\n    }}\n"
        ));
    }
    out.push_str("}\n");
    out
}

/// 全量生成实体接口（文件名 → 代码）。
pub fn generate_all_interfaces(model: &EntityModel) -> Vec<(String, String)> {
    model
        .tables
        .iter()
        .map(|table| (interface_file_name(table), generate_interface(table)))
        .collect()
}

// ================= 代码生成插件（对应 DH.NCode 的 `Code/ICodePlugin.cs`） =================

/// 代码生成插件：生成前可修正模型数据表（增删改表与列定义）。
pub trait CodePlugin {
    /// 修正数据表（默认空实现）。
    fn fix_tables(&self, tables: &mut Vec<TableMeta>) {
        let _ = tables;
    }
}

/// 依次应用插件修正（对齐 XCodeTool 的 `plugin.FixTables` 流程）。
pub fn apply_plugins(tables: &mut Vec<TableMeta>, plugins: &[&dyn CodePlugin]) {
    for plugin in plugins {
        plugin.fix_tables(tables);
    }
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

    #[test]
    fn generate_shard_policy_for_time_shard_columns() {
        // 雪花 Id 主键按年分表（对应 C# `[BindColumn(..., DataScale = "timeShard:yyyy")]`）
        let model = EntityModel::parse(
            r#"<EntityModel><Tables><Table Name="ExpressLogs" TableName="ExpressLogs">
              <Columns>
                <Column Name="Id" DataType="Int64" PrimaryKey="True" DataScale="timeShard:yyyy" />
                <Column Name="CreateTime" DataType="DateTime" />
              </Columns>
            </Table></Tables></EntityModel>"#,
        )
        .unwrap();
        let code = generate(&model.tables[0]);
        assert!(
            code.contains("pub fn shard_policy() -> pek_rcode::shards::TimeShardPolicy {"),
            "{code}"
        );
        assert!(code.contains("TimeShardPolicy::new(\"Id\")"), "{code}");
        assert!(code.contains(".with_table_policy(\"{0}_{1:yyyy}\")"), "{code}");
        assert!(code.contains(".with_step_days(365)"), "{code}");
        assert!(
            code.contains(".with_snow(pek_rcode::snowflake::shared())"),
            "雪花主键分表应携带进程级雪花实例：{code}"
        );
        assert!(
            code.contains("fn set_field(&mut self, column: &str, value: DbValue) -> Result<bool> {"),
            "{code}"
        );
        assert!(code.contains("if column.eq_ignore_ascii_case(\"Id\")"), "{code}");

        // 时间列分表 + 连接模板（`timeShard:表模板:连接模板`，对应 C# BuildShardPolicy 的三段解析）
        let model = EntityModel::parse(
            r#"<EntityModel><Tables><Table Name="WmsLog" TableName="WmsLog">
              <Columns>
                <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
                <Column Name="CreateTime" DataType="DateTime" DataScale="timeShard:yyyyMMdd:yyyy" />
              </Columns>
            </Table></Tables></EntityModel>"#,
        )
        .unwrap();
        let code = generate(&model.tables[0]);
        assert!(code.contains("TimeShardPolicy::new(\"CreateTime\")"), "{code}");
        assert!(code.contains(".with_table_policy(\"{0}_{1:yyyyMMdd}\")"), "{code}");
        assert!(code.contains(".with_conn_policy(\"{0}_{1:yyyy}\")"), "{code}");
        assert!(code.contains(".with_step_days(1)"), "{code}");
        assert!(
            !code.contains("with_snow"),
            "时间字段分表不应携带雪花实例：{code}"
        );

        // 无分表列的表不生成 shard_policy
        let table = EntityModel::parse(MODEL).unwrap();
        let code = generate(&table.tables[0]);
        assert!(!code.contains("shard_policy"), "{code}");
    }

    #[test]
    fn search_builder_columns_and_parameters() {
        const SEARCH_MODEL: &str = r#"<EntityModel><Tables><Table Name="SearchOrder" TableName="DH_SearchOrder">
          <Columns>
            <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
            <Column Name="Code" DataType="String" Length="50" />
            <Column Name="Status" DataType="Int32" Type="Dto.Status" />
            <Column Name="Ok" DataType="Boolean" />
            <Column Name="Visible" DataType="String" ShowIn="Search" />
            <Column Name="Secret" DataType="String" ShowIn="-Search" />
            <Column Name="Enable" DataType="Boolean" />
            <Column Name="CreateTime" DataType="DateTime" />
          </Columns>
          <Indexes><Index Columns="Code" Unique="True" /></Indexes>
        </Table></Tables></EntityModel>"#;
        let model = EntityModel::parse(SEARCH_MODEL).unwrap();
        let table = &model.tables[0];
        let builder = SearchBuilder::new(table);

        let names: Vec<&str> = builder.columns().iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["Code", "Status", "Ok", "Visible", "Enable"],
            "搜索字段筛选与顺序：{names:?}"
        );
        assert_eq!(
            builder.data_time().map(|c| c.name.as_str()),
            Some("CreateTime"),
            "数据时间字段应为 CreateTime"
        );

        let params = builder.parameters(false);
        let param_names: Vec<&str> = params.iter().map(|p| p.parameter_name.as_str()).collect();
        assert_eq!(param_names, vec!["code", "status", "ok", "visible", "enable", "start", "end"]);
        let status = params.iter().find(|p| p.name == "Status").unwrap();
        assert_eq!(status.type_name, "i32");
        let ok = params.iter().find(|p| p.name == "Ok").unwrap();
        assert!(ok.nullable, "布尔参数恒为可空");
        assert_eq!(params.last().unwrap().type_name, "NaiveDateTime");
    }

    #[test]
    fn generate_model_and_interface() {
        const MODEL_WITH_FLAGS: &str = r#"<EntityModel><Tables><Table Name="OrderItem" TableName="DH_OrderItem">
          <Columns>
            <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
            <Column Name="Code" DataType="String" Length="50" />
            <Column Name="Remark" DataType="String" Nullable="True" />
            <Column Name="CreateUser" DataType="String" Model="False" />
            <Column Name="CreateTime" DataType="DateTime" />
          </Columns>
        </Table></Tables></EntityModel>"#;
        let model = EntityModel::parse(MODEL_WITH_FLAGS).unwrap();
        let table = &model.tables[0];

        // 模型类
        let code = generate_model(table);
        assert!(code.contains("pub struct OrderItemModel {"), "{code}");
        assert!(code.contains("pub code: String,"), "{code}");
        assert!(code.contains("pub remark: Option<String>,"), "{code}");
        assert!(code.contains("pub fn from_row(row: &DbRow) -> Self {"), "{code}");
        assert!(!code.contains("create_user"), "Model=False 的列不应进入模型类：{code}");

        // 接口
        let code = generate_interface(table);
        assert!(code.contains("pub trait IOrderItem {"), "{code}");
        assert!(code.contains("fn code(&self) -> &String;"), "{code}");
        assert!(code.contains("fn set_code(&mut self, value: String);"), "{code}");
        assert!(code.contains("impl IOrderItem for OrderItem {"), "{code}");
        assert!(!code.contains("create_user"), "Model=False 的列不应进入接口：{code}");

        // 全量生成文件名
        assert_eq!(generate_all_models(&model)[0].0, "order_item_model.rs");
        assert_eq!(
            generate_all_interfaces(&model)[0].0,
            "order_item_interface.rs"
        );
    }

    #[test]
    fn code_plugin_fixes_tables() {
        struct AddTablePlugin;
        impl CodePlugin for AddTablePlugin {
            fn fix_tables(&self, tables: &mut Vec<TableMeta>) {
                let mut table = tables[0].clone();
                table.name = "Extra".into();
                table.table_name = "DH_Extra".into();
                tables.push(table);
            }
        }

        let mut model = EntityModel::parse(MODEL).unwrap();
        let plugins: Vec<&dyn CodePlugin> = vec![&AddTablePlugin];
        apply_plugins(&mut model.tables, &plugins);
        assert_eq!(model.tables.len(), 2);
        assert_eq!(model.tables[1].name, "Extra");
    }

    #[test]
    fn enum_columns_generate_membership_types() {
        const ENUM_MODEL: &str = r#"<EntityModel><Tables><Table Name="User" TableName="DH_User">
          <Columns>
            <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" />
            <Column Name="Sex" DataType="Int32" Type="SexKinds" />
            <Column Name="Menu" DataType="Int32" Type="XCode.Membership.MenuTypes" Nullable="True" />
            <Column Name="Custom" DataType="Int32" Type="My.CustomKind" />
          </Columns>
        </Table></Tables></EntityModel>"#;

        let model = EntityModel::parse(ENUM_MODEL).unwrap();
        let code = generate(&model.tables[0]);

        // 已知成员枚举 → Rust 枚举类型（含 Option 与 i32 存取）
        assert!(
            code.contains("pub sex: pek_rcode::membership::SexKinds"),
            "{code}"
        );
        assert!(
            code.contains("pub menu: Option<pek_rcode::membership::MenuTypes>"),
            "{code}"
        );
        assert!(
            code.contains("self.sex as i32).into()"),
            "{code}"
        );
        assert!(
            code.contains("self.menu.map(|v| v as i32).into()"),
            "{code}"
        );
        assert!(
            code.contains("pek_rcode::membership::SexKinds::from_i32"),
            "{code}"
        );
        assert!(
            code.contains("unwrap_or(pek_rcode::membership::SexKinds::Unknown)"),
            "{code}"
        );
        // 未知枚举 → 按整型生成并注明 C# 枚举
        assert!(code.contains("pub custom: i32"), "{code}");
        assert!(code.contains("对应 C# 枚举 My.CustomKind"), "{code}");
    }

    #[test]
    fn biz_skeleton_and_merge_keep_handwritten_code() {
        let model = EntityModel::parse(MODEL).unwrap();
        let table = &model.tables[0];

        // 骨架：文件头 + 双区块 + 实体导入
        let skeleton = generate_biz(table);
        assert_eq!(biz_file_name(table), "order_item_biz.rs");
        assert!(
            skeleton.contains("use super::order_item::OrderItem;"),
            "{skeleton}"
        );
        assert!(skeleton.contains("// #region 扩展方法"), "{skeleton}");
        assert!(skeleton.contains("// #region 扩展查询"), "{skeleton}");
        assert!(skeleton.contains("impl OrderItem {"), "{skeleton}");
        assert!(skeleton.contains("// #endregion 扩展方法"), "{skeleton}");

        // 二次生成：无缺失区块 → 无需写盘（幂等）
        assert_eq!(merge_biz(&skeleton, &skeleton), None);

        // 旧文件缺“扩展查询”区块：手写内容保留，缺失区块自动补回
        let old = "//! 头注释\n\nuse super::order_item::OrderItem;\n\n// #region 扩展方法\nimpl OrderItem {\n    pub fn custom(&self) -> i32 { 42 }\n}\n// #endregion 扩展方法\n";
        let merged = merge_biz(old, &skeleton).expect("应补回缺失区块");
        assert!(
            merged.contains("pub fn custom(&self) -> i32 { 42 }"),
            "手写内容不能丢：{merged}"
        );
        assert_eq!(
            merged.matches("// #region 扩展方法").count(),
            1,
            "已有区块不重复：{merged}"
        );
        assert!(
            merged.contains("// #region 扩展查询"),
            "缺失区块应补回：{merged}"
        );

        // 合并结果再次合并：幂等无变化
        assert_eq!(merge_biz(&merged, &skeleton), None);
    }
}
