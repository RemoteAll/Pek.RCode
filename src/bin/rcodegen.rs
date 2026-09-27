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
    Dal, codegen,
    model::{EntityModel, TableMeta},
};

/// 生成文件标记：带此标记的文件可被本工具自动覆盖（保护手写代码）
const GEN_MARKER: &str = "由 pek-rcode 从 Model.xml 自动生成";

/// 命令行选项。
struct Options {
    /// 模型文件路径
    model: PathBuf,
    /// 反向工程连接串（设置后进入反向模式：数据库 → Model.xml）
    conn: Option<String>,
    /// 输出目录（正向）/ 输出文件（反向；缺省 Model.xml）
    out: Option<PathBuf>,
    /// 只生成指定表（逗号分隔可重复）
    tables: Vec<String>,
    /// 仅列出模型中的表
    list: bool,
    /// 覆盖非本工具生成的文件
    force: bool,
    /// 只显示不写盘
    dry_run: bool,
    /// 生成类型（entity/model/interface；默认 entity）
    kind: Vec<String>,
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

/// 执行生成（正向：模型 → 实体；反向：数据库 → 模型，由 `--conn` 触发）。
fn run(opts: &Options) -> Result<(), String> {
    // 反向模式：数据库结构 → Model.xml（对应 C# 的 DAL.GetTables）
    if let Some(conn) = &opts.conn {
        return run_reverse(opts, conn);
    }

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

    // 生成类型：缺省仅实体；校验合法性
    let kinds: Vec<String> = if opts.kind.is_empty() {
        vec!["entity".to_string()]
    } else {
        opts.kind.clone()
    };
    for kind in &kinds {
        if !matches!(kind.as_str(), "entity" | "model" | "interface") {
            return Err(format!("未知生成类型 {kind}（支持 entity/model/interface）"));
        }
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
        let outputs: Vec<(String, String)> = kinds
            .iter()
            .map(|kind| match kind.as_str() {
                "model" => (codegen::model_file_name(table), codegen::generate_model(table)),
                "interface" => (
                    codegen::interface_file_name(table),
                    codegen::generate_interface(table),
                ),
                _ => (codegen::file_name(table), codegen::generate(table)),
            })
            .collect();

        for (file_name, code) in outputs {
            let path = out_dir.join(&file_name);

            match dhrust::io::read_all_text(&path) {
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
                        dhrust::io::write_all_text(&path, &code)
                            .map_err(|e| format!("写入 {} 失败：{e}", path.display()))?;
                        println!("[更新] {}", path.display());
                    }
                    updated += 1;
                }
                Err(_) => {
                    if opts.dry_run {
                        println!("[将生成] {}", path.display());
                    } else {
                        dhrust::io::write_all_text(&path, &code)
                            .map_err(|e| format!("写入 {} 失败：{e}", path.display()))?;
                        println!("[生成] {}", path.display());
                    }
                    created += 1;
                }
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

/// 反向工程：读取数据库结构并输出 `Model.xml`（对应 C# 的 `DAL.GetTables`）。
fn run_reverse(opts: &Options, conn: &str) -> Result<(), String> {
    let dal = Dal::open(conn).map_err(|e| e.to_string())?;

    if opts.list {
        let names = dal.read_table_names().map_err(|e| e.to_string())?;
        println!(
            "{} 共 {} 张表（{}）\n",
            dal.kind().name(),
            names.len(),
            dal.connection_string().data_source().unwrap_or("")
        );
        for (index, name) in names.iter().enumerate() {
            println!("{:>4}. {name}", index + 1);
        }
        return Ok(());
    }

    let model = dal.read_model().map_err(|e| e.to_string())?;
    let out = opts.out.clone().unwrap_or_else(|| PathBuf::from("Model.xml"));

    if opts.dry_run {
        println!("（--dry-run 模式，未写入文件）");
        println!(
            "将反向生成 {} 张表 → {}",
            model.tables.len(),
            out.display()
        );
        return Ok(());
    }

    dhrust::io::write_all_text(&out, &model.to_xml())
        .map_err(|e| format!("写入 {} 失败：{e}", out.display()))?;
    println!(
        "反向工程完成：{} 张表 → {}（{}）",
        model.tables.len(),
        out.display(),
        dal.kind().name()
    );
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
        conn: None,
        out: None,
        tables: Vec::new(),
        list: false,
        force: false,
        dry_run: false,
        kind: Vec::new(),
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
            "-c" | "--conn" => opts.conn = Some(next_value(&mut iter, arg)?),
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
            "-k" | "--kind" => {
                let value = next_value(&mut iter, arg)?;
                opts.kind.extend(
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
  -o, --out <路径>     输出目录（正向，默认取模型 Output 配置，否则 ./entities）
                       或输出文件（反向，默认 ./Model.xml）
  -t, --table <名称>   只生成指定表/实体（逗号分隔，可重复指定）
  -k, --kind <类型>    生成类型：entity（实体，默认）/ model（模型类）/ interface（接口），
                       逗号分隔可多选
  -c, --conn <串>      反向工程连接串（与 XCode 一致，如 Data Source=x.db;Provider=SQLite），
                       设置后进入反向模式：数据库 → Model.xml
  -l, --list           列出表后退出（正向：模型表；反向：数据库表）
  -f, --force          覆盖非本工具生成的文件（默认仅覆盖带生成标记的文件）
      --dry-run        只显示将要生成的内容，不写盘
  -h, --help           显示本帮助
  -V, --version        显示版本

示例：
  # 查看模型里有哪些表
  rcodegen --model ..\..\<你的项目>\Entity\Model.xml --list

  # 只生成两张表到指定目录
  rcodegen --model ..\..\<你的项目>\Entity\Model.xml --out src\entities --table JiLiYu,VerifyCode

  # 预览将生成的文件（不写盘）
  rcodegen --model ..\..\<你的项目>\Entity\Model.xml --dry-run

  # 反向工程：列出数据库表 / 从数据库生成 Model.xml
  rcodegen --conn "Data Source=..\Data\DG.db;Provider=SQLite" --list
  rcodegen --conn "Data Source=..\Data\DG.db;Provider=SQLite" --out Model.xml"#
    );
}
