//! 历史遗留修复：**名字里带反斜杠的库文件归位**（Linux 上才会出现的坑）。
//!
//! 早期版本按 `Path::new("Data\\DH.db")` 拼 SQLite 路径：Windows 上反斜杠是分隔符，
//! 但 **Linux 上不是** —— 于是 `base.join("Data\\DH.db")` 得到的是一个「文件名」，
//! 在部署根生成了 `Data\DH.db`（以及 `-shm` / `-wal`），`Data/` 目录里反而没有
//! （线上实测：`zzz.hlktech.com/Data\DH.db` 就躺在根目录，运维看到的是空库）。
//!
//! 连接串解析已把 `\` 按 `/` 处理（见 [`crate::database::resolve_connection`]），
//! 启动时调 [`relocate_backslash_files`] 把旧文件搬到正确位置，升级后数据仍然在。
//!
//! 下沉自 Pek.Proxy `db::relocate_backslash_db_files` / `relocated_path`（其单测一并搬来）。

use std::path::{Path, PathBuf};

/// 把文件名里带反斜杠的路径映射到「按目录层级展开」的目标路径。
///
/// 典型：`Data\DH.db` → `<base>/Data/DH.db`。不含反斜杠返回 `None`。
///
/// 注意 Windows 上文件名**不可能**含反斜杠（它是分隔符），所以带反斜杠的名字只会出现在
/// Linux/Unix 部署里 —— 这正是本函数存在的原因。
pub fn relocated_path(base: &Path, name: &str) -> Option<PathBuf> {
    if !name.contains('\\') {
        return None;
    }
    Some(base.join(name.replace('\\', "/")))
}

/// 扫描 `base` 根目录，把名字里带反斜杠的文件搬到展开后的目录里（已存在则跳过并告警）。
///
/// - 只处理**文件**（目录不动）；
/// - 目标已存在 → 不覆盖，打告警让运维人工核对（数据安全优先）；
/// - 目标目录缺失 → 先建目录；
/// - 返回实际搬迁的条数（便于启动日志/测试断言）。
///
/// `base`：部署根目录。
pub fn relocate_backslash_files(base: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(base) else {
        return 0;
    };
    let mut moved = 0usize;
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(relocated) = relocated_path(base, name) else {
            continue;
        };
        if !entry.metadata().map(|m| m.is_file()).unwrap_or(false) {
            continue;
        }
        if relocated.exists() {
            dhrust::warn!(
                "[db] 发现反斜杠库文件 {}，但目标 {} 已存在，跳过搬迁（请人工核对）",
                path.display(),
                relocated.display()
            );
            continue;
        }
        // 目标目录缺失先建（失败就跳过这一条，不影响其它文件）
        let created = relocated
            .parent()
            .filter(|dir| !dir.exists())
            .map(|dir| (dir.to_path_buf(), std::fs::create_dir_all(dir)));
        if let Some((dir, Err(err))) = created {
            dhrust::warn!("[db] 反斜杠库文件搬迁时创建目录失败（{}）：{err}", dir.display());
            continue;
        }

        match std::fs::rename(&path, &relocated) {
            Ok(()) => {
                moved += 1;
                dhrust::info!(
                    "[db] 已把反斜杠库文件归位：{} → {}",
                    path.display(),
                    relocated.display()
                );
            }
            Err(err) => dhrust::warn!(
                "[db] 反斜杠库文件搬迁失败：{} → {}：{err}",
                path.display(),
                relocated.display()
            ),
        }
    }
    moved
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backslash_name_maps_to_nested_path() {
        // 线上遗留形态：文件名里带反斜杠（只可能出现在 Unix）
        let base = Path::new("deploy");
        let target = relocated_path(base, r"Data\DH.db").expect("含反斜杠应给出归位目标");
        let comps: Vec<String> = target
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .collect();
        assert!(comps.iter().any(|c| c == "Data"), "应拆出 Data 目录：{comps:?}");
        // 关键：**文件名部分**不能再带反斜杠
        let leaf = target
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        assert_eq!(leaf, "DH.db", "文件名部分不应含反斜杠：{leaf}");
        // `-wal` / `-shm` 同样要跟着搬
        for name in [r"Data\DH.db-wal", r"Data\DH.db-shm"] {
            let path = relocated_path(base, name).expect("应能归位");
            let leaf = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            assert!(leaf.starts_with("DH.db-"), "文件名应保留：{leaf}");
        }
        // 正常名字不动
        assert!(relocated_path(base, "DH.db").is_none());
    }

    /// 真实搬迁只在 Unix 上有意义：Windows 文件名不能含 `\`（它是分隔符），
    /// `join(r"Data\DH.db")` 会变成子目录，写文件直接失败。
    #[cfg(unix)]
    #[test]
    fn relocate_moves_legacy_file_and_keeps_target_when_exists() {
        let dir = std::env::temp_dir().join(format!(
            "pek-rcode-relocate-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建目录");
        // 线上遗留：根目录下的 `Data\DH.db`
        let legacy = dir.join(r"Data\DH.db");
        std::fs::write(&legacy, b"LEGACY").expect("写旧文件");
        assert_eq!(relocate_backslash_files(&dir), 1, "应搬走 1 个");
        assert!(dir.join("Data").join("DH.db").is_file(), "目标应就位");
        assert!(!legacy.exists(), "旧文件应已移走");

        // 目标已存在时不能覆盖（数据安全优先）
        std::fs::write(&legacy, b"NEW").expect("再放一个旧文件");
        assert_eq!(relocate_backslash_files(&dir), 0, "目标存在应跳过");
        assert_eq!(
            std::fs::read(dir.join("Data").join("DH.db")).unwrap(),
            b"LEGACY",
            "已有数据不能被覆盖"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
