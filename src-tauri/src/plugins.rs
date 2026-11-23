//! Tauri adapter; the development harness shares the exact same dispatcher.
use hghub_hpp::{PluginManager, Result};
use serde_json::Value;
use std::sync::Arc;
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_fs::FsExt;

/// Document-provider access stays in the host adapter, never in a plugin API.
pub struct PickedDocuments(pub tauri::AppHandle);
impl hghub_hpp::manager::DocumentReader for PickedDocuments {
    #[cfg(target_os = "android")]
    fn resolve_relative(&self, base: &str, reference: &str) -> Result<String> {
        use tauri::Manager;
        self.0
            .state::<crate::android_documents::AndroidDocuments>()
            .resolve(base, reference)
    }
    fn read(&self, location: &str, limit: usize) -> Result<Vec<u8>> {
        use std::io::Read;
        let file: tauri_plugin_fs::FilePath = location
            .parse()
            .map_err(|_| hghub_hpp::PluginError::invalid("文件选择器返回了无效地址"))?;
        let mut bytes = Vec::new();
        let mut options = tauri_plugin_fs::OpenOptions::new();
        options.read(true);
        self.0
            .fs()
            .open(file, options)?
            .take((limit + 1) as u64)
            .read_to_end(&mut bytes)?;
        Ok(bytes)
    }
}

#[tauri::command]
pub async fn pick_plugin_directory(app: tauri::AppHandle) -> Result<Value> {
    #[cfg(target_os = "android")]
    {
        use tauri::Manager;
        Ok(serde_json::json!(
            app.state::<crate::android_documents::AndroidDocuments>()
                .pick()
                .await?
        ))
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        Err(hghub_hpp::PluginError::new(
            "UNSUPPORTED",
            "此平台请直接选择本地列表文件",
        ))
    }
}

/// 选择器返回的地址可能是 tmp 中转文件（iOS 上从别处打开的文件先落在
/// tmp/...-Inbox），也可能在选择器关闭后失效；选完立刻拷进应用自己的
/// plugin-imports 目录再返回，后面的预览与安装读的都是这份拷贝。
const STAGE_LIMIT: usize = 2 * 1024 * 1024; // 与 hpp 的 JS_LIMIT 一致
const STAGE_KEEP_SECS: u64 = 7 * 24 * 3600;

#[tauri::command]
pub async fn pick_plugin_files(app: tauri::AppHandle) -> Result<Vec<String>> {
    tauri::async_runtime::spawn_blocking(move || {
        let picked = app
            .dialog()
            .file()
            .add_filter("HPP 插件、列表、HTML 页面或 Python 蜘蛛", &["js", "mjs", "json", "html", "py"])
            .blocking_pick_files()
            .unwrap_or_default();
        if picked.is_empty() {
            return Ok(Vec::new());
        }
        let staging = stage_dir(&app)?;
        prune_staging(&staging);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mut out = Vec::new();
        for (i, file) in picked.into_iter().enumerate() {
            let bytes = read_picked(&app, &file)?;
            let name = staged_name(&file.to_string(), i, stamp);
            let target = staging.join(name);
            std::fs::write(&target, &bytes).map_err(|e| {
                hghub_hpp::PluginError::internal(format!("无法暂存所选文件：{e}"))
            })?;
            out.push(target.to_string_lossy().into_owned());
        }
        Ok(out)
    })
    .await
    .map_err(|_| hghub_hpp::PluginError::internal("无法打开文件选择器"))?
}

fn stage_dir(app: &tauri::AppHandle) -> Result<std::path::PathBuf> {
    use tauri::Manager;
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|_| hghub_hpp::PluginError::internal("无法确定应用数据目录"))?
        .join("plugin-imports");
    std::fs::create_dir_all(&dir)
        .map_err(|e| hghub_hpp::PluginError::internal(format!("无法创建导入暂存目录：{e}")))?;
    Ok(dir)
}

fn prune_staging(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let Some(cutoff) =
        std::time::SystemTime::now().checked_sub(std::time::Duration::from_secs(STAGE_KEEP_SECS))
    else {
        return;
    };
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| t < cutoff)
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn staged_name(location: &str, index: usize, stamp: u128) -> String {
    // 选择器/Inbox 返回的最后一段常是百分号编码过的（如 %E8%A5%BF…）：
    // 先解码再切词干/扩展名，否则 `%` 会被下面的 safe 映射换成 `_`，
    // 中文名就不可逆地毁成 `_E8_A5_BF…`。
    let raw = location.rsplit('/').next().unwrap_or("").trim();
    let decoded = if raw.is_empty() {
        "plugin.js".to_string()
    } else {
        percent_decode(raw)
    };
    let last = decoded.as_str();
    let (stem, ext) = match last.rfind('.') {
        Some(i) if i > 0 => (&last[..i], &last[i + 1..]),
        _ => (last, "js"),
    };
    let safe: String = stem
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{safe}-{stamp}-{index}.{ext}")
}

/// 百分号解码，解不出的原样保留（与 hpp distribution.rs 的同款逻辑一致）。
fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (
                (bytes[i + 1] as char).to_digit(16),
                (bytes[i + 2] as char).to_digit(16),
            ) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn read_picked(app: &tauri::AppHandle, file: &tauri_plugin_fs::FilePath) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    let mut options = tauri_plugin_fs::OpenOptions::new();
    options.read(true);
    let opened = app.fs().open(file.clone(), options).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            hghub_hpp::PluginError::invalid("文件已不存在，请重新打开附件再导入")
        } else {
            hghub_hpp::PluginError::internal(format!("无法读取所选文件：{e}"))
        }
    })?;
    opened
        .take((STAGE_LIMIT + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| hghub_hpp::PluginError::internal(format!("无法读取所选文件：{e}")))?;
    if bytes.len() > STAGE_LIMIT {
        return Err(hghub_hpp::PluginError::invalid("插件文件超过 2 MiB"));
    }
    Ok(bytes)
}

pub struct PluginHost {
    dir: std::path::PathBuf,
    ready: tokio::sync::OnceCell<Arc<PluginManager>>,
}
impl PluginHost {
    pub fn new(dir: std::path::PathBuf) -> Self {
        Self {
            dir,
            ready: Default::default(),
        }
    }
    /// 懒初始化插件管理器（hpp_manage 与 serve_download 共用）。
    pub async fn manager(
        &self,
        app: &tauri::AppHandle,
    ) -> Result<Arc<PluginManager>> {
        self.ready
            .get_or_try_init(|| async {
                // 边车地址文件：引导启动 CPython 后由 main.py 写出实际地址。
                let info_path = {
                    use tauri::Manager;
                    app.path()
                        .app_data_dir()
                        .unwrap_or_else(|_| self.dir.clone())
                        .join("python-sidecar.json")
                };
                let manager = PluginManager::open(self.dir.clone())?
                    .with_document_reader(Arc::new(PickedDocuments(app.clone())))
                    .with_python_bootstrap(crate::python::python_bootstrap(info_path));
                #[cfg(target_os = "windows")]
                let manager = match crate::windows_login::WindowsLogin::detect() {
                    Some(provider) => manager.with_web_login(Arc::new(provider)),
                    None => manager,
                };
                #[cfg(target_os = "android")]
                let manager = {
                    use tauri::Manager;
                    let provider = app.state::<crate::android_login::AndroidLogin>();
                    if provider.supported().await? {
                        manager.with_web_login(Arc::new(provider.inner().clone()))
                    } else {
                        manager
                    }
                };
                let manager = Arc::new(manager);
                // 回环媒体服务预热：Python 蜘蛛初始化要把 base 传给边车（取数桥 /
                // 日志桥的目标），而初始化发生在媒体请求的处理路径上，不能在那里
                // 现启动（路由构造会引用 serve_resource，形成 Send 循环依赖），
                // 所以在管理器第一次建好时就启动一次。失败即整个管理器不可用。
                hghub_hpp::media::MediaSessions::start(&manager).await?;
                Ok::<_, hghub_hpp::PluginError>(manager)
            })
            .await
            .map(|m| m.clone())
    }
}

#[tauri::command]
pub async fn hpp_manage(
    app: tauri::AppHandle,
    host: tauri::State<'_, PluginHost>,
    action: String,
    params: Value,
) -> Result<Value> {
    let manager = host.manager(&app).await?;
    hghub_hpp::ipc::dispatch(&manager, action, params).await
}
