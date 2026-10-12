// src/trust.rs
//! QL-05：外置可执行文件与配置来源的信任边界。
//!
//! 原实现把 channels.json 里的 hpatchz_path 原样交给 Command::new，只做一个 is_file() 检查；
//! channels.json 在生产环境位于 app_data_dir —— 普通用户可写；kuro 的 api 覆盖同理。
//! 于是"改一个配置文件"就等于"让启动器执行任意程序 / 把整条更新链指向恶意服务器"。
//!
//! 这里做三件事：
//!   ① 可用位置收敛到可信根（本程序所在目录；开发期额外允许源码目录）；
//!   ② 文件名必须精确等于约定名；
//!   ③ 对最终选中的二进制做 SHA-256 固定校验。
//!
//! 这是纵深防御，不是密码学保证：同用户权限的进程本来就能替换启动器自身。
//! 它的目标是消灭"改配置即可执行任意路径"这条最容易达成的链路。

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// 随包分发的 hpatchz.exe（HDiffPatch hpatchz，515072 字节）的 SHA-256。
/// 换用别的 HDiffPatch 版本时，必须同步改这里并重新编译。
pub const HPATCHZ_SHA256: &str = "9703c694b5955c576d9f0e26e98b60941f0bbb53b382b1f1988d75d461e580cb";

/// 计算文件 SHA-256（十六进制小写）
pub fn file_sha256(path: &Path) -> Result<String, String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).map_err(|e| format!("打开失败 {}: {}", path.display(), e))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 { break; }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// 允许放外置二进制的目录：本程序所在目录；开发期额外允许源码目录。
///
/// 故意**不包含** app_data_dir —— 那是普通用户可写的配置目录，
/// 把它当可信根等于没修。
pub fn trusted_exe_roots(app: &tauri::AppHandle) -> Vec<PathBuf> {
    let _ = app;
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() { roots.push(dir.to_path_buf()); }
    }
    #[cfg(debug_assertions)]
    roots.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    dedup(roots)
}

/// 不依赖 AppHandle 的版本（供并发/测试路径使用）
pub fn trusted_exe_roots_plain() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() { roots.push(dir.to_path_buf()); }
    }
    #[cfg(debug_assertions)]
    roots.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    dedup(roots)
}

fn dedup(roots: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for r in roots {
        let r = normalize(&r);
        if !out.iter().any(|x| *x == r) { out.push(r); }
    }
    out
}

fn normalize(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

fn is_under(path: &Path, root: &Path) -> bool {
    normalize(path).starts_with(normalize(root))
}

/// 解析并校验一个外置可执行文件。
///
/// - configured 非空：必须存在、必须是普通文件、文件名必须精确等于 file_name、
///   且必须位于 roots 之一内部；任何一条不满足都直接拒绝（不再"找不到就换下一个"）。
/// - configured 为空：按 roots 顺序查找 roots/{file_name}。
/// - expected_hash 非空时：对最终选中的文件做 SHA-256 校验，不符即拒绝。
pub fn resolve_executable(
    roots: &[PathBuf],
    configured: Option<&str>,
    file_name: &str,
    expected_hash: &str,
) -> Result<PathBuf, String> {
    let chosen: PathBuf = if let Some(c) = configured.map(str::trim).filter(|s| !s.is_empty()) {
        let p = PathBuf::from(c);
        if !p.is_file() {
            return Err(format!("配置里指定的 {} 不是普通文件: {}", file_name, p.display()));
        }
        let name_ok = p.file_name().map(|n| n.eq_ignore_ascii_case(file_name)).unwrap_or(false);
        if !name_ok {
            return Err(format!("拒绝执行 {}：文件名必须精确是 {}", p.display(), file_name));
        }
        if !roots.iter().any(|r| is_under(&p, r)) {
            return Err(format!(
                "拒绝执行 {}：它不在可信目录内（只允许随程序分发的 {}）。可信目录: {}",
                p.display(),
                file_name,
                roots.iter().map(|r| r.display().to_string()).collect::<Vec<_>>().join(" / ")
            ));
        }
        p
    } else {
        let mut found = None;
        for r in roots {
            let cand = r.join(file_name);
            if cand.is_file() { found = Some(cand); break; }
        }
        found.ok_or_else(|| format!("未找到 {}：请把它放在程序同目录", file_name))?
    };

    if !expected_hash.trim().is_empty() {
        let got = file_sha256(&chosen)?;
        if !got.eq_ignore_ascii_case(expected_hash.trim()) {
            return Err(format!(
                "{} 完整性校验失败（SHA-256 不符）\n  路径: {}\n  期望: {}\n  实际: {}\n→ 确实换用了其它 HDiffPatch 版本时，请更新 src/trust.rs 里的 HPATCHZ_SHA256 后重新编译。",
                file_name, chosen.display(), expected_hash.trim(), got
            ));
        }
    }
    Ok(chosen)
}

/// 只接受 https:// 且主机名等于 allowed 中某个域名、或其子域的 URL。
/// 用于把"可被配置覆盖的服务端地址"限制在官方域内。
pub fn is_trusted_https_host(url: &str, allowed: &[&str]) -> bool {
    // 整体小写化：scheme 与主机名都大小写无关
    let lower = url.trim().to_ascii_lowercase();
    let Some(rest) = lower.strip_prefix("https://") else { return false; };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() { return false; }
    // 去掉 userinfo（evil.com@kurogame.com 的真实主机是 kurogame.com）
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    let host = host_port.split(':').next().unwrap_or(host_port);
    if host.is_empty() { return false; }
    allowed.iter().any(|d| {
        let d = d.to_ascii_lowercase();
        host == d || host.ends_with(&format!(".{}", d))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox(name: &str) -> PathBuf {
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("ql_trust_{}_{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn accepts_configured_path_inside_root() {
        let base = sandbox("inside");
        let root = base.join("app");
        std::fs::create_dir_all(&root).unwrap();
        let exe = root.join("hpatchz.exe");
        std::fs::write(&exe, b"payload").unwrap();
        let r = resolve_executable(&[root.clone()], Some(exe.to_str().unwrap()), "hpatchz.exe", "");
        assert!(r.is_ok(), "可信目录内的配置路径应放行: {:?}", r);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn rejects_configured_path_outside_root() {
        let base = sandbox("outside");
        let root = base.join("app");
        let evil = base.join("evil");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&evil).unwrap();
        let fake = evil.join("hpatchz.exe");
        std::fs::write(&fake, b"payload").unwrap();
        let r = resolve_executable(&[root], Some(fake.to_str().unwrap()), "hpatchz.exe", "");
        assert!(r.is_err(), "可信目录外的配置路径必须拒绝");
        assert!(r.unwrap_err().contains("不在可信目录内"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn rejects_wrong_file_name_even_inside_root() {
        let base = sandbox("name");
        let root = base.join("app");
        std::fs::create_dir_all(&root).unwrap();
        let other = root.join("cmd.exe");
        std::fs::write(&other, b"payload").unwrap();
        let r = resolve_executable(&[root], Some(other.to_str().unwrap()), "hpatchz.exe", "");
        assert!(r.is_err(), "文件名不符必须拒绝");
        assert!(r.unwrap_err().contains("文件名必须精确是"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn rejects_missing_configured_file() {
        let base = sandbox("missing");
        let root = base.join("app");
        std::fs::create_dir_all(&root).unwrap();
        let ghost = root.join("hpatchz.exe");
        let r = resolve_executable(&[root], Some(ghost.to_str().unwrap()), "hpatchz.exe", "");
        assert!(r.is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn searches_roots_when_not_configured() {
        let base = sandbox("search");
        let root = base.join("app");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("hpatchz.exe"), b"payload").unwrap();
        let r = resolve_executable(&[root], None, "hpatchz.exe", "");
        assert!(r.is_ok(), "应在可信根里搜到: {:?}", r);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn enforces_sha256_pin() {
        let base = sandbox("hash");
        let root = base.join("app");
        std::fs::create_dir_all(&root).unwrap();
        let exe = root.join("hpatchz.exe");
        std::fs::write(&exe, b"payload").unwrap();
        let good = file_sha256(&exe).unwrap();

        let ok = resolve_executable(&[root.clone()], None, "hpatchz.exe", &good);
        assert!(ok.is_ok(), "哈希相符应放行: {:?}", ok);

        let bad = resolve_executable(&[root], None, "hpatchz.exe", &"0".repeat(64));
        assert!(bad.is_err(), "哈希不符必须拒绝");
        assert!(bad.unwrap_err().contains("SHA-256 不符"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn https_host_allowlist() {
        let kuro = ["kurogame.com"];
        assert!(is_trusted_https_host("https://prod-alicdn-gamestarter.kurogame.com/launcher/game", &kuro));
        assert!(is_trusted_https_host("https://kurogame.com", &kuro));
        assert!(is_trusted_https_host("https://sub.kurogame.com:443/x", &kuro));
        assert!(is_trusted_https_host("HTTPS://PROD.KUROGAME.COM/x", &kuro));
        // 空域名列表 = 谁都不信
        assert!(!is_trusted_https_host("https://kurogame.com", &[]));

        assert!(!is_trusted_https_host("http://prod-alicdn-gamestarter.kurogame.com", &kuro));
        assert!(!is_trusted_https_host("https://evil.com", &kuro));
        assert!(!is_trusted_https_host("https://kurogame.com.evil.com", &kuro));
        assert!(!is_trusted_https_host("https://evil.com/kurogame.com", &kuro));
        assert!(!is_trusted_https_host("https://kurogame.com@evil.com/", &kuro));
        assert!(!is_trusted_https_host("ftp://kurogame.com", &kuro));
    }

    #[test]
    fn https_host_is_case_insensitive() {
        assert!(is_trusted_https_host("HTTPS://PROD.KUROGAME.COM/x", &["kurogame.com"]));
    }
}
