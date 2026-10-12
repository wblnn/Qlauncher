// src/digest.rs
//! QL-08：本地二次校验（SHA-256）。
//!
//! 背景：全链路只能拿到官方 MD5，且清单与差分包本身没有签名 ——
//! 所以哈希只能证明"内容与清单一致"，不能证明"内容来自官方"（完整性 ≠ 来源认证）。
//! 真要解决来源认证，只能等官方提供签名，这一点报告已经写明。
//!
//! 本模块做能做且有意义的另一半：给"已通过官方 MD5 校验、已经落在本地"的补丁载荷
//! 建一份 SHA-256 指纹，在真正使用前再核对一次，封住
//! 「下载并校验通过 →（可能换一次会话）→ 真正用于合成」之间被改动的窗口。
//!
//! 局限（必须说清楚）：指纹文件与载荷在同一个用户可写目录里，
//! 它挡不住"知道要删指纹文件"的本地攻击者，只挡得住无意损坏与顺手篡改。
//! 这是纵深防御里的本地一致性检查，不是来源认证。

use std::collections::BTreeMap;
use std::path::Path;

/// 指纹文件名（存放在载荷目录内）
pub const PINS_FILE: &str = ".qlauncher_sha256.json";

#[derive(serde::Serialize, serde::Deserialize)]
pub struct PayloadPins {
    pub version: u32,
    /// 载荷身份：元清单文件（hdiffmap.json）的 SHA-256
    pub meta_sha256: String,
    /// 相对路径（统一 / 分隔）→ SHA-256
    pub files: BTreeMap<String, String>,
}

/// 校验结果
pub enum Check {
    /// 与指纹完全一致
    Verified,
    /// 原来没有指纹（旧版本下载的补丁 / 手工准备的目录）：已现场建立指纹并放行
    Pinned,
    /// 与指纹不一致：返回不一致的相对路径
    Changed(Vec<String>),
}

fn rel_key(base: &Path, p: &Path) -> Option<String> {
    p.strip_prefix(base).ok().map(|r| r.to_string_lossy().replace('\\', "/"))
}

/// 递归收集目录下所有文件的 SHA-256。
/// 跳过指纹文件本身；跳过符号链接（不跟随，免得把目录外的内容读进来）。
pub fn snapshot(dir: &Path) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    collect(dir, dir, &mut out)?;
    Ok(out)
}

fn collect(base: &Path, dir: &Path, out: &mut BTreeMap<String, String>) -> Result<(), String> {
    let rd = std::fs::read_dir(dir).map_err(|e| format!("读取目录失败 {}: {}", dir.display(), e))?;
    for entry in rd.filter_map(|e| e.ok()) {
        let p = entry.path();
        let md = match std::fs::symlink_metadata(&p) { Ok(m) => m, Err(_) => continue };
        if md.file_type().is_symlink() { continue; }
        if md.is_dir() {
            collect(base, &p, out)?;
        } else if md.is_file() {
            let Some(rel) = rel_key(base, &p) else { continue };
            if rel == PINS_FILE { continue; }
            let sha = crate::trust::file_sha256(&p)?;
            out.insert(rel, sha);
        }
    }
    Ok(())
}

pub fn save(dir: &Path, pins: &PayloadPins) -> Result<(), String> {
    let p = dir.join(PINS_FILE);
    let s = serde_json::to_string_pretty(pins).map_err(|e| e.to_string())?;
    std::fs::write(&p, s).map_err(|e| format!("写指纹文件失败 {}: {}", p.display(), e))
}

pub fn load(dir: &Path) -> Option<PayloadPins> {
    let s = std::fs::read_to_string(dir.join(PINS_FILE)).ok()?;
    serde_json::from_str(&s).ok()
}

/// 建立 / 刷新指纹。meta_file 必须存在于 dir 内（载荷的身份）。
pub fn pin(dir: &Path, meta_file: &str) -> Result<PayloadPins, String> {
    let meta = dir.join(meta_file);
    let meta_sha256 = crate::trust::file_sha256(&meta)
        .map_err(|e| format!("读不到载荷元清单 {}: {}", meta.display(), e))?;
    let files = snapshot(dir)?;
    let pins = PayloadPins { version: 1, meta_sha256, files };
    save(dir, &pins)?;
    Ok(pins)
}

/// 在使用载荷之前核对一次。没有指纹时现场建立（不阻塞旧补丁目录 / 手工目录）。
pub fn check(dir: &Path, meta_file: &str) -> Result<Check, String> {
    let Some(pins) = load(dir) else {
        pin(dir, meta_file)?;
        return Ok(Check::Pinned);
    };
    let meta_path = dir.join(meta_file);
    let meta_sha256 = crate::trust::file_sha256(&meta_path)
        .map_err(|e| format!("读不到载荷元清单 {}: {}", meta_path.display(), e))?;
    let now = snapshot(dir)?;

    let mut changed: Vec<String> = Vec::new();
    if meta_sha256 != pins.meta_sha256 { changed.push(meta_file.to_string()); }
    for (k, v) in &pins.files {
        match now.get(k) {
            Some(nv) if nv == v => {}
            Some(_) => changed.push(k.clone()),
            None => changed.push(format!("{}（缺失）", k)),
        }
    }
    for k in now.keys() {
        if !pins.files.contains_key(k) { changed.push(format!("{}（新增）", k)); }
    }
    if changed.is_empty() {
        Ok(Check::Verified)
    } else {
        changed.sort();
        changed.dedup();
        Ok(Check::Changed(changed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn sandbox(name: &str) -> PathBuf {
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("ql_digest_{}_{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn seed(dir: &Path) {
        std::fs::write(dir.join("hdiffmap.json"), br#"{"diff_map":[]}"#).unwrap();
        std::fs::write(dir.join("deletefiles.txt"), b"a/b.txt\n").unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub").join("x.hdiff"), b"PATCHDATA").unwrap();
    }

    #[test]
    fn pins_then_verifies_unchanged_payload() {
        let dir = sandbox("clean");
        seed(&dir);
        let _ = pin(&dir, "hdiffmap.json").unwrap();
        match check(&dir, "hdiffmap.json").unwrap() {
            Check::Verified => {}
            _ => panic!("指纹建立后应立即校验通过"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detects_modified_file() {
        let dir = sandbox("modified");
        seed(&dir);
        let _ = pin(&dir, "hdiffmap.json").unwrap();
        std::fs::write(dir.join("sub").join("x.hdiff"), b"TAMPERED!").unwrap();
        match check(&dir, "hdiffmap.json").unwrap() {
            Check::Changed(list) => assert!(list.iter().any(|s| s.contains("x.hdiff")), "应指出被改的文件: {:?}", list),
            _ => panic!("改动后必须报不一致"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detects_added_and_missing_files() {
        let dir = sandbox("addremove");
        seed(&dir);
        let _ = pin(&dir, "hdiffmap.json").unwrap();
        std::fs::write(dir.join("injected.dll"), b"evil").unwrap();
        std::fs::remove_file(dir.join("deletefiles.txt")).unwrap();
        match check(&dir, "hdiffmap.json").unwrap() {
            Check::Changed(list) => {
                assert!(list.iter().any(|s| s.contains("injected.dll") && s.contains("新增")), "{:?}", list);
                assert!(list.iter().any(|s| s.contains("deletefiles.txt") && s.contains("缺失")), "{:?}", list);
            }
            _ => panic!("新增/缺失都必须报出来"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detects_changed_meta_manifest() {
        let dir = sandbox("meta");
        seed(&dir);
        let _ = pin(&dir, "hdiffmap.json").unwrap();
        std::fs::write(dir.join("hdiffmap.json"), br#"{"diff_map":[{"x":1}]}"#).unwrap();
        match check(&dir, "hdiffmap.json").unwrap() {
            Check::Changed(list) => assert!(list.iter().any(|s| s == "hdiffmap.json"), "{:?}", list),
            _ => panic!("元清单被改必须报出来"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pins_on_first_sight_without_blocking() {
        let dir = sandbox("firstseen");
        seed(&dir);
        match check(&dir, "hdiffmap.json").unwrap() {
            Check::Pinned => {}
            _ => panic!("首次应建立指纹并放行"),
        }
        assert!(load(&dir).is_some(), "首次应写出指纹文件");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
