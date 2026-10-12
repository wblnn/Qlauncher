// src/path_safety.rs
//! 远程清单路径安全拼接（QL-01 / QL-02 / L-02 / L-03）。
//!
//! 清单里的相对路径全部由远程响应决定，必须先在词法层面拒绝：
//! 绝对路径 / 盘符 / UNC / 父目录上跳 / 保留设备名 / NTFS 数据流；
//! 再对「已存在的最近祖先」做 canonicalize 比较，挡住符号链接与目录联接；
//! 之后才允许用于 create_dir_all / OpenOptions / rename / remove_file。
//!
//! 注意事项：
//! - 不要用未规范化的路径直接做 starts_with 前缀检查：
//!   gamedir\..\x 的字面前缀仍然成立，检查会失效。
//! - 不要用 dunce::canonicalize：它的语义是去掉 verbatim 前缀，且 canonicalize
//!   要求路径已存在，会破坏首次安装（root 尚未创建）。这里只在 root 与最近祖先
//!   都存在时才比较，其余情况依赖词法校验已经保证不会跳出 root。

use std::path::{Component, Path, PathBuf};

/// Windows 保留设备名（大小写无关），出现在任意路径段即拒绝。
const RESERVED_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL",
    "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9",
    "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// 把远程清单给出的相对路径 rel 安全地拼到 root 之下。
///
/// 任何越界形态都返回 Err，调用方应把它当作致命错误中止本次更新
/// （fallback 到「跳过这一条」会让恶意清单继续推进）。
pub fn safe_join_under(root: &Path, rel: &str) -> Result<PathBuf, String> {
    validate_rel(rel)?;
    let joined = root.join(rel);
    ensure_within(root, &joined)?;
    Ok(joined)
}

/// 只做词法校验，不要求路径存在。供需要保留原始字符串语义的调用点复用。
pub fn validate_relative(rel: &str) -> Result<(), String> {
    validate_rel(rel)
}

fn validate_rel(rel: &str) -> Result<(), String> {
    if rel.trim().is_empty() {
        return Err("清单路径为空".to_string());
    }
    if rel.contains('\0') {
        return Err("清单路径含 NUL 字节".to_string());
    }
    // URL 编码变体（..%2f / ..%5c / %2e%2e）在解码后若仍是穿越形态则拒绝；
    // 本层不做解码，但解码后的结果同样必须通过词法校验。
    if rel.contains('%') {
        if let Some(decoded) = percent_decode(rel) {
            if decoded != rel {
                check_components(&decoded)?;
            }
        }
    }
    check_components(rel)
}

fn check_components(rel: &str) -> Result<(), String> {
    let mut normals = 0usize;
    for comp in Path::new(rel).components() {
        match comp {
            // "." 本身不改变层级，容忍
            Component::CurDir => continue,
            Component::Normal(seg) => {
                let s = seg.to_string_lossy();
                if s.is_empty() {
                    return Err(format!("清单路径含空路径段: {}", rel));
                }
                if s.contains(':') {
                    return Err(format!("清单路径段含冒号（可能是 NTFS 数据流）: {}", s));
                }
                if s.ends_with('.') || s.ends_with(' ') {
                    return Err(format!("清单路径段以点或空格结尾（Windows 会截断为同名）: {}", s));
                }
                if is_reserved_device_name(&s) {
                    return Err(format!("清单路径段是 Windows 保留设备名: {}", s));
                }
                normals += 1;
            }
            Component::ParentDir => {
                return Err(format!("清单路径含父目录上跳 ..（路径穿越）: {}", rel));
            }
            Component::RootDir => {
                return Err(format!("清单路径是绝对路径（从根开始）: {}", rel));
            }
            Component::Prefix(p) => {
                return Err(format!("清单路径带盘符/UNC 前缀 {:?}: {}", p, rel));
            }
        }
    }
    if normals == 0 {
        return Err(format!("清单路径没有任何有效路径段: {}", rel));
    }
    Ok(())
}

fn is_reserved_device_name(seg: &str) -> bool {
    // Windows 比较时忽略扩展名与结尾空格
    let stem = seg.split('.').next().unwrap_or(seg).trim_end_matches(' ');
    let up = stem.to_ascii_uppercase();
    RESERVED_NAMES.contains(&up.as_str())
}

/// 简单百分号解码，仅用于识别 ..%2f / %2e%2e 之类的编码穿越。
/// 返回 None 表示没有发生解码或解码结果不是合法 UTF-8。
fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0usize;
    let mut changed = false;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hi = (b[i + 1] as char).to_digit(16);
            let lo = (b[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                changed = true;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    if !changed {
        return None;
    }
    String::from_utf8(out).ok()
}

/// 符号链接 / 目录联接防护：把 root 与 joined 的「已存在的最近祖先」分别
/// canonicalize 后比较。两者都带 Windows verbatim 前缀，前缀比较是可靠的。
fn ensure_within(root: &Path, joined: &Path) -> Result<(), String> {
    let Ok(canon_root) = std::fs::canonicalize(root) else {
        // root 还不存在（首次安装）：词法校验已保证不跳出 root
        return Ok(());
    };
    let Some(existing) = nearest_existing(joined) else {
        return Ok(());
    };
    let Ok(canon_existing) = std::fs::canonicalize(existing) else {
        return Ok(());
    };
    if canon_existing.starts_with(&canon_root) {
        Ok(())
    } else {
        Err(format!(
            "清单路径经符号链接/目录联接指向 root 之外: {} -> {}",
            joined.display(),
            canon_existing.display()
        ))
    }
}

fn nearest_existing(mut p: &Path) -> Option<&Path> {
    loop {
        if p.exists() {
            return Some(p);
        }
        p = p.parent()?;
    }
}

#[cfg(test)]
mod tests {
    use super::safe_join_under;
    use std::path::{Path, PathBuf};

    fn root() -> PathBuf {
        PathBuf::from(r"D:\Games\Example")
    }

    fn ok(rel: &str) {
        let r = safe_join_under(&root(), rel);
        assert!(r.is_ok(), "应允许 {:?}，实际 {:?}", rel, r);
    }

    fn denied(rel: &str) {
        let r = safe_join_under(&root(), rel);
        assert!(r.is_err(), "应拒绝 {:?}，实际 {:?}", rel, r);
    }

    #[test]
    fn allows_normal_relative_paths() {
        ok("data/file.bin");
        ok("data\\file.bin");
        ok("a/b/c/d.bin");
        ok("config.ini");
    }

    #[test]
    fn rejects_parent_traversal() {
        denied("../outside.bin");
        denied("a/../../outside.bin");
        denied("..\\..\\outside.bin");
        denied("data\\..\\..\\outside.bin");
        denied("a/..");
    }

    #[test]
    fn rejects_absolute_paths() {
        denied(r"C:\outside.bin");
        denied(r"D:\gamedir\..\x.bin");
        denied(r"\\server\share\file.bin");
        denied(r"\outside.bin");
        denied("/etc/passwd");
        denied(r"\\?\C:\Windows\System32\drivers\etc\hosts");
    }

    #[test]
    fn rejects_encoded_traversal() {
        denied("..%2f..%2foutside.bin");
        denied("..%5c..%5coutside.bin");
        denied("%2e%2e/%2e%2e/outside.bin");
    }

    #[test]
    fn rejects_reserved_device_names() {
        denied("CON");
        denied("nul");
        denied("NUL.txt");
        denied("a/COM1");
        denied("dir/LPT9.bin");
    }

    #[test]
    fn rejects_ntfs_streams_and_trailing_dots() {
        denied("file.txt:hidden");
        denied("a/evil.");
        denied("a/evil ");
    }

    #[test]
    fn rejects_empty_and_nul() {
        denied("");
        denied("   ");
        assert!(safe_join_under(&root(), "a/\u{0}/b").is_err());
    }

    #[test]
    fn zip_entry_bypass_from_l02_is_closed() {
        // 旧解压器用的是这个检查，它放过了绝对路径
        let old_check = |n: &str| n.contains("..") || n.starts_with('/') || n.starts_with('\\');
        for name in [r"C:\evil.exe", r"a:\evil.exe", r"C:/evil.exe"] {
            assert!(!old_check(name), "前提：旧检查确实放过了 {}", name);
            assert!(safe_join_under(Path::new(r"D:\games\x"), name).is_err(),
                "新校验必须拒绝 {}", name);
        }
    }

    #[test]
    fn allows_long_paths() {
        let long = format!("{}/deep.bin", "d".repeat(300));
        ok(&long);
    }

    #[test]
    fn literal_prefix_check_is_not_enough() {
        // D:\Games\Example\..\escaped.bin 的字面前缀成立，但必须被拒绝
        let joined = root().join("../escaped.bin");
        assert!(joined.starts_with(root()), "前提：字面前缀检查确实会通过");
        assert!(safe_join_under(&root(), "../escaped.bin").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn rejects_symlink_escaping_root() {
        use std::os::windows::fs::symlink_dir;
        // 放在 target/ 下：确定可写，且不污染系统临时目录
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("ql_pathsafe_{}", std::process::id()));
        let root = base.join("root");
        let outside = base.join("outside");
        let _ = std::fs::remove_dir_all(&base);
        if std::fs::create_dir_all(&root).is_err() || std::fs::create_dir_all(&outside).is_err() {
            // 环境不允许建目录：跳过（逻辑已由其余用例覆盖）
            let _ = std::fs::remove_dir_all(&base);
            return;
        }
        let link = root.join("link");
        if symlink_dir(&outside, &link).is_err() {
            // 没有创建符号链接的权限（未开开发者模式）：跳过
            let _ = std::fs::remove_dir_all(&base);
            return;
        }
        let r = safe_join_under(&root, "link/x.bin");
        assert!(r.is_err(), "经由符号链接的路径应被拒绝，实际 {:?}", r);
        let _ = std::fs::remove_dir_all(&base);
    }
}
