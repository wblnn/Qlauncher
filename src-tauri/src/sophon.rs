// src-tauri/src/sophon.rs (同步阻塞版)
use prost::Message;
use std::path::Path;

#[derive(Clone, PartialEq, Message)]
pub struct SophonManifest {
    #[prost(message, repeated, tag = "1")]
    pub files: Vec<GameFile>,
}

#[derive(Clone, PartialEq, Message)]
pub struct GameFile {
    #[prost(string, tag = "1")]
    pub file: String,
    #[prost(message, repeated, tag = "2")]
    pub chunks: Vec<Chunk>,
    #[prost(bool, tag = "3")]
    pub is_folder: bool,
    #[prost(int64, tag = "4")]
    pub size: i64,
    #[prost(string, tag = "5")]
    pub md5: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct Chunk {
    #[prost(string, tag = "1")]
    pub url_suffix: String,
    #[prost(string, tag = "2")]
    pub md5: String,
    #[prost(int64, tag = "3")]
    pub offset: i64,
    #[prost(int64, tag = "4")]
    pub compressed_size: i64,
    #[prost(int64, tag = "5")]
    pub size: i64,
    #[prost(uint64, tag = "6")]
    pub unknown: u64,
}

pub struct ManifestBundle {
    pub tag: String,
    pub build_id: String,
    pub chunk_prefix: String,
    pub manifest: SophonManifest,
}

pub fn fetch_manifest(client: &reqwest::blocking::Client, manifest_url: &str) -> Result<SophonManifest, String> {
    println!("[sophon] 下载 Manifest: {}", manifest_url);
    let resp = client.get(manifest_url).send().map_err(|e| format!("请求 manifest 失败: {}", e))?;
    if !resp.status().is_success() {
        return Err(format!("下载 manifest 失败, 状态码: {}", resp.status()));
    }
    let compressed = resp.bytes().map_err(|e| e.to_string())?;
    println!("[sophon] 压缩大小: {:.2} MB", compressed.len() as f64 / 1048576.0);
    let decompressed = zstd::decode_all(&compressed[..]).map_err(|e| format!("Zstd 解压失败: {}", e))?;
    println!("[sophon] 解压大小: {:.2} MB", decompressed.len() as f64 / 1048576.0);
    SophonManifest::decode(&decompressed[..]).map_err(|e| format!("Protobuf 解析失败: {}", e))
}

pub fn fetch_game_manifest(
    client: &reqwest::blocking::Client,
    biz: &str,
    category_kw: &str,
) -> Result<ManifestBundle, String> {
    let branches_url = "https://hyp-api.mihoyo.com/hyp/hyp-connect/api/getGameBranches?launcher_id=jGHBHlcOq1";
    let resp = client.get(branches_url).send().map_err(|e| e.to_string())?;
    let v: serde_json::Value = resp.json().map_err(|e| e.to_string())?;

    let mut cur_biz = None;
    let mut entries = Vec::new();
    collect_entries(&v, &mut cur_biz, "", &mut entries);

    let chosen = entries
        .iter()
        .find(|(b, br, _)| b == biz && br == "main")
        .or_else(|| entries.iter().find(|(b, _, _)| b == biz))
        .ok_or_else(|| format!("biz={} 挖不到 package_id", biz))?;

    let build_url = format!(
        "https://api-takumi.mihoyo.com/downloader/sophon_chunk/api/getBuild?branch={}&{}&plat_app=cxgf44wie1a8",
        chosen.1, chosen.2
    );
    println!("[sophon] getBuild: {}", build_url);

    let resp = client.get(&build_url).send().map_err(|e| e.to_string())?;
    let v: serde_json::Value = resp.json().map_err(|e| e.to_string())?;
    let data = &v["data"];
    let tag = data["tag"].as_str().unwrap_or("?").to_string();
    let build_id = data["build_id"].as_str().unwrap_or("?").to_string();

    let manifests = data["manifests"]
        .as_array()
        .filter(|a| !a.is_empty())
        .ok_or_else(|| "getBuild 缺 manifests 数组".to_string())?;
    let entry = manifests
        .iter()
        .find(|m| m["category_name"].as_str().map(|n| n.contains(category_kw)).unwrap_or(false))
        .or_else(|| manifests.first())
        .ok_or_else(|| "无匹配分类".to_string())?;

    let prefix = entry["manifest_download"]["url_prefix"].as_str().unwrap_or("");
    let id = entry["manifest"]["id"].as_str().unwrap_or("");
    let chunk_prefix = entry["chunk_download"]["url_prefix"].as_str().unwrap_or("").to_string();
    let url = if prefix.ends_with('/') { format!("{}{}", prefix, id) } else { format!("{}/{}", prefix, id) };

    let manifest = fetch_manifest(client, &url)?;
    Ok(ManifestBundle { tag, build_id, chunk_prefix, manifest })
}

fn collect_entries(v: &serde_json::Value, biz: &mut Option<String>, hint: &str, out: &mut Vec<(String, String, String)>) {
    match v {
        serde_json::Value::Object(map) => {
            if let Some(b) = map.get("biz").and_then(|x| x.as_str()) { *biz = Some(b.to_string()); }
            if let (Some(pid), Some(pw)) = (map.get("package_id").and_then(|x| x.as_str()), map.get("password").and_then(|x| x.as_str())) {
                let branch = map.get("branch").and_then(|x| x.as_str()).map(|s| s.to_string())
                    .unwrap_or_else(|| if hint.is_empty() { "main".into() } else { hint.into() });
                out.push((biz.clone().unwrap_or_default(), branch, format!("package_id={}&password={}", pid, pw)));
            }
            for (key, val) in map { collect_entries(val, biz, key, out); }
        }
        serde_json::Value::Array(arr) => { for item in arr { collect_entries(item, biz, hint, out); } }
        _ => {}
    }
}

pub struct ApplyProgress {
    pub chunks_done: usize,
    pub chunks_total: usize,
    pub bytes_downloaded: u64,
    pub current_file: String,
}

pub fn apply_update<F>(
    client: &reqwest::blocking::Client,
    manifest: &SophonManifest,
    chunk_prefix: &str,
    game_dir: &Path,
    staging_dir: &Path,
    mut on_progress: F,
) -> Result<String, String>
where F: FnMut(&ApplyProgress) {
    use std::io::{Read, Seek, SeekFrom, Write};
    use sysinfo::Disks;

    // 空间预检
    let disks = Disks::new_with_refreshed_list();
    let abs = std::fs::canonicalize(game_dir).unwrap_or_else(|_| game_dir.to_path_buf());
    let free = disks.list().iter().find(|d| abs.starts_with(d.mount_point()))
        .map(|d| d.available_space() / 1048576).unwrap_or(0);
    let staging_need: u64 = manifest.files.iter().map(|f| f.size as u64).sum();
    let need_mb = staging_need / 1048576 + 5120;
    if free < need_mb {
        return Err(format!("空间不足: 剩余 {:.1} GB, 需 {:.1} GB", free as f64 / 1024.0, need_mb as f64 / 1024.0));
    }

    let chunks_total: usize = manifest.files.iter().map(|f| f.chunks.len()).sum();
    let mut p = ApplyProgress { chunks_done: 0, chunks_total, bytes_downloaded: 0, current_file: String::new() };

    for f in &manifest.files {
        if f.is_folder { continue; }
        p.current_file = f.file.clone();
        let staging_path = staging_dir.join(&f.file);
        if let Some(par) = staging_path.parent() { std::fs::create_dir_all(par).map_err(|e| e.to_string())?; }

        let mut chunks = f.chunks.clone();
        chunks.sort_by_key(|c| c.offset);

        let mut file = std::fs::OpenOptions::new().create(true).read(true).write(true).open(&staging_path)
            .map_err(|e| format!("开 staging 失败: {}", e))?;

        for c in &chunks {
            // 断点续传检查
            let need = (c.offset + c.size) as u64;
            let mut ok = file.metadata().map(|m| m.len() >= need).unwrap_or(false);
            if ok {
                let mut buf = vec![0u8; c.size as usize];
                file.seek(SeekFrom::Start(c.offset as u64)).map_err(|e| e.to_string())?;
                if file.read_exact(&mut buf).is_ok() {
                    ok = format!("{:x}", md5::compute(&buf)) == c.md5.trim().to_lowercase();
                } else { ok = false; }
            }
            if ok { p.chunks_done += 1; on_progress(&p); continue; }

            let u1 = format!("{}{}", chunk_prefix, c.url_suffix);
            let u2 = format!("{}/{}", chunk_prefix, c.url_suffix);
            let mut raw = Vec::new();
            for u in [&u1, &u2] {
                if let Ok(r) = client.get(u).send() {
                    if r.status().is_success() {
                        raw = r.bytes().map(|b| b.to_vec()).map_err(|e| e.to_string())?;
                        break;
                    }
                }
            }
            if raw.is_empty() { return Err(format!("chunk 下载失败: {}", f.file)); }

            let plain = zstd::decode_all(&raw[..]).map_err(|e| format!("chunk zstd 失败: {}", e))?;
            file.seek(SeekFrom::Start(c.offset as u64)).map_err(|e| e.to_string())?;
            file.write_all(&plain).map_err(|e| e.to_string())?;
            p.chunks_done += 1;
            p.bytes_downloaded += raw.len() as u64;
            on_progress(&p);
        }
        file.flush().map_err(|e| e.to_string())?;
        drop(file);

        // 整文件 MD5 校验
        let mut ctx = md5::Context::new();
        let mut buf = vec![0u8; 1048576];
        let mut rf = std::fs::File::open(&staging_path).map_err(|e| e.to_string())?;
        loop {
            let n = rf.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 { break; }
            ctx.consume(&buf[..n]);
        }
        if format!("{:x}", ctx.compute()) != f.md5.trim().to_lowercase() {
            return Err(format!("staging 整文件 md5 不符: {}", f.file));
        }

        // 原子替换
        let target = game_dir.join(&f.file);
        if let Some(par) = target.parent() { std::fs::create_dir_all(par).map_err(|e| e.to_string())?; }
        if target.exists() {
            let mut old = target.clone();
            old.set_file_name(format!("{}.qlold", old.file_name().unwrap().to_string_lossy()));
            let _ = std::fs::remove_file(&old);
            std::fs::rename(&target, &old).map_err(|e| format!("移旧失败: {}", e))?;
            std::fs::rename(&staging_path, &target).map_err(|e| format!("替换失败: {}", e))?;
            let _ = std::fs::remove_file(&old);
        } else {
            std::fs::rename(&staging_path, &target).map_err(|e| e.to_string())?;
        }
    }
    Ok(format!("完成: 更新 {} 个文件", manifest.files.len()))
}

pub fn bump_config_version(game_dir: &Path, ver: &str) -> Result<(), String> {
    let p = game_dir.join("config.ini");
    if !p.exists() { return Ok(()); }
    let txt = std::fs::read_to_string(&p).map_err(|e| e.to_string())?;
    let out: Vec<String> = txt.lines().map(|l| if l.trim_start().starts_with("version=") { format!("version={}", ver) } else { l.to_string() }).collect();
    std::fs::write(&p, out.join("\r\n")).map_err(|e| e.to_string())?;
    Ok(())
}

/// 取指定 biz 的最新版本 tag（不下载 manifest），供版本对账使用
pub fn fetch_latest_tag(biz: &str) -> Result<String, String> {
    let client = reqwest::blocking::Client::new();
    let branches_url =
        "https://hyp-api.mihoyo.com/hyp/hyp-connect/api/getGameBranches?launcher_id=jGHBHlcOq1";
    let resp = client.get(branches_url).send().map_err(|e| e.to_string())?;
    let v: serde_json::Value = resp.json().map_err(|e| e.to_string())?;
    let mut cur_biz = None;
    let mut entries = Vec::new();
    collect_entries(&v, &mut cur_biz, "", &mut entries);
    let chosen = entries
        .iter()
        .find(|(b, br, _)| b == biz && br == "main")
        .or_else(|| entries.iter().find(|(b, _, _)| b == biz))
        .ok_or_else(|| format!("biz={} 挖不到 package_id", biz))?;
    let build_url = format!(
        "https://api-takumi.mihoyo.com/downloader/sophon_chunk/api/getBuild?branch={}&{}&plat_app=cxgf44wie1a8",
        chosen.1, chosen.2
    );
    let resp = client.get(&build_url).send().map_err(|e| e.to_string())?;
    let v: serde_json::Value = resp.json().map_err(|e| e.to_string())?;
    v["data"]["tag"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "getBuild 无 tag".to_string())
}