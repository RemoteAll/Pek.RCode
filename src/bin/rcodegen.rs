//! `rcodegen` —— 实体生成工具（对应 DH.NCode 的 `xcode` 命令 / XCodeTool）。
//!
//! 从 `Model.xml` 生成 Rust **对象实体**（结构体 + `new()`/`Default` + `Entity` 实现，
//! 自带 insert/save/update/delete/find/query/count）。
//!
//! 用法见 `rcodegen --help`。

use std::{
    env, fs,
    path::PathBuf,
    process::ExitCode,
};

use pek_rcode::{
    codegen,
    model::{EntityModel, TableMeta},
};

/// 生成文件标记：带此标记的文件可被本工具自动覆盖（保护手写代码）
const GEN_MARKER: &str = "由 pek-rcode 从 Model.xml 自动生成";

/// 命令行选项。
struct Options {
    /// 模型文件路径
    model: PathBuf,
    /// 输出目录（None 表示取模型 Output 配置，缺省 ./entities）
    out: Option<PathBuf>,
    /// 只生成指定表（逗号分隔可重复）
    tables: Vec<String>,
    /// 仅列出模型中的表
    list: bool,
    /// 覆盖非本工具生成的文件
    force: bool,
    /// 只显示不写盘
    dry_run: bool,
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();

    let opts = match parse_args(&args) {
        Ok(Some(o)) => o,
        Ok(None) => return ExitCode::SUCCESS, // --help / --version 已输出
        Err(e) => {
            eprintln!("参数错误：{e}\n");
            print_usage();
            return ExitCode::from(2);
        }
    };

    match run(&opts) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(1)
        }
    }
}

/// 执行生成。
fn run(opts: &Options) -> Result<(), String> {
    if !opts.model.is_file() {
        return Err(format!(
            "模型文件不存在：{}（可用 --model 指定）",
            opts.model.display()
        ));
    }

    let model = EntityModel::load(&opts.model).map_err(|e| e.to_string())?;

    if opts.list {
        print_table_list(&model, &opts.model);
        return Ok(());
    }

    // 选择待生成的表
    let tables: Vec<&TableMeta> = if opts.tables.is_empty() {
        model.tables.iter().collect()
    } else {
        let mut selected = Vec::with_capacity(opts.tables.len());
        for name in &opts.tables {
            let table = model
                .table(name)
                .ok_or_else(|| format!("模型中不存在表/实体：{name}"))?;
            selected.push(table);
        }
        selected
    };
    if tables.is_empty() {
        return Err("模型中没有可生成的表".into());
    }

    // 输出目录：--out > 模型 Output 配置 > ./entities
    let out_dir = opts
        .out
        .clone()
        .or_else(|| model.options.output().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("entities"));

    if !opts.dry_run {
        fs::create_dir_all(&out_dir)
            .map_err(|e| format!("创建输出目录 {} 失败：{e}", out_dir.display()))?;
    }

    let mut created = 0usize;
    let mut updated = 0usize;
    let mut unchanged = 0usize;
    let mut skipped: Vec<String> = Vec::new();

    for table in tables {
        let file_name = codegen::file_name(table);
        let path = out_dir.join(&file_name);
        let code = codegen::generate(table);

        match fs::read_to_string(&path) {
            Ok(existing) => {
                if existing == code {
                    unchanged += 1;
                    continue;
                }
                // 保护手写文件：仅带生成标记的文件允许自动覆盖
                if !existing.contains(GEN_MARKER) && !opts.force {
                    skipped.push(file_name);
                    continue;
                }
                if opts.dry_run {
                    println!("[将更新] {}", path.display());
                } else {
                    fs::write(&path, code)
                        .map_err(|e| format!("写入 {} 失败：{e}", path.display()))?;
                    println!("[更新] {}", path.display());
                }
                updated += 1;
            }
            Err(_) => {
                if opts.dry_run {
                    println!("[将生成] {}", path.display());
                } else {
                    fs::write(&path, code)
                        .map_err(|e| format!("写入 {} 失败：{e}", path.display()))?;
                    println!("[生成] {}", path.display());
                }
                created += 1;
            }
        }
    }

    println!();
    if opts.dry_run {
        println!("（--dry-run 模式，未写入任何文件）");
    }
    println!("输出目录：{}", out_dir.display());
    println!("生成 {created}，更新 {updated}，未变化 {unchanged}，跳过 {}", skipped.len());
    if !skipped.is_empty() {
        println!("以下文件不是本工具生成的（内容已变化但未覆盖，可用 --force 强制覆盖）：");
        for name in &skipped {
            println!("  {name}");
        }
    }

    Ok(())
}

/// 列出模型中的全部表。
#[allow(clippy::print_literal)] // 表头对齐需要格式化宽度，保留字面量更直观
fn print_table_list(model: &EntityModel, path: &std::path::Path) {
    println!(
        "{} 共 {} 张表（模型：{}）\n",
        model.options.namespace().unwrap_or("（未设置命名空间）"),
        model.tables.len(),
        path.display()
    );
    println!("{:<30} {:<34} {:>5}  {}", "实体名", "表名", "列数", "说明");
    println!("{:-<96}", "");
    for table in &model.tables {
        println!(
            "{:<30} {:<34} {:>5}  {}",
            table.name,
            table.effective_table_name(),
            table.columns.len(),
            table.description
        );
    }
}

/// 解析命令行参数；`Ok(None)` 表示已输出帮助/版本，正常退出。
fn parse_args(args: &[String]) -> Result<Option<Options>, String> {
    let mut opts = Options {
        model: PathBuf::from("Model.xml"),
        out: None,
        tables: Vec::new(),
        list: false,
        force: false,
        dry_run: false,
    };

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print_usage();
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("rcodegen {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "-m" | "--model" => opts.model = PathBuf::from(next_value(&mut iter, arg)?),
            "-o" | "--out" => opts.out = Some(PathBuf::from(next_value(&mut iter, arg)?)),
            "-t" | "--table" => {
                let value = next_value(&mut iter, arg)?;
                opts.tables.extend(
                    value
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty()),
                );
            }
            "-l" | "--list" => opts.list = true,
            "-f" | "--force" => opts.force = true,
            "--dry-run" => opts.dry_run = true,
            other => return Err(format!("未知参数 {other}")),
        }
    }

    Ok(Some(opts))
}

/// 读取选项的值。
fn next_value<'a>(
    iter: &mut impl Iterator<Item = &'a String>,
    flag: &str,
) -> Result<String, String> {
    iter.next()
        .cloned()
        .ok_or_else(|| format!("{flag} 缺少参数值"))
}

/// 输出帮助。
fn print_usage() {
    println!(
        r#"rcodegen —— Model.xml 实体生成工具（Rust 版 xcode 命令）

用法：
  rcodegen [选项]

选项：
  -m, --model <文件>   模型文件路径（默认 ./Model.xml）
  -o, --out <目录>     输出目录（默认取模型 Output 配置，否则 ./entities）
  -t, --table <名称>   只生成指定表/实体（逗号分隔，可重复指定）
  -l, --list           列出模型中的全部表后退出
  -f, --force          覆盖非本工具生成的文件（默认仅覆盖带生成标记的文件）
      --dry-run        只显示将要生成的文件，不写盘
  -h, --help           显示本帮助
  -V, --version        显示版本

示例：
  # 查看模型里有哪些表
  rcodegen --model ..\..\<你的项目>\Entity\Model.xml --list

  # 只生成两张表到指定目录
  rcodegen --model ..\..\<你的项目>\Entity\Model.xml --out src\entities --table JiLiYu,VerifyCode

  # 预览将生成的文件（不写盘）
  rcodegen --model ..\..\<你的项目>\Entity\Model.xml --dry-run"#
    );
}
