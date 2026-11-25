//! 猫源订阅记录与导入的纯逻辑（不依赖 `PluginManager` 的私有字段）。
//!
//! 记录存在 App 数据目录的 `cat-sources.json`（与 `node-sidecar.json`
//! 同一目录层级）；一个订阅 = 一个插件，id 稳定、同一订阅重导入覆盖
//! 同一条。
use crate::{cat::read_capped_body, PluginError, Result};
use serde::{Deserialize, Serialize};
use std::{
    io::{Cursor, Read},
    path::{Path, PathBuf},
    time::Duration,
};

/// 一条猫源订阅记录（字段见设计 §4.2）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatSourceRecord {
    pub id: String,
    pub name: String,
    /// `"url"`（网络导入）或 `"zip"`（ZIP 导入）。
    pub source_type: String,
    /// 订阅 URL（仅网络导入时有）。
    pub url: Option<String>,
    /// 下载到的 JS 的 md5（网络导入时是校验值；ZIP 导入时是存档时算出的值）。
    pub md5: String,
    /// 导入时的初始启用值；运行时的启用状态以插件条目为准。
    pub enabled: bool,
    /// 上次成功拉取/导入的时间（epoch 毫秒）。
    pub updated_at: i64,
}

/// `cat-sources.json` 的读写（文件不存在时为空记录集）。
#[derive(Debug)]
pub struct CatStore {
    path: PathBuf,
    records: Vec<CatSourceRecord>,
}

impl CatStore {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self {
                path: path.to_path_buf(),
                records: vec![],
            });
        }
        let bytes = std::fs::read(path)?;
        let records: Vec<CatSourceRecord> = serde_json::from_slice(&bytes)
            .map_err(|_| PluginError::internal("猫源订阅记录损坏"))?;
        Ok(Self {
            path: path.to_path_buf(),
            records,
        })
    }

    /// 原子写（tmp + rename，与 `manager.rs` 的 `atomic_write` 同款）。
    pub fn save(&self) -> Result<()> {
        crate::store::atomic_write(
            &self.path,
            &serde_json::to_vec_pretty(&self.records)
                .map_err(|_| PluginError::internal("无法保存猫源订阅记录"))?,
        )
    }

    pub fn list(&self) -> Vec<CatSourceRecord> {
        self.records.clone()
    }

    pub fn get(&self, id: &str) -> Option<CatSourceRecord> {
        self.records.iter().find(|r| r.id == id).cloned()
    }

    /// 按 id 覆盖（不存在则追加）。
    pub fn upsert(&mut self, record: CatSourceRecord) {
        if let Some(old) = self.records.iter_mut().find(|r| r.id == record.id) {
            *old = record;
        } else {
            self.records.push(record);
        }
    }

    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.records.len();
        self.records.retain(|r| r.id != id);
        self.records.len() != before
    }
}

/// id 清洗：小写、非 `[a-z0-9._-]` 的字符换成 `-`、连续 `-` 去重、
/// 去首尾 `-`。
fn clean_id_body(raw: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for c in raw.to_ascii_lowercase().chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
            if c == '-' {
                if last_dash {
                    continue;
                }
                last_dash = true;
            } else {
                last_dash = false;
            }
            out.push(c);
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

fn plugin_id(body: &str, what: &str) -> Result<String> {
    let body = clean_id_body(body);
    if body.is_empty() {
        return Err(PluginError::invalid(format!("无法从{what}派生插件 ID")));
    }
    let mut id = format!("cat.{body}");
    if id.len() > 64 {
        id.truncate(64);
        id = id.trim_end_matches('-').to_string();
    }
    Ok(id)
}

/// 网络导入的 id：订阅 URL 的 host + path 生成，总长 ≤64。
/// host 里的 `.` 换成 `-`（例如 `catpaw.douer.me` → `catpaw-douer-me`），
/// path 里的 `.` 保留（`index.js.md5` 的点不换）。
pub fn derive_plugin_id_from_url(url: &str) -> Result<String> {
    let u = url::Url::parse(url).map_err(|_| PluginError::invalid("订阅地址无效"))?;
    if !matches!(u.scheme(), "http" | "https") {
        return Err(PluginError::invalid("订阅地址必须为 http(s)"));
    }
    let host = u.host_str().unwrap_or("").replace('.', "-");
    let raw = format!("{host}{}", u.path());
    plugin_id(&raw, "订阅地址")
}

/// ZIP 导入的 id：ZIP 文件名词干生成，总长 ≤64。
pub fn derive_plugin_id_from_stem(stem: &str) -> Result<String> {
    plugin_id(stem, "ZIP 文件名")
}

pub fn md5_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    md5::Md5::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn is_hex32(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// 拉取一个网络订阅：URL 以 `.md5` 结尾时先 GET 它取已发布校验值
///（30 秒超时，body 去空白后须为 32 位 hex），再 GET 去 `.md5` 后缀的
/// JS 地址（60 秒超时；body 上限 16 MiB）。返回（已发布 md5 或 None，
/// JS 字节）。代理决议跟随用户设置（`proxy::snapshot`），绝不默认直连。
pub async fn fetch_subscription(url: &str) -> Result<(Option<String>, Vec<u8>)> {
    url::Url::parse(url).map_err(|_| PluginError::invalid("订阅地址无效"))?;
    let client = crate::proxy::configure(
        wreq::Client::builder().emulation(wreq_util::Profile::Chrome149),
        &crate::proxy::snapshot(),
    )
    .map_err(|e| PluginError::new("NETWORK", e))?
    .build()
    .map_err(|_| PluginError::new("NETWORK", "无法创建订阅下载客户端"))?;
    let (js_url, published) = if let Some(base) = url.strip_suffix(".md5") {
        let rsp = client
            .get(url)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    PluginError::timeout()
                } else {
                    PluginError::new("NETWORK", format!("订阅下载失败：{e}"))
                }
            })?;
        if !rsp.status().is_success() {
            return Err(PluginError::invalid("订阅地址返回的校验值无效"));
        }
        let bytes = read_capped_body(rsp, 64 * 1024, "订阅校验值过大").await?;
        let md5 = String::from_utf8_lossy(&bytes).trim().to_ascii_lowercase();
        if !is_hex32(&md5) {
            return Err(PluginError::invalid("订阅地址返回的校验值无效"));
        }
        (base.to_string(), Some(md5))
    } else {
        (url.to_string(), None)
    };
    let rsp = client
        .get(&js_url)
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                PluginError::timeout()
            } else {
                PluginError::new("NETWORK", format!("订阅下载失败：{e}"))
            }
        })?;
    if !rsp.status().is_success() {
        return Err(PluginError::new(
            "UPSTREAM_ERROR",
            format!("订阅下载失败：HTTP {}", rsp.status()),
        ));
    }
    let bytes = read_capped_body(rsp, 16 * 1024 * 1024, "订阅文件过大").await?;
    let actual = md5_hex(&bytes);
    if let Some(published) = &published {
        if *published != actual {
            return Err(PluginError::invalid("校验失败，文件可能被篡改或未更新"));
        }
    }
    Ok((published, bytes))
}

/// ZIP 解压结果：`files` 是除 `index.js` / `index.config.js` 外保留的
/// 文件（相对路径以 `index.js` 所在目录为基准，含 `.md5` 文件）。
#[derive(Debug)]
pub struct ZipExtract {
    pub files: Vec<(String, Vec<u8>)>,
    pub index_js: Vec<u8>,
    pub index_js_md5: String,
    pub config: Option<Vec<u8>>,
}

/// 解压一个猫源 ZIP：条目总数 ≤100、解压后总大小 ≤32 MiB、单个文件
/// ≤16 MiB；只认 `enclosed_name()` 安全的路径（不安全条目直接跳过）；
/// 找 `index.js`（先根目录，再仅一层子目录）；有 `.md5` 文件时用它
/// 校验对应文件，无 `.md5` 时算 `index.js` 的 md5 存档。
pub fn extract_zip(bytes: &[u8]) -> Result<ZipExtract> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| PluginError::invalid(format!("ZIP 文件损坏：{e}")))?;
    if archive.len() > 100 {
        return Err(PluginError::invalid("ZIP 文件过大或条目过多"));
    }
    struct Entry {
        name: String,
        data: Vec<u8>,
    }
    let mut entries: Vec<Entry> = vec![];
    let mut total: usize = 0;
    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| PluginError::invalid(format!("ZIP 解压失败：{e}")))?;
        let Some(name) = file
            .enclosed_name()
            .map(|p| p.to_string_lossy().into_owned())
        else {
            continue;
        };
        if file.is_dir() {
            continue;
        }
        // 按实际读到的字节数限流（头里的 size 不可信），防 ZIP 炸弹。
        let mut data = Vec::new();
        (&mut file)
            .take(16 * 1024 * 1024 + 1)
            .read_to_end(&mut data)
            .map_err(|e| PluginError::invalid(format!("ZIP 解压失败：{e}")))?;
        if data.len() > 16 * 1024 * 1024 {
            return Err(PluginError::invalid("ZIP 文件过大或条目过多"));
        }
        total += data.len();
        if total > 32 * 1024 * 1024 {
            return Err(PluginError::invalid("ZIP 文件过大或条目过多"));
        }
        entries.push(Entry { name, data });
    }
    let index_pos = entries
        .iter()
        .position(|e| e.name == "index.js")
        .or_else(|| {
            entries.iter().position(|e| {
                let mut parts = e.name.split('/');
                matches!(
                    (parts.next(), parts.next(), parts.next()),
                    (Some(_), Some("index.js"), None)
                )
            })
        })
        .ok_or_else(|| PluginError::invalid("ZIP 里没有 index.js"))?;
    let index_path = entries[index_pos].name.clone();
    let base_dir = index_path
        .rsplit_once('/')
        .map(|(d, _)| d.to_string());
    // 相对路径：去掉 index.js 所在那一层目录前缀；不在该目录下的文件丢弃。
    let rel_of = |name: &str| -> Option<String> {
        match &base_dir {
            None => Some(name.to_string()),
            Some(d) => name
                .strip_prefix(&format!("{d}/"))
                .map(str::to_string),
        }
    };
    let index_js = entries[index_pos].data.clone();
    let index_js_md5 = md5_hex(&index_js);
    let mut config = None;
    let mut files: Vec<(String, Vec<u8>)> = vec![];
    let mut md5_files: Vec<(String, String)> = vec![];
    for e in &entries {
        let Some(r) = rel_of(&e.name) else {
            continue;
        };
        if r == "index.js" {
            continue;
        }
        if r == "index.config.js" {
            config = Some(e.data.clone());
            continue;
        }
        if r.ends_with(".md5") {
            md5_files.push((r.clone(), String::from_utf8_lossy(&e.data).trim().to_string()));
        }
        files.push((r, e.data.clone()));
    }
    for (name, published) in &md5_files {
        let published = published.to_ascii_lowercase();
        let actual = match name.as_str() {
            "index.js.md5" => Some(index_js_md5.clone()),
            "index.config.js.md5" => config.as_ref().map(|c| md5_hex(c)),
            _ => None,
        };
        if let Some(actual) = actual {
            if !is_hex32(&published) || published != actual {
                return Err(PluginError::invalid(format!(
                    "校验失败：{name} 与 .md5 不符"
                )));
            }
        }
    }
    Ok(ZipExtract {
        files,
        index_js,
        index_js_md5,
        config,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    fn make_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(&mut buf);
        let options = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, data) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(data).unwrap();
        }
        writer.finish().unwrap();
        buf.into_inner()
    }

    #[test]
    fn derive_id_from_url() {
        assert_eq!(
            derive_plugin_id_from_url("https://catpaw.douer.me/index.js.md5").unwrap(),
            "cat.catpaw-douer-me-index.js.md5"
        );
        assert_eq!(
            derive_plugin_id_from_url("https://EXAMPLE.com/A_B/c.JS").unwrap(),
            "cat.example-com-a_b-c.js"
        );
        assert_eq!(
            derive_plugin_id_from_url("https://example.com").unwrap(),
            "cat.example-com"
        );
        assert!(derive_plugin_id_from_url("not a url").is_err());
        assert!(derive_plugin_id_from_url("ftp://example.com/x.js").is_err());
    }

    #[test]
    fn derive_id_from_stem_and_length() {
        assert_eq!(
            derive_plugin_id_from_stem("catpaw").unwrap(),
            "cat.catpaw"
        );
        assert_eq!(
            derive_plugin_id_from_stem("我的 猫源 v2").unwrap(),
            "cat.v2"
        );
        assert!(derive_plugin_id_from_stem("").is_err());
        assert!(derive_plugin_id_from_stem("!!!").is_err());
        let long = "a".repeat(100);
        let id = derive_plugin_id_from_stem(&long).unwrap();
        assert!(id.len() <= 64, "{id}");
        assert!(id.starts_with("cat."));
    }

    #[test]
    fn md5_known_vector() {
        assert_eq!(
            md5_hex(b"abc"),
            "900150983cd24fb0d6963f7d28e17f72"
        );
    }

    #[test]
    fn store_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cat-sources.json");
        let store = CatStore::load(&path).unwrap();
        assert!(store.list().is_empty());
        let mut store = store;
        let record = CatSourceRecord {
            id: "cat.a".into(),
            name: "A".into(),
            source_type: "url".into(),
            url: Some("https://example.invalid/a.js.md5".into()),
            md5: "00".into(),
            enabled: true,
            updated_at: 123,
        };
        store.upsert(record.clone());
        store.save().unwrap();
        let store = CatStore::load(&path).unwrap();
        assert_eq!(store.get("cat.a").unwrap().name, "A");
        assert_eq!(store.list().len(), 1);
        let mut record2 = record.clone();
        record2.name = "A2".into();
        let mut store = store;
        store.upsert(record2);
        assert_eq!(store.list().len(), 1);
        assert_eq!(store.get("cat.a").unwrap().name, "A2");
        assert!(store.remove("cat.a"));
        assert!(!store.remove("cat.a"));
        assert!(store.list().is_empty());
        // 损坏的记录文件报明错。
        std::fs::write(&path, b"{bad json").unwrap();
        let err = CatStore::load(&path).unwrap_err();
        assert_eq!(err.code, "INTERNAL");
        assert!(err.message.contains("猫源订阅记录损坏"), "{}", err.message);
    }

    #[test]
    fn zip_root_layout_ok() {
        let js = b"module.exports = {start(){}, stop(){}};";
        let cfg = b"module.exports = {};";
        let md5 = md5_hex(js);
        let bytes = make_zip(&[
            ("index.js", js.as_slice()),
            ("index.config.js", cfg.as_slice()),
            ("index.js.md5", md5.as_bytes()),
            ("extra.txt", b"hello".as_slice()),
        ]);
        let out = extract_zip(&bytes).unwrap();
        assert_eq!(out.index_js, js);
        assert_eq!(out.index_js_md5, md5);
        assert_eq!(out.config.unwrap(), cfg);
        assert!(out.files.iter().any(|(n, _)| n == "extra.txt"));
        assert!(out.files.iter().any(|(n, _)| n == "index.js.md5"));
    }

    #[test]
    fn zip_subdir_layout_ok() {
        let js = b"module.exports = {};";
        let bytes = make_zip(&[
            ("catpaw/index.js", js.as_slice()),
            ("catpaw/lib/util.js", b"// util".as_slice()),
        ]);
        let out = extract_zip(&bytes).unwrap();
        assert_eq!(out.index_js, js);
        assert_eq!(out.index_js_md5, md5_hex(js));
        assert!(out.config.is_none());
        assert!(out.files.iter().any(|(n, _)| n == "lib/util.js"));
    }

    #[test]
    fn zip_bad_md5_rejected() {
        let js = b"module.exports = {};";
        let bytes = make_zip(&[
            ("index.js", js.as_slice()),
            ("index.js.md5", b"00000000000000000000000000000000".as_slice()),
        ]);
        let err = extract_zip(&bytes).unwrap_err();
        assert!(err.message.contains("校验失败"), "{}", err.message);
    }

    #[test]
    fn zip_missing_index_js_rejected() {
        let bytes = make_zip(&[("other.js", b"".as_slice())]);
        let err = extract_zip(&bytes).unwrap_err();
        assert!(err.message.contains("ZIP 里没有 index.js"), "{}", err.message);
    }

    #[test]
    fn zip_traversal_entry_skipped() {
        let js = b"module.exports = {};";
        let bytes = make_zip(&[
            ("index.js", js.as_slice()),
            ("../evil.js", b"evil".as_slice()),
            ("/abs.js", b"abs".as_slice()),
        ]);
        let out = extract_zip(&bytes).unwrap();
        assert_eq!(out.index_js, js);
        assert!(!out.files.iter().any(|(n, _)| n.contains("evil")));
        assert!(!out.files.iter().any(|(n, _)| n.contains("abs")));
    }
}
