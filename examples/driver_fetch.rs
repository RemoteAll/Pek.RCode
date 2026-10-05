//! 驱动包按需分发演示：从平台组件源下载 `dbserver` 驱动包、拉起宿主，并以 `provider=network` 连接。
//!
//! 运行（需启用特性 `driver-pack`）：
//!
//! ```powershell
//! cargo run --example driver_fetch -- <平台地址> <公钥hex> ["<真实连接串>"] ["<根证书PEM路径>"]
//! # 例：
//! cargo run --example driver_fetch -- http://127.0.0.1:5502 <64位hex> "Server=127.0.0.1;Database=demo;Provider=MySql"
//! # 自签/内网 CA 的 https 组件源：第 4 参传根证书 PEM 文件路径
//! cargo run --example driver_fetch -- https://drv.example.com <64位hex> "" "Config/store-ca.pem"
//! ```
//!
//! 平台地址与公钥来自 Pek.RPanlServer「下载管理」页（组件源卡片）。

use pek_rcode::driver_pack::{DriverManager, DriverManagerConfig};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("用法：driver_fetch <平台地址> <公钥hex> [真实连接串] [根证书PEM路径]");
        eprintln!(
            "示例：driver_fetch http://127.0.0.1:5502 <64位hex> \"Server=127.0.0.1;Database=demo;Provider=MySql\""
        );
        std::process::exit(2);
    }
    let store = args[0].clone();
    let pubkey = args[1].clone();
    let real = args
        .get(2)
        .filter(|s| !s.is_empty())
        .cloned()
        .unwrap_or_else(|| "Server=127.0.0.1;Database=demo;Provider=MySql".to_string());
    // 可选：https 自签/内网 CA 的根证书 PEM 路径（第 4 参或环境变量 PEK_DRIVER_CA）
    let ca_path = args
        .get(3)
        .filter(|s| !s.is_empty())
        .cloned()
        .or_else(|| std::env::var("PEK_DRIVER_CA").ok().filter(|s| !s.is_empty()));
    let ca_pem = ca_path
        .map(|p| std::fs::read(&p).unwrap_or_else(|e| panic!("读根证书失败（{p}）：{e}")));

    let mgr = match DriverManager::new(DriverManagerConfig {
        store_url: store,
        pubkey,
        ca_pem,
        ..Default::default()
    }) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("初始化失败：{e}");
            std::process::exit(1);
        }
    };

    println!("== 确保驱动就绪（联网检查更新）：{real}");
    let conn = match mgr.ensure_updated(&real) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("失败：{e}");
            std::process::exit(1);
        }
    };
    println!("network 连接串：{conn}");
    for host in mgr.hosts() {
        println!(
            "宿主：{} {} pid={} addr={}（空闲 {}s）",
            host.component, host.version, host.pid, host.addr, host.idle_secs
        );
    }

    // 用 network 驱动打开（登录远端探明数据库类型）
    match pek_rcode::dal::Dal::open(&conn) {
        Ok(dal) => println!("打开成功：数据库类型 {}", dal.kind().name()),
        Err(e) => {
            eprintln!("network 打开失败：{e}");
            std::process::exit(1);
        }
    }
    println!("== 演示结束，停止宿主");
    mgr.stop_all();
}
