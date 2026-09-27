//! 显示位置选项（对应 DH.NCode 的 `XCode.Configuration.ShowInOption`）。
//!
//! 单字段 `ShowIn` 表达列在 5 个区域的显示策略：`List`、`Detail`、`AddForm`、`EditForm`、`Search`，
//! 每个区域三态：`Auto`（自动）/ `Show`（显示）/ `Hide`（隐藏）。
//!
//! 支持三种等价语法（与 C# 版一致）：
//! - **具名列表 + 宏**：`List,Search`、`-EditForm,-Detail`、`All,-Detail`、`None,Search,Add`、`Auto`；
//!   无前缀=显示，`-` 前缀=隐藏；区域别名 `List(L) / Detail(D) / AddForm(Add) / EditForm(Edit) / Search(S)`
//! - **管道 5 段**：`Y|Y|N||A`（顺序 List|Detail|AddForm|EditForm|Search；`Y/N/A/空`）
//! - **5 字符掩码**：`110A?`（`1`=Show、`0`=Hide、`A`/`?`/`-`=Auto）
//!
//! 未指定的区域均为 `Auto`（由系统原有规则决定）。

/// 三态枚举。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriState {
    /// 自动（系统决定）
    Auto,
    /// 显示
    Show,
    /// 隐藏
    Hide,
}

/// 显示位置选项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShowInOption {
    /// 列表页
    pub list: TriState,
    /// 明细页
    pub detail: TriState,
    /// 添加表单
    pub add_form: TriState,
    /// 编辑表单
    pub edit_form: TriState,
    /// 搜索区
    pub search: TriState,
}

impl ShowInOption {
    /// 默认全 `Auto`。
    pub fn auto_all() -> Self {
        Self {
            list: TriState::Auto,
            detail: TriState::Auto,
            add_form: TriState::Auto,
            edit_form: TriState::Auto,
            search: TriState::Auto,
        }
    }

    /// 解析字符串（三种语法，见模块文档）。
    pub fn parse(text: &str) -> Self {
        let text = text.trim();
        if text.is_empty() {
            return Self::auto_all();
        }

        // 管道 5 段
        if text.contains('|') {
            let mut segs: Vec<&str> = text.split('|').collect();
            while segs.len() < 5 {
                segs.push("");
            }
            return Self {
                list: parse_yn(segs[0]),
                detail: parse_yn(segs[1]),
                add_form: parse_yn(segs[2]),
                edit_form: parse_yn(segs[3]),
                search: parse_yn(segs[4]),
            };
        }

        // 5 字符掩码：1/0/A/?/-
        if text.chars().count() == 5
            && text
                .chars()
                .all(|c| matches!(c, '1' | '0' | 'a' | 'A' | '?' | '-'))
        {
            let chars: Vec<char> = text.chars().collect();
            return Self {
                list: parse_mask(chars[0]),
                detail: parse_mask(chars[1]),
                add_form: parse_mask(chars[2]),
                edit_form: parse_mask(chars[3]),
                search: parse_mask(chars[4]),
            };
        }

        // 具名列表 + 宏（顺序覆盖）
        let mut option = Self::auto_all();
        for token in text.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let lower = token.to_ascii_lowercase();
            match lower.as_str() {
                "auto" => option = Self::auto_all(),
                "all" => {
                    option = Self {
                        list: TriState::Show,
                        detail: TriState::Show,
                        add_form: TriState::Show,
                        edit_form: TriState::Show,
                        search: TriState::Show,
                    }
                }
                "none" => {
                    option = Self {
                        list: TriState::Hide,
                        detail: TriState::Hide,
                        add_form: TriState::Hide,
                        edit_form: TriState::Hide,
                        search: TriState::Hide,
                    }
                }
                _ => {
                    let (state, name) = match lower.strip_prefix('-') {
                        Some(rest) => (TriState::Hide, rest),
                        None => (TriState::Show, lower.as_str()),
                    };
                    match name {
                        "list" | "l" => option.list = state,
                        "detail" | "d" => option.detail = state,
                        "addform" | "add" => option.add_form = state,
                        "editform" | "edit" => option.edit_form = state,
                        "search" | "s" => option.search = state,
                        // 未知标记忽略（向前兼容）
                        _ => {}
                    }
                }
            }
        }
        option
    }

    /// 搜索区是否显式显示。
    pub fn search_show(&self) -> bool {
        matches!(self.search, TriState::Show)
    }

    /// 搜索区是否显式隐藏。
    pub fn search_hide(&self) -> bool {
        matches!(self.search, TriState::Hide)
    }
}

impl Default for ShowInOption {
    fn default() -> Self {
        Self::auto_all()
    }
}

/// 解析 `Y/N/A/空`。
fn parse_yn(text: &str) -> TriState {
    match text.trim().to_ascii_uppercase().as_str() {
        "Y" | "YES" | "TRUE" | "1" | "SHOW" => TriState::Show,
        "N" | "NO" | "FALSE" | "0" | "HIDE" => TriState::Hide,
        _ => TriState::Auto,
    }
}

/// 解析掩码字符。
fn parse_mask(ch: char) -> TriState {
    match ch {
        '1' => TriState::Show,
        '0' => TriState::Hide,
        _ => TriState::Auto,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_list_syntax() {
        let option = ShowInOption::parse("List,Search");
        assert_eq!(option.list, TriState::Show);
        assert_eq!(option.search, TriState::Show);
        assert_eq!(option.detail, TriState::Auto);
        assert!(option.search_show());

        let option = ShowInOption::parse("-EditForm,-Detail");
        assert_eq!(option.edit_form, TriState::Hide);
        assert_eq!(option.detail, TriState::Hide);
        assert_eq!(option.list, TriState::Auto);

        let option = ShowInOption::parse("All,-Detail");
        assert_eq!(option.list, TriState::Show);
        assert_eq!(option.search, TriState::Show);
        assert_eq!(option.detail, TriState::Hide);

        let option = ShowInOption::parse("None,Search,Add");
        assert_eq!(option.list, TriState::Hide);
        assert_eq!(option.search, TriState::Show);
        assert_eq!(option.add_form, TriState::Show);

        // 别名与大小写
        let option = ShowInOption::parse("l,S");
        assert_eq!(option.list, TriState::Show);
        assert_eq!(option.search, TriState::Show);
    }

    #[test]
    fn pipe_and_mask_syntax() {
        let option = ShowInOption::parse("Y|Y|N||A");
        assert_eq!(option.list, TriState::Show);
        assert_eq!(option.detail, TriState::Show);
        assert_eq!(option.add_form, TriState::Hide);
        assert_eq!(option.edit_form, TriState::Auto);
        assert_eq!(option.search, TriState::Auto);

        let option = ShowInOption::parse("|||N|");
        assert_eq!(option.edit_form, TriState::Hide);
        assert_eq!(option.list, TriState::Auto);

        let option = ShowInOption::parse("110A?");
        assert_eq!(option.list, TriState::Show);
        assert_eq!(option.detail, TriState::Show);
        assert_eq!(option.add_form, TriState::Hide);
        assert_eq!(option.edit_form, TriState::Auto);
        assert_eq!(option.search, TriState::Auto);
        assert!(!option.search_hide());

        let option = ShowInOption::parse("-----");
        assert_eq!(option, ShowInOption::auto_all());

        let option = ShowInOption::parse("AAAAA");
        assert_eq!(option, ShowInOption::auto_all());
    }

    #[test]
    fn defaults() {
        assert_eq!(ShowInOption::parse(""), ShowInOption::auto_all());
        assert_eq!(ShowInOption::parse("Auto"), ShowInOption::auto_all());
        assert_eq!(ShowInOption::default(), ShowInOption::auto_all());
    }
}
