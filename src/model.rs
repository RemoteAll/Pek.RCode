//! 数据模型：`Model.xml`（XCode 数据建模文件）的解析、内存表示与写出。
//!
//! 文件结构与 DH.NCode/XCode 完全一致，可直接沿用 C# 项目中已有的 `Model.xml`：
//!
//! ```xml
//! <EntityModel xmlns="https://newlifex.com/Model202509.xsd">
//!   <Option>
//!     <Namespace>Demo.Entity</Namespace>
//!     <ConnName>DH</ConnName>
//!   </Option>
//!   <Tables>
//!     <Table Name="JiLiYu" TableName="DH_JiLiYu" Description="激励语">
//!       <Columns>
//!         <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" Description="编号" />
//!       </Columns>
//!       <Indexes>
//!         <Index Columns="Code" Unique="True" />
//!       </Indexes>
//!     </Table>
//!   </Tables>
//! </EntityModel>
//! ```
//!
//! 说明：
//! - 解析时按 XML 局部名匹配（忽略命名空间版本差异，兼容 Model202509 等各期 XSD）
//! - 属性语义与 XCode 对齐：`Nullable` 缺省为 `false`（即 NOT NULL）、`TableName` 缺省等于 `Name`
//! - 未知属性/未知元素一律忽略，保证向后兼容

use std::collections::BTreeMap;
use std::path::Path;

use roxmltree::{Document, Node};

use crate::error::{Error, Result};
use crate::types::DataType;

/// 默认的模型命名空间（与当前 XCode 版本一致）。
pub const MODEL_NAMESPACE: &str = "https://newlifex.com/Model202509.xsd";

/// 实体模型（一个 Model.xml 的完整内容）。
#[derive(Debug, Clone, PartialEq)]
pub struct EntityModel {
    /// 文件版本号（原样保留，写出时回写）
    pub version: Option<String>,
    /// 模型版本号（原样保留）
    pub model_version: Option<String>,
    /// 文档地址（原样保留）
    pub document: Option<String>,
    /// 全局配置
    pub options: ModelOptions,
    /// 数据表
    pub tables: Vec<TableMeta>,
}

/// `<Option>` 全局配置。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelOptions {
    /// 原始键值对（保留全部配置项，键为元素局部名）
    pub raw: BTreeMap<String, String>,
}

impl ModelOptions {
    /// 读取配置项（不存在或为空时返回 None）。
    pub fn get(&self, name: &str) -> Option<&str> {
        match self.raw.get(name) {
            Some(v) if !v.is_empty() => Some(v),
            _ => None,
        }
    }

    /// 命名空间（`Namespace`）。
    pub fn namespace(&self) -> Option<&str> {
        self.get("Namespace")
    }

    /// 数据库连接名（`ConnName`）。
    pub fn conn_name(&self) -> Option<&str> {
        self.get("ConnName")
    }

    /// 输出目录（`Output`）。
    pub fn output(&self) -> Option<&str> {
        self.get("Output")
    }

    /// 实体基类（`BaseClass`）。
    pub fn base_class(&self) -> Option<&str> {
        self.get("BaseClass")
    }
}

/// 数据表定义（`<Table>`）。
#[derive(Debug, Clone, PartialEq)]
pub struct TableMeta {
    /// 实体名（`Name`，如 `JiLiYu`）
    pub name: String,
    /// 数据库表名（`TableName`，如 `DH_JiLiYu`）
    pub table_name: String,
    /// 说明（`Description`）
    pub description: String,
    /// 表级连接名覆盖（少见）
    pub conn_name: Option<String>,
    /// 列定义
    pub columns: Vec<ColumnMeta>,
    /// 索引定义
    pub indexes: Vec<IndexMeta>,
}

impl TableMeta {
    /// 数据库实际表名（`TableName` 为空时退回 `Name`）。
    pub fn effective_table_name(&self) -> &str {
        if self.table_name.is_empty() {
            &self.name
        } else {
            &self.table_name
        }
    }

    /// 按列名查找（忽略大小写；`Name` 或 `ColumnName` 均匹配）。
    pub fn column(&self, name: &str) -> Option<&ColumnMeta> {
        self.columns.iter().find(|c| {
            c.name.eq_ignore_ascii_case(name)
                || c.column_name
                    .as_deref()
                    .is_some_and(|cn| cn.eq_ignore_ascii_case(name))
        })
    }

    /// 主键列（按声明顺序）。
    pub fn primary_keys(&self) -> Vec<&ColumnMeta> {
        self.columns.iter().filter(|c| c.primary_key).collect()
    }

    /// 自增列（若有）。
    pub fn identity(&self) -> Option<&ColumnMeta> {
        self.columns.iter().find(|c| c.identity)
    }

    /// 数据库实际列名（`ColumnName` 为空时退回 `Name`）。
    pub fn effective_column_name<'a>(&self, column: &'a ColumnMeta) -> &'a str {
        match column.column_name.as_deref() {
            Some(v) if !v.is_empty() => v,
            _ => &column.name,
        }
    }
}

/// 数据列定义（`<Column>`）。
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnMeta {
    /// 属性名（`Name`）
    pub name: String,
    /// 数据库列名（`ColumnName`，缺省与 Name 相同）
    pub column_name: Option<String>,
    /// 数据类型（`DataType`）
    pub data_type: DataType,
    /// 原始类型（`RawType`，反向工程时用于保持目标库字段类型）
    pub raw_type: Option<String>,
    /// 长度（`Length`，0 表示不限）
    pub length: i32,
    /// 精度（`Precision`）
    pub precision: i32,
    /// 小数位（`Scale`）
    pub scale: i32,
    /// 是否自增（`Identity`）
    pub identity: bool,
    /// 是否主键（`PrimaryKey`）
    pub primary_key: bool,
    /// 是否主字段（`Master`，代表数据行意义的业务字段）
    pub master: bool,
    /// 是否允许空（`Nullable`，缺省 false 即 NOT NULL）
    pub nullable: bool,
    /// 默认值（`DefaultValue`）
    pub default_value: Option<String>,
    /// 说明（`Description`）
    pub description: String,
    /// 枚举类型（`Type`，如 `Dto.HardwareType`，代码生成时引用）
    pub enum_type: Option<String>,
    /// 数据规模（`DataScale`，大数据分表相关）
    pub data_scale: Option<String>,
    /// 关联映射（`Map`，格式 Role.Id.Name）
    pub map: Option<String>,
    /// 显示选项（`ShowIn`）
    pub show_in: Option<String>,
    /// 模型类开关（`Model`，`"False"` 表示排除；对应 DH.NCode 的 `Properties["Model"]`）
    pub model: Option<String>,
}

/// 索引定义（`<Index>`）。
#[derive(Debug, Clone, PartialEq)]
pub struct IndexMeta {
    /// 索引名（缺省由生成器编制）
    pub name: Option<String>,
    /// 组成列（按顺序）
    pub columns: Vec<String>,
    /// 是否唯一索引
    pub unique: bool,
}

impl EntityModel {
    /// 从文件路径加载。
    pub fn load(path: &Path) -> Result<Self> {
        let text = dhrust::io::read_all_text(path)?;
        Self::parse(&text)
    }

    /// 从 XML 文本解析。
    pub fn parse(text: &str) -> Result<Self> {
        let doc = Document::parse(text).map_err(|e| Error::Xml(e.to_string()))?;
        let root = doc.root_element();
        if !root.tag_name().name().eq_ignore_ascii_case("EntityModel") {
            return Err(Error::Xml(format!(
                "根元素应为 EntityModel，实际为 {}",
                root.tag_name().name()
            )));
        }

        let mut model = EntityModel {
            version: attr_string(&root, "Version"),
            model_version: attr_string(&root, "ModelVersion"),
            document: attr_string(&root, "Document"),
            options: ModelOptions::default(),
            tables: Vec::new(),
        };

        for child in root.children().filter(Node::is_element) {
            match child.tag_name().name() {
                "Option" => parse_options(&child, &mut model.options),
                "Tables" => {
                    for table_node in child.children().filter(Node::is_element) {
                        if table_node.tag_name().name() == "Table" {
                            model.tables.push(parse_table(&table_node)?);
                        }
                    }
                }
                _ => {}
            }
        }

        Ok(model)
    }

    /// 按实体名查找表（忽略大小写）。
    pub fn table(&self, name: &str) -> Option<&TableMeta> {
        self.tables
            .iter()
            .find(|t| t.name.eq_ignore_ascii_case(name) || t.effective_table_name().eq_ignore_ascii_case(name))
    }

    /// 仅包含指定表的新模型（用于切片生成/局部迁移）。
    pub fn subset(&self, tables: &[&str]) -> EntityModel {
        let mut cloned = self.clone();
        cloned.tables.retain(|t| {
            tables
                .iter()
                .any(|n| t.name.eq_ignore_ascii_case(n) || t.effective_table_name().eq_ignore_ascii_case(n))
        });
        cloned
    }

    /// 序列化为 Model.xml 文本（与 C# 版可互读）。
    pub fn to_xml(&self) -> String {
        let mut out = String::with_capacity(4096);
        out.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
        out.push_str(&format!("<EntityModel xmlns=\"{MODEL_NAMESPACE}\""));
        write_attr(&mut out, "Version", self.version.as_deref());
        write_attr(&mut out, "ModelVersion", self.model_version.as_deref());
        write_attr(&mut out, "Document", self.document.as_deref());
        out.push_str(">\n");

        // Option
        out.push_str("  <Option>\n");
        for (key, value) in &self.options.raw {
            out.push_str(&format!(
                "    <{key}>{}</{key}>\n",
                escape_text(value)
            ));
        }
        out.push_str("  </Option>\n");

        // Tables
        out.push_str("  <Tables>\n");
        for table in &self.tables {
            out.push_str(&format!(
                "    <Table{} Description=\"{}\">\n",
                write_name_and_table_name(table),
                escape_attr(&table.description)
            ));

            out.push_str("      <Columns>\n");
            for col in &table.columns {
                out.push_str("        <Column");
                write_attr(&mut out, "Name", Some(&col.name));
                write_attr(&mut out, "ColumnName", col.column_name.as_deref());
                write_attr(&mut out, "DataType", Some(col.data_type.name()));
                write_attr(&mut out, "RawType", col.raw_type.as_deref());
                if col.length > 0 {
                    write_attr(&mut out, "Length", Some(&col.length.to_string()));
                }
                if col.precision > 0 {
                    write_attr(&mut out, "Precision", Some(&col.precision.to_string()));
                }
                if col.scale > 0 {
                    write_attr(&mut out, "Scale", Some(&col.scale.to_string()));
                }
                if col.identity {
                    write_attr(&mut out, "Identity", Some("True"));
                }
                if col.primary_key {
                    write_attr(&mut out, "PrimaryKey", Some("True"));
                }
                if col.master {
                    write_attr(&mut out, "Master", Some("True"));
                }
                if col.nullable {
                    write_attr(&mut out, "Nullable", Some("True"));
                }
                write_attr(&mut out, "DefaultValue", col.default_value.as_deref());
                write_attr(&mut out, "Type", col.enum_type.as_deref());
                write_attr(&mut out, "DataScale", col.data_scale.as_deref());
                write_attr(&mut out, "Map", col.map.as_deref());
                write_attr(&mut out, "ShowIn", col.show_in.as_deref());
                write_attr(&mut out, "Model", col.model.as_deref());
                write_attr(&mut out, "Description", Some(&col.description));
                out.push_str(" />\n");
            }
            out.push_str("      </Columns>\n");

            if !table.indexes.is_empty() {
                out.push_str("      <Indexes>\n");
                for idx in &table.indexes {
                    out.push_str("        <Index");
                    write_attr(&mut out, "Name", idx.name.as_deref());
                    write_attr(&mut out, "Columns", Some(&idx.columns.join(",")));
                    if idx.unique {
                        write_attr(&mut out, "Unique", Some("True"));
                    }
                    out.push_str(" />\n");
                }
                out.push_str("      </Indexes>\n");
            }

            out.push_str("    </Table>\n");
        }
        out.push_str("  </Tables>\n");
        out.push_str("</EntityModel>\n");
        out
    }
}

/// 解析 `<Option>` 子元素（每个子元素的局部名即键名）。
fn parse_options(node: &Node, options: &mut ModelOptions) {
    for child in node.children().filter(Node::is_element) {
        let key = child.tag_name().name().to_string();
        let value = child.text().unwrap_or("").trim().to_string();
        options.raw.insert(key, value);
    }
}

/// 解析 `<Table>`。
fn parse_table(node: &Node) -> Result<TableMeta> {
    let name = attr_string(node, "Name")
        .ok_or_else(|| Error::Model("存在缺少 Name 属性的 Table".to_string()))?;
    let table_name = attr_string(node, "TableName").unwrap_or_else(|| name.clone());

    let mut table = TableMeta {
        name,
        table_name,
        description: attr_string(node, "Description").unwrap_or_default(),
        conn_name: attr_string(node, "ConnName"),
        columns: Vec::new(),
        indexes: Vec::new(),
    };

    for child in node.children().filter(Node::is_element) {
        match child.tag_name().name() {
            "Columns" => {
                for col_node in child.children().filter(Node::is_element) {
                    if col_node.tag_name().name() == "Column" {
                        table.columns.push(parse_column(&col_node, &table.name)?);
                    }
                }
            }
            "Indexes" => {
                for idx_node in child.children().filter(Node::is_element) {
                    if idx_node.tag_name().name() == "Index" {
                        table.indexes.push(parse_index(&idx_node));
                    }
                }
            }
            _ => {}
        }
    }

    if table.columns.is_empty() {
        return Err(Error::Model(format!("表 {} 没有任何列定义", table.name)));
    }

    Ok(table)
}

/// 解析 `<Column>`。
fn parse_column(node: &Node, table_name: &str) -> Result<ColumnMeta> {
    let name = attr_string(node, "Name").ok_or_else(|| {
        Error::Model(format!("表 {table_name} 中存在缺少 Name 属性的 Column"))
    })?;
    let type_text = attr_string(node, "DataType").ok_or_else(|| {
        Error::Model(format!("表 {table_name} 的列 {name} 缺少 DataType 属性"))
    })?;
    let data_type = DataType::from_name(&type_text)
        .map_err(|e| Error::Model(format!("表 {table_name} 的列 {name}：{e}")))?;

    Ok(ColumnMeta {
        name,
        column_name: attr_string(node, "ColumnName"),
        data_type,
        raw_type: attr_string(node, "RawType"),
        length: attr_i32(node, "Length"),
        precision: attr_i32(node, "Precision"),
        scale: attr_i32(node, "Scale"),
        identity: attr_bool(node, "Identity"),
        primary_key: attr_bool(node, "PrimaryKey"),
        master: attr_bool(node, "Master"),
        nullable: attr_bool(node, "Nullable"),
        default_value: attr_string(node, "DefaultValue"),
        description: attr_string(node, "Description").unwrap_or_default(),
        enum_type: attr_string(node, "Type"),
        data_scale: attr_string(node, "DataScale"),
        map: attr_string(node, "Map"),
        show_in: attr_string(node, "ShowIn"),
        model: attr_string(node, "Model"),
    })
}

/// 解析 `<Index>`。
fn parse_index(node: &Node) -> IndexMeta {
    let columns = attr_string(node, "Columns")
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    IndexMeta {
        name: attr_string(node, "Name"),
        columns,
        unique: attr_bool(node, "Unique"),
    }
}

/// 读取字符串属性（空串视为 None）。
fn attr_string(node: &Node, name: &str) -> Option<String> {
    node.attribute(name)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// 读取整数属性（缺省 0）。
fn attr_i32(node: &Node, name: &str) -> i32 {
    node.attribute(name)
        .and_then(|v| v.trim().parse::<i32>().ok())
        .unwrap_or(0)
}

/// 读取布尔属性（与 XCode 一致：True/False，缺省 false）。
fn attr_bool(node: &Node, name: &str) -> bool {
    node.attribute(name)
        .map(|v| v.trim().eq_ignore_ascii_case("true") || v.trim() == "1")
        .unwrap_or(false)
}

/// 写出属性（值为 None 时跳过）。
fn write_attr(out: &mut String, name: &str, value: Option<&str>) {
    if let Some(v) = value
        && !v.is_empty()
    {
        out.push_str(&format!(" {name}=\"{}\"", escape_attr(v)));
    }
}

/// 拼装 Table 的 Name/TableName/ConnName 属性。
fn write_name_and_table_name(table: &TableMeta) -> String {
    let mut s = format!(" Name=\"{}\"", escape_attr(&table.name));
    if !table.table_name.is_empty() && table.table_name != table.name {
        s.push_str(&format!(" TableName=\"{}\"", escape_attr(&table.table_name)));
    }
    if let Some(conn) = &table.conn_name {
        s.push_str(&format!(" ConnName=\"{}\"", escape_attr(conn)));
    }
    s
}

/// XML 属性转义。
fn escape_attr(v: &str) -> String {
    v.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// XML 文本转义。
fn escape_text(v: &str) -> String {
    v.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<EntityModel xmlns="https://newlifex.com/Model202509.xsd" Version="1.0" ModelVersion="2.0">
  <Option>
    <Namespace>Demo.Entity</Namespace>
    <ConnName>DH</ConnName>
    <Output>.\</Output>
  </Option>
  <Tables>
    <Table Name="Order" TableName="DH_Order" Description="订单">
      <Columns>
        <Column Name="Id" DataType="Int32" Identity="True" PrimaryKey="True" Description="编号" />
        <Column Name="Code" DataType="String" Master="True" Nullable="True" Length="50" Description="单号" />
        <Column Name="Amount" DataType="Decimal" Precision="18" Scale="4" Description="金额" />
        <Column Name="CreateTime" DataType="DateTime" Description="创建时间" />
      </Columns>
      <Indexes>
        <Index Columns="Code" Unique="True" />
      </Indexes>
    </Table>
  </Tables>
</EntityModel>"#;

    /// 测试固件：生产 WMS 模型快照样本（7 张真实表）
    ///
    /// 覆盖生产模型实际使用的全部 8 种数据类型：
    /// String / Int32 / DateTime / Decimal / Boolean / Int64 / Int16 / Double
    const SAMPLE_MODEL: &str = include_str!("../tests/fixtures/wms_model_sample.xml");

    #[test]
    fn parse_sample() {
        let model = EntityModel::parse(SAMPLE).unwrap();
        assert_eq!(model.options.namespace(), Some("Demo.Entity"));
        assert_eq!(model.options.conn_name(), Some("DH"));
        assert_eq!(model.tables.len(), 1);

        let t = &model.tables[0];
        assert_eq!(t.name, "Order");
        assert_eq!(t.effective_table_name(), "DH_Order");
        assert_eq!(t.columns.len(), 4);

        let id = t.column("Id").unwrap();
        assert!(id.identity && id.primary_key && !id.nullable);

        let code = t.column("code").unwrap(); // 忽略大小写
        assert_eq!(code.data_type, DataType::String);
        assert!(code.master && code.nullable);
        assert_eq!(code.length, 50);

        let amount = t.column("Amount").unwrap();
        assert_eq!(amount.data_type, DataType::Decimal);
        assert_eq!((amount.precision, amount.scale), (18, 4));

        assert_eq!(t.primary_keys().len(), 1);
        assert_eq!(t.identity().unwrap().name, "Id");
        assert_eq!(t.indexes.len(), 1);
        assert!(t.indexes[0].unique);
        assert_eq!(t.indexes[0].columns, vec!["Code"]);
    }

    #[test]
    fn parse_production_model_sample() {
        let model = EntityModel::parse(SAMPLE_MODEL).expect("固件模型应可解析");
        assert_eq!(model.tables.len(), 7, "固件应包含 7 张真实表");

        let total_columns: usize = model.tables.iter().map(|t| t.columns.len()).sum();
        assert!(total_columns >= 100, "列数量：{total_columns}");

        // 抽查一张表与关键语义
        let jly = model.table("JiLiYu").expect("应存在 JiLiYu 表");
        assert_eq!(jly.effective_table_name(), "DH_JiLiYu");
        let id = jly.column("Id").unwrap();
        assert!(id.identity && id.primary_key);
        assert!(!id.nullable, "未声明 Nullable 的列应为 NOT NULL");

        // 枚举类型与精度字段
        let dev = model.table("HardwareDevices").unwrap();
        assert_eq!(dev.column("HType").unwrap().enum_type.as_deref(), Some("Dto.HardwareType"));

        // 固件应完整覆盖生产模型使用的 8 种数据类型
        let mut kinds = std::collections::BTreeSet::new();
        for t in &model.tables {
            for c in &t.columns {
                kinds.insert(c.data_type);
            }
        }
        assert_eq!(kinds.len(), 8, "固件应覆盖全部 8 种类型：{:?}", kinds);
    }

    /// 可选全量回归：设置环境变量 `RCODE_MODEL` 指向任意 XCode Model.xml
    /// （例如生产完整模型）后执行，验证大模型解析能力。
    #[test]
    fn full_model_regression_when_configured() {
        let Ok(path) = std::env::var("RCODE_MODEL") else {
            eprintln!(
                "未设置 RCODE_MODEL，跳过全量模型回归（示例：$env:RCODE_MODEL=\"...\\Model.xml\"; cargo test full_model_regression）"
            );
            return;
        };

        let model = EntityModel::load(std::path::Path::new(&path)).expect("全量模型应可解析");
        assert!(!model.tables.is_empty(), "全量模型不应为空");
        let total: usize = model.tables.iter().map(|t| t.columns.len()).sum();
        eprintln!("全量模型回归通过：{} 张表 / {} 列（{path}）", model.tables.len(), total);
    }

    #[test]
    fn xml_roundtrip() {
        let model = EntityModel::parse(SAMPLE).unwrap();
        let xml = model.to_xml();
        let again = EntityModel::parse(&xml).expect("写出的 XML 应能再次解析");
        assert_eq!(model, again, "解析 → 写出 → 解析 应完全一致");
    }

    #[test]
    fn xml_roundtrip_production_sample() {
        let model = EntityModel::parse(SAMPLE_MODEL).unwrap();
        let xml = model.to_xml();
        let again = EntityModel::parse(&xml).expect("生产模型样本写出后应能再次解析");
        assert_eq!(model.tables.len(), again.tables.len());
        assert_eq!(model.tables[0].columns, again.tables[0].columns);
    }

    #[test]
    fn subset_by_name() {
        let model = EntityModel::parse(SAMPLE_MODEL).unwrap();
        let sub = model.subset(&["JiLiYu", "DH_VerifyCode"]);
        assert_eq!(sub.tables.len(), 2); // 实体名与表名均可作为筛选条件
        assert_eq!(sub.tables[0].name, "JiLiYu");
    }

    #[test]
    fn bad_inputs_are_reported() {
        assert!(matches!(EntityModel::parse("<Foo/>"), Err(Error::Xml(_))));
        assert!(matches!(EntityModel::parse("not xml"), Err(Error::Xml(_))));

        let missing_type = r#"<EntityModel><Tables><Table Name="T">
            <Columns><Column Name="A"/></Columns></Table></Tables></EntityModel>"#;
        let err = EntityModel::parse(missing_type).unwrap_err().to_string();
        assert!(err.contains("DataType"), "{err}");

        let bad_type = r#"<EntityModel><Tables><Table Name="T">
            <Columns><Column Name="A" DataType="Guid"/></Columns></Table></Tables></EntityModel>"#;
        assert!(EntityModel::parse(bad_type).is_err());
    }
}
