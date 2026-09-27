//! Sql 模版（对应 DH.NCode `Configuration/SqlTemplate.cs`）。
//!
//! 一个 SQL 语句在不同数据库下的多种写法：`--[SqlServer]` 分段标记目标数据库，
//! 未标记的段落作为默认语句；[`SqlTemplate::get_sql`] 按数据库键选择，缺失时回退默认。
//!
//! C# 的 `ParseEmbedded`（嵌入资源）在 Rust 中由调用方读取文本后传入本模块解析。

use std::collections::BTreeMap;

/// Sql 模版（对齐 `SqlTemplate`）。
#[derive(Debug, Clone, Default)]
pub struct SqlTemplate {
    /// 名称。
    pub name: Option<String>,
    /// 默认 Sql 语句。
    pub sql: Option<String>,
    /// 特定数据库语句（小写键 → 语句，查找大小写不敏感）。
    pub sqls: BTreeMap<String, String>,
}

impl SqlTemplate {
    /// 实例化。
    pub fn new() -> Self {
        Self::default()
    }

    /// 从文本解析（对齐 `Parse`）。
    ///
    /// 逐行读取：`--[名称]` 行标记片段归属的数据库（名称在方括号内，如 `--[SqlServer]`），
    /// 其后各行累积为该片段的语句；无标记的片段作为默认语句 `sql`。
    /// 空行跳过；各片段内容 `trim` 后存储。
    /// <param name="text">模版文本</param>
    /// <returns>是否解析成功（恒为 true，与 C# 一致）</returns>
    pub fn parse(&mut self, text: &str) -> bool {
        let mut buffer = String::new();
        let mut name = String::new();

        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let mut current = line.to_string();
            if current.starts_with("--") {
                let p1 = current.find('[').map(|i| i as i64).unwrap_or(-1);
                let p2 = current.find(']').map(|i| i as i64).unwrap_or(-1);
                if p1 > 0 && p2 > p1 {
                    // 完成一个 sql 片段，开始新的片段
                    self.flush_fragment(&mut buffer, &name);
                    name = current[(p1 as usize + 1)..(p2 as usize)].to_string();
                    current.clear();
                }
            }
            if !current.is_empty() {
                buffer.push_str(&current);
                buffer.push('\n');
            }
        }

        // 完成最后一个 sql 片段
        self.flush_fragment(&mut buffer, &name);
        true
    }

    /// 获取指定数据库的 Sql，如果未指定，则返回默认（对齐 `GetSql`）。
    /// <param name="database">数据库键（如 `SqlServer`/`MySql`，大小写不敏感）</param>
    /// <returns>Sql 语句；没有默认语句时为 None</returns>
    pub fn get_sql(&self, database: &str) -> Option<&str> {
        if let Some(sql) = self.sqls.get(&database.to_ascii_lowercase()) {
            return Some(sql);
        }
        self.sql.as_deref()
    }

    /// 落盘当前片段（对齐 C# 中 `sb.Length > 0` 的收尾逻辑）。
    fn flush_fragment(&mut self, buffer: &mut String, name: &str) {
        if buffer.is_empty() {
            return;
        }
        let content = buffer.trim().to_string();
        if name.is_empty() {
            self.sql = Some(content);
        } else {
            self.sqls.insert(name.to_ascii_lowercase(), content);
        }
        buffer.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = r#"
SELECT ID, Name FROM SysUser WHERE Enable=1

--[SqlServer]
SELECT TOP 10 ID, Name FROM SysUser ORDER BY ID

--[MySql]
SELECT ID, Name FROM SysUser LIMIT 10
"#;

    #[test]
    fn parse_and_get_sql() {
        let mut t = SqlTemplate::new();
        assert!(t.parse(TEXT));
        assert_eq!(
            t.sql.as_deref(),
            Some("SELECT ID, Name FROM SysUser WHERE Enable=1")
        );
        assert_eq!(
            t.get_sql("SqlServer"),
            Some("SELECT TOP 10 ID, Name FROM SysUser ORDER BY ID")
        );
        // 键大小写不敏感
        assert_eq!(
            t.get_sql("mysql"),
            Some("SELECT ID, Name FROM SysUser LIMIT 10")
        );
        // 未指定的数据库回退默认
        assert_eq!(
            t.get_sql("Oracle"),
            Some("SELECT ID, Name FROM SysUser WHERE Enable=1")
        );
    }

    #[test]
    fn parse_multi_line_fragment_and_no_default() {
        let mut t = SqlTemplate::new();
        t.parse("--[PostgreSQL]\nSELECT 1\nFROM t\n");
        assert_eq!(t.sql, None);
        assert_eq!(t.get_sql("PostgreSQL"), Some("SELECT 1\nFROM t"));
        // 没有默认语句时返回 None
        assert_eq!(t.get_sql("SqlServer"), None);
        // -- 注释行不是标记行时保留在片段中（p1 > 0 约束）
        let mut t2 = SqlTemplate::new();
        t2.parse("[not-a-marker]\nSELECT 2");
        assert_eq!(t2.sql.as_deref(), Some("[not-a-marker]\nSELECT 2"));
    }
}
