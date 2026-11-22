//! 当前资源的下载：经回环播放地址取流，存到文稿/Downloads。
//!
//! 只消费已有的回环地址（media.url），不改媒体服务器、不改插件协议。
//! 直链文件（MP4 等）直接流式写盘，不读入内存；HLS 先取清单、再按顺序
//! 取分片拼成一个文件（TS 直接拼，fMP4 先写 MAP 初始化段再拼媒体分片）。
//! 协议 09 §5/§6：下载与播放分别排队，一次只下一个，让位于前台播放；
//! 只在容器与范围连续性允许时写盘，不续传——中途失败删掉未完成文件，
//! 由用户重新点按下载（即重启当前文件）。加密 HLS（EXT-X-KEY）与
//! BYTERANGE 分片暂不支持，明确报错，不拼出损坏文件。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use reqwest::StatusCode;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use url::Url;

/// 同时只允许一个下载（协议 09 §6：下载队列让位于前台播放；顺序取流
/// 也避免多个下载抢媒体传输许可）。
static DOWNLOADING: AtomicBool = AtomicBool::new(false);

struct DownloadGuard;

impl DownloadGuard {
    fn acquire() -> Result<Self, String> {
        DOWNLOADING
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .map(|_| DownloadGuard)
            .map_err(|_| "已有下载进行中，请稍候".to_string())
    }
}

impl Drop for DownloadGuard {
    fn drop(&mut self) {
        DOWNLOADING.store(false, Ordering::Release);
    }
}

/// 下载完成返回给前端的结果。
#[derive(Serialize, Clone)]
pub struct DownloadResult {
    pub path: String,
    pub bytes: u64,
    #[serde(rename = "durationSec")]
    pub duration_sec: Option<f64>,
    pub live: bool,
}

/// 进度事件（download-progress）的载荷，按 download_id 过滤。
#[derive(Serialize, Clone)]
struct DownloadProgress {
    id: String,
    downloaded: u64,
    total: Option<u64>,
    #[serde(rename = "segmentsDone")]
    segments_done: Option<usize>,
    #[serde(rename = "segmentsTotal")]
    segments_total: Option<usize>,
}

/// 清单最多读 8 MiB（清单只是文本；超过即异常，不继续读）。
const MAX_PLAYLIST_BYTES: usize = 8 * 1024 * 1024;

#[tauri::command]
pub async fn download_media(
    app: AppHandle,
    url: String,
    name: String,
    download_id: String,
) -> Result<DownloadResult, String> {
    let _guard = DownloadGuard::acquire()?;
    download_inner(&app, &url, &name, &download_id).await
}

async fn download_inner(
    app: &AppHandle,
    url: &str,
    name: &str,
    download_id: &str,
) -> Result<DownloadResult, String> {
    let base = Url::parse(url).map_err(|_| "播放地址无效".to_string())?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("无法建立下载连接：{e}"))?;
    let dir = app
        .path()
        .document_dir()
        .map_err(|e| e.to_string())?
        .join("Downloads");
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("无法创建下载目录：{e}"))?;

    let mut resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| download_error(&e))?;
    check_status(resp.status())?;
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();
    let total = resp.content_length();

    // 先读第一块：回环服务器给清单的 Content-Type 是 mpegurl；个别
    // 情况下类型不对时再按内容嗅探（#EXTM3U）。
    let first = resp.chunk().await.map_err(|e| download_error(&e))?;
    let mut head: Vec<u8> = Vec::new();
    if let Some(b) = &first {
        head.extend_from_slice(b);
    }
    let is_playlist = content_type.contains("mpegurl") || head.starts_with(b"#EXTM3U");
    if is_playlist {
        let text = read_playlist_text(resp, head).await?;
        return download_hls(app, &client, &base, &dir, name, download_id, &text).await;
    }
    // 直链文件：扩展名按 Content-Type 定，流式写盘。
    let ext = extension_for_content_type(&content_type);
    let (path, part) = output_paths(&dir, name, ext);
    match download_progressive(app, resp, head, &part, download_id, total).await {
        Ok(bytes) => {
            finish_part(&part, &path).await?;
            Ok(DownloadResult {
                path: path_string(&path),
                bytes,
                duration_sec: None,
                live: false,
            })
        }
        Err(e) => {
            discard_part(&part).await;
            Err(e)
        }
    }
}

/// 直链文件：把已读的第一块与剩余响应流式写盘。
async fn download_progressive(
    app: &AppHandle,
    mut resp: reqwest::Response,
    head: Vec<u8>,
    part: &PathBuf,
    download_id: &str,
    total: Option<u64>,
) -> Result<u64, String> {
    let mut file = File::create(part)
        .await
        .map_err(|e| format!("无法创建下载文件：{e}"))?;
    let mut downloaded: u64 = 0;
    if !head.is_empty() {
        file.write_all(&head)
            .await
            .map_err(|e| format!("写入失败：{e}"))?;
        downloaded += head.len() as u64;
        emit_progress(app, download_id, downloaded, total, None, None);
    }
    while let Some(chunk) = resp.chunk().await.map_err(|e| download_error(&e))? {
        file.write_all(&chunk)
            .await
            .map_err(|e| format!("写入失败：{e}"))?;
        downloaded += chunk.len() as u64;
        emit_progress(app, download_id, downloaded, total, None, None);
    }
    file.flush().await.map_err(|e| format!("写入失败：{e}"))?;
    Ok(downloaded)
}

/// 清单只是文本：把剩余响应读进内存（有上限），转 UTF-8。
async fn read_playlist_text(
    mut resp: reqwest::Response,
    head: Vec<u8>,
) -> Result<String, String> {
    let mut buf = head;
    while let Some(chunk) = resp.chunk().await.map_err(|e| download_error(&e))? {
        if buf.len() + chunk.len() > MAX_PLAYLIST_BYTES {
            return Err("播放清单过大，暂不支持下载".to_string());
        }
        buf.extend_from_slice(&chunk);
    }
    String::from_utf8(buf).map_err(|_| "播放清单不是有效文本".to_string())
}

struct Variant {
    uri: String,
    bandwidth: u64,
}

struct MediaPlaylist {
    /// 主清单（多清晰度）还是媒体清单（分片）。
    master: bool,
    variants: Vec<Variant>,
    segments: Vec<(String, f64)>,
    init: Option<String>,
    /// 没有 #EXT-X-ENDLIST 即直播：只下当前窗口的分片，存成一段片段。
    live: bool,
}

fn parse_media_playlist(text: &str) -> Result<MediaPlaylist, String> {
    // 加密分片不能直接拼（要密钥解密），BYTERANGE 分片不是独立文件，
    // 都明确拒绝，不拼出损坏文件（协议 09 §4）。
    for line in text.lines() {
        if line.starts_with("#EXT-X-KEY") && !line.contains("METHOD=NONE") {
            return Err("加密视频暂不支持下载".to_string());
        }
    }
    if text.contains("BYTERANGE") {
        return Err("该视频暂不支持下载（分片按字节范围切分）".to_string());
    }
    let live = !text.contains("#EXT-X-ENDLIST");
    let master = text.contains("#EXT-X-STREAM-INF");
    let mut variants: Vec<Variant> = Vec::new();
    let mut segments: Vec<(String, f64)> = Vec::new();
    let mut init: Option<String> = None;
    let mut pending_bandwidth: Option<u64> = None;
    let mut pending_duration: Option<f64> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            pending_bandwidth = rest
                .split(',')
                .find_map(|kv| kv.trim().strip_prefix("BANDWIDTH="))
                .and_then(|v| v.parse().ok());
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-MAP:") {
            init = extract_uri_attr(rest);
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            pending_duration = rest.split(',').next().and_then(|v| v.trim().parse().ok());
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        if master {
            variants.push(Variant {
                uri: line.to_string(),
                bandwidth: pending_bandwidth.take().unwrap_or(0),
            });
        } else {
            segments.push((line.to_string(), pending_duration.take().unwrap_or(0.0)));
        }
    }
    Ok(MediaPlaylist {
        master,
        variants,
        segments,
        init,
        live,
    })
}

/// 从 `URI="..."` 这类属性里取值。
fn extract_uri_attr(rest: &str) -> Option<String> {
    rest.split(',')
        .find_map(|kv| kv.trim().strip_prefix("URI="))
        .map(|v| v.trim_matches('"').to_string())
}

/// 小文本（子清单）直接读全。
async fn fetch_text(client: &reqwest::Client, url: &str) -> Result<String, String> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| download_error(&e))?;
    check_status(resp.status())?;
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| download_error(&e))?;
    if bytes.len() > MAX_PLAYLIST_BYTES {
        return Err("播放清单过大，暂不支持下载".to_string());
    }
    String::from_utf8(bytes.to_vec()).map_err(|_| "播放清单不是有效文本".to_string())
}

async fn download_hls(
    app: &AppHandle,
    client: &reqwest::Client,
    base: &Url,
    dir: &PathBuf,
    name: &str,
    download_id: &str,
    text: &str,
) -> Result<DownloadResult, String> {
    // 主清单只取一档（带宽最高的一档，没有带宽信息时取第一档），
    // 再按它的媒体清单下分片。
    let mut playlist = parse_media_playlist(text)?;
    if playlist.master {
        let variant = playlist
            .variants
            .iter()
            .max_by_key(|v| v.bandwidth)
            .ok_or_else(|| "主清单里没有可选清晰度".to_string())?;
        let variant_url = base
            .join(&variant.uri)
            .map_err(|_| "播放清单地址无效".to_string())?;
        let sub = fetch_text(client, variant_url.as_str()).await?;
        playlist = parse_media_playlist(&sub)?;
        if playlist.master {
            return Err("多级主清单暂不支持下载".to_string());
        }
    }
    let ext = if playlist.init.is_some() { "mp4" } else { "ts" };
    let (path, part) = output_paths(dir, name, ext);
    match download_hls_segments(app, client, base, &playlist, &part, download_id).await {
        Ok((bytes, duration)) => {
            finish_part(&part, &path).await?;
            Ok(DownloadResult {
                path: path_string(&path),
                bytes,
                duration_sec: Some(duration),
                live: playlist.live,
            })
        }
        Err(e) => {
            discard_part(&part).await;
            Err(e)
        }
    }
}

/// HLS：fMP4 先写 MAP 初始化段，再按顺序取分片、逐个写盘。
/// 一次只取一个分片，让位于前台播放（协议 09 §6）。
async fn download_hls_segments(
    app: &AppHandle,
    client: &reqwest::Client,
    base: &Url,
    playlist: &MediaPlaylist,
    part: &PathBuf,
    download_id: &str,
) -> Result<(u64, f64), String> {
    let mut file = File::create(part)
        .await
        .map_err(|e| format!("无法创建下载文件：{e}"))?;
    let mut downloaded: u64 = 0;
    if let Some(init_uri) = &playlist.init {
        let init_url = base
            .join(init_uri)
            .map_err(|_| "播放清单地址无效".to_string())?;
        downloaded += fetch_into_file(client, init_url.as_str(), &mut file).await?;
    }
    let total_segments = playlist.segments.len();
    for (i, (uri, _)) in playlist.segments.iter().enumerate() {
        let seg_url = base.join(uri).map_err(|_| "播放清单地址无效".to_string())?;
        downloaded += fetch_into_file(client, seg_url.as_str(), &mut file).await?;
        emit_progress(
            app,
            download_id,
            downloaded,
            None,
            Some(i + 1),
            Some(total_segments),
        );
    }
    file.flush().await.map_err(|e| format!("写入失败：{e}"))?;
    let duration: f64 = playlist.segments.iter().map(|(_, d)| d).sum();
    Ok((downloaded, duration))
}

/// 取一个分片/初始化段并流式写盘。
async fn fetch_into_file(
    client: &reqwest::Client,
    url: &str,
    file: &mut File,
) -> Result<u64, String> {
    let mut resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| download_error(&e))?;
    check_status(resp.status())?;
    let mut written: u64 = 0;
    while let Some(chunk) = resp.chunk().await.map_err(|e| download_error(&e))? {
        file.write_all(&chunk)
            .await
            .map_err(|e| format!("写入失败：{e}"))?;
        written += chunk.len() as u64;
    }
    Ok(written)
}

/// 成功时把 .part 改名成正式文件；失败时删掉 .part。
async fn finish_part(part: &PathBuf, path: &PathBuf) -> Result<(), String> {
    tokio::fs::rename(part, path)
        .await
        .map_err(|e| format!("无法保存下载文件：{e}"))
}

async fn discard_part(part: &PathBuf) {
    let _ = tokio::fs::remove_file(part).await;
}

fn emit_progress(
    app: &AppHandle,
    download_id: &str,
    downloaded: u64,
    total: Option<u64>,
    segments_done: Option<usize>,
    segments_total: Option<usize>,
) {
    let _ = app.emit(
        "download-progress",
        DownloadProgress {
            id: download_id.to_string(),
            downloaded,
            total,
            segments_done,
            segments_total,
        },
    );
}

/// 会话过期（410/401）是下载最常见的失败：地址只在当前播放会话有效。
fn check_status(status: StatusCode) -> Result<(), String> {
    if status == StatusCode::GONE || status == StatusCode::UNAUTHORIZED {
        return Err("播放地址已失效，请重新播放后再下载".to_string());
    }
    if !status.is_success() {
        return Err(format!("下载失败（HTTP {status}）"));
    }
    Ok(())
}

fn download_error(e: &reqwest::Error) -> String {
    if e.is_connect() {
        return "无法连接播放服务，请重试".to_string();
    }
    if e.is_timeout() {
        return "下载超时，请重试".to_string();
    }
    format!("下载失败：{e}")
}

/// 标题做文件名：去掉路径分隔与非法字符，超长截断，空了用“视频”。
fn sanitize_filename(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect();
    s = s.trim().trim_matches('.').to_string();
    if s.chars().count() > 80 {
        s = s.chars().take(80).collect();
        s = s.trim().to_string();
    }
    if s.is_empty() {
        s = "视频".to_string();
    }
    s
}

/// 同名文件已存在时加（1）（2）…，不静默覆盖已下载的文件。
fn output_paths(dir: &PathBuf, name: &str, ext: &str) -> (PathBuf, PathBuf) {
    let stem = sanitize_filename(name);
    let mut n = 0;
    loop {
        let filename = if n == 0 {
            format!("{stem}.{ext}")
        } else {
            format!("{stem}（{n}）.{ext}")
        };
        let path = dir.join(&filename);
        if !path.exists() {
            let part = dir.join(format!("{filename}.part"));
            return (path, part);
        }
        n += 1;
        if n > 100 {
            // 极端情况：用时间戳兜底，避免无限循环。
            let filename = format!(
                "{stem}-{}.{ext}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            );
            let path = dir.join(&filename);
            let part = dir.join(format!("{filename}.part"));
            return (path, part);
        }
    }
}

fn extension_for_content_type(content_type: &str) -> &'static str {
    if content_type.contains("mp4") {
        "mp4"
    } else if content_type.contains("matroska") {
        "mkv"
    } else if content_type.contains("webm") {
        "webm"
    } else if content_type.contains("quicktime") {
        "mov"
    } else if content_type.contains("x-msvideo") {
        "avi"
    } else if content_type.contains("x-flv") {
        "flv"
    } else if content_type.contains("mp2t") || content_type.contains("video/mpeg") {
        "ts"
    } else {
        "mp4"
    }
}

fn path_string(path: &PathBuf) -> String {
    path.to_string_lossy().into_owned()
}
