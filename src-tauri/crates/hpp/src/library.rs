//! Host-owned library, independent of plugin storage and installation lifetime.
use crate::{
    model,
    store::{atomic_write, now},
    PluginError, Result,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Mutex,
};

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Data {
    entries: BTreeMap<String, Value>,
    progress: BTreeMap<String, Value>,
    #[serde(default)]
    associations: BTreeMap<String, BTreeSet<String>>,
}
pub struct Library {
    path: PathBuf,
    data: Mutex<Data>,
}
fn key(parts: &[&str]) -> String {
    serde_json::to_string(parts).unwrap()
}
fn ident_max<'a>(v: &'a Value, k: &str, max: usize) -> Result<&'a str> {
    v[k].as_str()
        .filter(|s| !s.is_empty() && s.len() <= max)
        .ok_or_else(|| PluginError::invalid(format!("{k} 标识无效")))
}
fn ident<'a>(v: &'a Value, k: &str) -> Result<&'a str> {
    ident_max(v, k, 256)
}
fn flag(v: &Value, k: &str) -> Result<Option<bool>> {
    v.get(k)
        .map(|v| {
            v.as_bool()
                .ok_or_else(|| PluginError::invalid(format!("{k} 必须为布尔值")))
        })
        .transpose()
}
fn progress_key(p: &Value) -> Result<String> {
    let playlist = if p.get("playlistId").is_some() {
        Some(ident(p, "playlistId")?)
    } else {
        None
    };
    // 分集 id 走 EPISODE_ID_MAX：网盘分集 id 是分享链接（见 model.rs），
    // 256 不够用——2026-11-26 齐天圣瞳在 v0.0.101 上点播报
    // “episodeId 标识无效”，就是这一步把 565 字节的 id 挡掉了。
    Ok(serde_json::to_string(&json!([
        ident(p, "pluginId")?,
        ident(p, "mediaId")?,
        playlist,
        ident_max(p, "episodeId", model::EPISODE_ID_MAX)?
    ]))
    .unwrap())
}
impl Library {
    pub fn open(path: PathBuf) -> Result<Self> {
        let data = if path.exists() {
            serde_json::from_slice(&std::fs::read(&path)?)
                .map_err(|_| PluginError::internal("本地片库损坏，原文件已保留"))?
        } else {
            Data::default()
        };
        Ok(Self {
            path,
            data: Mutex::new(data),
        })
    }
    pub fn operation(&self, action: &str, p: &Value) -> Result<Value> {
        let mut lock = self
            .data
            .lock()
            .map_err(|_| PluginError::internal("片库锁不可用"))?;
        let mut data = lock.clone();
        let out = match action {
            "list" => json!(data.entries.values().collect::<Vec<_>>()),
            "get" => data
                .entries
                .get(&key(&[ident(p, "pluginId")?, ident(p, "mediaId")?]))
                .cloned()
                .unwrap_or(Value::Null),
            "sources.get" => {
                let k = key(&[ident(p, "pluginId")?, ident(p, "mediaId")?]);
                json!(data
                    .associations
                    .get(&k)
                    .into_iter()
                    .flatten()
                    .filter(|other| **other != k)
                    .filter_map(|other| data.entries.get(other))
                    .collect::<Vec<_>>())
            }
            "sources.link" => {
                // Validate the entire pair before changing either entry or its associations.
                let pair = ["from", "to"].map(|side| -> Result<(String, &str, Value)> {
                    let plugin = ident(&p[side], "pluginId")?;
                    let item = model::media_item(p[side]["item"].clone(), true)
                        .map_err(|_| PluginError::invalid(format!("{side} 片库条目无效")))?;
                    let k = key(&[plugin, ident(&item, "id")?]);
                    Ok((k, plugin, item))
                });
                let [from, to] = pair;
                let from = from?;
                let to = to?;
                if from.0 == to.0 {
                    return Err(PluginError::invalid("不能关联同一个来源条目"));
                }
                let mut group = BTreeSet::from([from.0.clone(), to.0.clone()]);
                for k in [&from.0, &to.0] {
                    if let Some(existing) = data.associations.get(k) {
                        group.extend(existing.iter().cloned());
                    }
                }
                for (k, plugin, item) in [from, to] {
                    let entry = data.entries.entry(k).or_insert_with(|| json!({
                        "pluginId":plugin,"mediaId":item["id"],"favorite":false,"tracking":false,"updatedAt":now()
                    }));
                    entry["item"] = item;
                }
                for k in &group {
                    data.associations.insert(k.clone(), group.clone());
                }
                Value::Null
            }
            "put" => {
                let plugin = ident(p, "pluginId")?;
                let item = model::media_item(p["item"].clone(), true)
                    .map_err(|_| PluginError::invalid("片库条目无效"))?;
                let id = ident(&item, "id")?;
                let k = key(&[plugin, id]);
                let mut entry=data.entries.get(&k).cloned().unwrap_or_else(||json!({"pluginId":plugin,"mediaId":id,"favorite":false,"tracking":false,"updatedAt":now()}));
                entry["item"] = item;
                entry["updatedAt"] = json!(now());
                for f in ["favorite", "tracking"] {
                    if let Some(value) = flag(p, f)? {
                        entry[f] = json!(value);
                    }
                }
                if let Some(v) = p.get("latestLabel") {
                    entry["latestLabel"] = json!(v
                        .as_str()
                        .ok_or_else(|| PluginError::invalid("latestLabel 必须为文本"))?
                        .chars()
                        .take(200)
                        .collect::<String>());
                }
                data.entries.insert(k, entry.clone());
                entry
            }
            "progress.set" => {
                let plugin = ident(p, "pluginId")?;
                let media = ident(p, "mediaId")?;
                let playlist = if p.get("playlistId").is_some() {
                    Some(ident(p, "playlistId")?)
                } else {
                    None
                };
                let episode = ident_max(p, "episodeId", model::EPISODE_ID_MAX)?;
                for field in ["positionSec", "durationSec"] {
                    if (field == "positionSec" || p.get(field).is_some())
                        && !p[field].as_f64().is_some_and(|n| n.is_finite() && n >= 0.0)
                    {
                        return Err(PluginError::invalid(format!("{field} 必须为非负秒数")));
                    }
                }
                let k = key(&[plugin, media]);
                let mut progress = json!({"pluginId":plugin,"mediaId":media,"episodeId":episode,"positionSec":p["positionSec"],"at":now()});
                if let Some(playlist) = playlist {
                    progress["playlistId"] = json!(playlist);
                }
                if let Some(v) = p.get("durationSec") {
                    progress["durationSec"] = v.clone();
                }
                if let Some(e) = data.entries.get_mut(&k) {
                    e["lastProgress"] = progress.clone();
                    e["updatedAt"] = json!(now());
                } else {
                    return Err(PluginError::invalid("请先保存片库条目"));
                }
                data.progress.insert(progress_key(p)?, progress.clone());
                progress
            }
            "progress.get" => data
                .progress
                .get(&progress_key(p)?)
                .cloned()
                .unwrap_or(Value::Null),
            "history.clear" => {
                data.progress.clear();
                for e in data.entries.values_mut() {
                    e.as_object_mut().unwrap().remove("lastProgress");
                }
                Value::Null
            }
            _ => return Err(PluginError::new("UNSUPPORTED", "未知片库操作")),
        };
        if !matches!(action, "list" | "get" | "progress.get" | "sources.get") {
            atomic_write(&self.path, &serde_json::to_vec(&data).unwrap())?;
            *lock = data;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp_lib(name: &str) -> (Library, PathBuf) {
        let path = std::env::temp_dir()
            .join(format!("hghub-library-test-{name}-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        (Library::open(path.clone()).unwrap(), path)
    }

    fn put_entry(lib: &Library) {
        lib.operation(
            "put",
            &json!({"pluginId": "p", "item": {"id": "m1", "title": "t"}}),
        )
        .unwrap();
    }

    #[test]
    fn progress_accepts_long_episode_id_up_to_2048() {
        // 2026-11-26 齐天圣瞳：网盘分集 id 565 字节，播放前调
        // library.progress.get 曾被 256 上限挡掉、报“episodeId 标识无效”。
        let (lib, path) = tmp_lib("long-episode");
        put_entry(&lib);
        let long_id = "x".repeat(600);
        lib.operation(
            "progress.set",
            &json!({"pluginId": "p", "mediaId": "m1", "episodeId": long_id, "positionSec": 0}),
        )
        .unwrap();
        let got = lib
            .operation(
                "progress.get",
                &json!({"pluginId": "p", "mediaId": "m1", "episodeId": long_id}),
            )
            .unwrap();
        assert_eq!(got["episodeId"], json!(long_id));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn progress_rejects_episode_id_over_2048() {
        let (lib, path) = tmp_lib("too-long-episode");
        put_entry(&lib);
        let err = lib
            .operation(
                "progress.set",
                &json!({"pluginId": "p", "mediaId": "m1", "episodeId": "x".repeat(2049), "positionSec": 0}),
            )
            .unwrap_err();
        assert!(err.message.contains("episodeId 标识无效"));
        let _ = std::fs::remove_file(&path);
    }
}
