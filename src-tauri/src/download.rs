//! 当前资源的下载：经回环播放地址取流，存到文稿/Downloads。
//!
//! 只消费已有的回环地址（media.url），不改媒体服务器、不改插件协议。
//! 直链文件（MP4 等）直接流式写盘，不读入内存；HLS 先取清单、再按顺序
//! 取分片拼成一个文件（TS 直接拼，fMP4 先写 MAP 初始化段再拼媒体分片）。
//! fMP4 源若把音频放在独立的 EXT-X-MEDIA 清单里（主清单的
//! #EXT-X-STREAM-INF 带 AUDIO="..."），只下视频分片拼出的文件没有音轨，
//! 本机播就没声音——这时把音频清单的分片也取下来合轨（见 fmp4.rs），
//! 音频没有初始化段（如 TS 音频）时仍只存视频，不拼坏文件。
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
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use url::Url;

use crate::fmp4;

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
    folder: Option<String>,
) -> Result<DownloadResult, String> {
    let _guard = DownloadGuard::acquire()?;
    download_inner(&app, &url, &name, &download_id, folder.as_deref()).await
}

async fn download_inner(
    app: &AppHandle,
    url: &str,
    name: &str,
    download_id: &str,
    folder: Option<&str>,
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
    // 多集资源按文件夹归类：folder 是资源名（如作品标题），分集文件
    // 存进 Downloads/<folder>/；单文件资源不传 folder，直接散放在
    // Downloads 下。文件夹名走与文件名同一套清理规则（非法字符换 _、
    // 超长截断），空串当没传。
    let dir = match folder.map(str::trim).filter(|f| !f.is_empty()) {
        Some(folder) => dir.join(sanitize_filename(folder)),
        None => dir,
    };
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
    /// #EXT-X-STREAM-INF 上的 AUDIO="..."，指向音频组（GROUP-ID）。
    audio: Option<String>,
}

/// 主清单里的一个音频组（#EXT-X-MEDIA:TYPE=AUDIO）。
#[derive(Clone)]
struct AudioGroup {
    group_id: String,
    uri: String,
    default: bool,
}

struct MediaPlaylist {
    /// 主清单（多清晰度）还是媒体清单（分片）。
    master: bool,
    variants: Vec<Variant>,
    segments: Vec<(String, f64)>,
    init: Option<String>,
    /// 没有 #EXT-X-ENDLIST 即直播：只下当前窗口的分片，存成一段片段。
    live: bool,
    /// 只在主清单里有：独立音频组的清单地址。
    audio_groups: Vec<AudioGroup>,
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
    let mut audio_groups: Vec<AudioGroup> = Vec::new();
    let mut pending_bandwidth: Option<u64> = None;
    let mut pending_audio: Option<String> = None;
    let mut pending_duration: Option<f64> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA:") {
            // 独立音频组：只收 TYPE=AUDIO 的；URI 是相对主清单的地址。
            let attrs = parse_attrs(rest);
            let get = |k: &str| attrs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
            if get("TYPE").as_deref() == Some("AUDIO") {
                if let (Some(group_id), Some(uri)) = (get("GROUP-ID"), get("URI")) {
                    audio_groups.push(AudioGroup {
                        group_id,
                        uri,
                        default: get("DEFAULT").as_deref() == Some("YES"),
                    });
                }
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            let attrs = parse_attrs(rest);
            pending_bandwidth = attrs
                .iter()
                .find(|(k, _)| k == "BANDWIDTH")
                .and_then(|(_, v)| v.parse().ok());
            pending_audio = attrs
                .iter()
                .find(|(k, _)| k == "AUDIO")
                .map(|(_, v)| v.clone());
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
                audio: pending_audio.take(),
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
        audio_groups,
    })
}

/// 逗号分隔的属性表（`BANDWIDTH=123,AUDIO="a"`）：值可带引号，引号里的
/// 逗号不切分；返回的键名与值都不带引号与空白。
fn parse_attrs(rest: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for ch in rest.chars() {
        match ch {
            '"' => {
                quoted = !quoted;
                cur.push(ch);
            }
            ',' if !quoted => {
                push_attr(&mut out, std::mem::take(&mut cur));
            }
            _ => cur.push(ch),
        }
    }
    push_attr(&mut out, cur);
    out
}

fn push_attr(out: &mut Vec<(String, String)>, part: String) {
    let part = part.trim();
    if part.is_empty() {
        return;
    }
    if let Some((k, v)) = part.split_once('=') {
        out.push((
            k.trim().to_string(),
            v.trim().trim_matches('"').to_string(),
        ));
    }
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
    // 再按它的媒体清单下分片。分片地址按所在清单的地址解析（子清单在
    // 子目录里时，相对地址要以子清单为准，不是以主清单）。
    let mut playlist = parse_media_playlist(text)?;
    let mut video_base = base.clone();
    let mut audio_url: Option<Url> = None;
    if playlist.master {
        let variant = playlist
            .variants
            .iter()
            .max_by_key(|v| v.bandwidth)
            .ok_or_else(|| "主清单里没有可选清晰度".to_string())?;
        let variant_audio = variant.audio.clone();
        let master_groups = std::mem::take(&mut playlist.audio_groups);
        let variant_url = base
            .join(&variant.uri)
            .map_err(|_| "播放清单地址无效".to_string())?;
        let sub = fetch_text(client, variant_url.as_str()).await?;
        playlist = parse_media_playlist(&sub)?;
        if playlist.master {
            return Err("多级主清单暂不支持下载".to_string());
        }
        video_base = variant_url;
        audio_url = resolve_audio_url(base, &variant_audio, &master_groups);
    }
    let ext = if playlist.init.is_some() { "mp4" } else { "ts" };
    let (path, part) = output_paths(dir, name, ext);
    // 音频清单：取不到、本身又是主清单、或没有分片时不合轨；
    // 只有视频与音频都是 fMP4（都有 MAP 初始化段）才合轨，TS 音频
    // 暂不支持合轨，仍只存视频，不拼坏文件。
    let audio_playlist: Option<(Url, MediaPlaylist)> = match audio_url {
        Some(url) => {
            let atext = fetch_text(client, url.as_str()).await?;
            let ap = parse_media_playlist(&atext)?;
            if ap.master {
                return Err("音频清单暂不支持多级主清单".to_string());
            }
            Some((url, ap))
        }
        None => None,
    };
    let mux = match &audio_playlist {
        Some((_, ap)) => {
            playlist.init.is_some()
                && ap.init.is_some()
                && !playlist.segments.is_empty()
                && !ap.segments.is_empty()
        }
        None => false,
    };
    let result = if mux {
        // mux 为真时 audio_playlist 一定是 Some。
        let (aurl, ap) = match audio_playlist {
            Some((u, p)) => (u, p),
            None => return Err("音频清单丢失".to_string()),
        };
        download_hls_muxed(
            app,
            client,
            &video_base,
            &playlist,
            &aurl,
            &ap,
            &part,
            download_id,
        )
        .await
    } else {
        download_hls_segments(app, client, &video_base, &playlist, &part, download_id).await
    };
    match result {
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

/// 音频组解析：优先用选中清晰度点的 AUDIO 组，其次 DEFAULT=YES 的组，
/// 再其次第一个音频组；地址按主清单解析。
fn resolve_audio_url(
    base: &Url,
    variant_audio: &Option<String>,
    groups: &[AudioGroup],
) -> Option<Url> {
    let group = variant_audio
        .as_deref()
        .and_then(|g| groups.iter().find(|a| a.group_id == g))
        .or_else(|| groups.iter().find(|a| a.default))
        .or_else(|| groups.first())?;
    base.join(&group.uri).ok()
}

/// fMP4 合轨下载：两路初始化段合并（fmp4::merge_inits），两路分片
/// 各自顺序取下、记下每片的时间（moof 里 tfdt），最后按时间交错写盘。
/// 分片先落在与 .part 同目录的临时文件里（内存只留一片），成败都删。
async fn download_hls_muxed(
    app: &AppHandle,
    client: &reqwest::Client,
    video_base: &Url,
    video: &MediaPlaylist,
    audio_url: &Url,
    audio: &MediaPlaylist,
    part: &PathBuf,
    download_id: &str,
) -> Result<(u64, f64), String> {
    let vtmp = PathBuf::from(format!("{}.vtmp", part.display()));
    let atmp = PathBuf::from(format!("{}.atmp", part.display()));
    let r = download_hls_muxed_inner(
        app, client, video_base, video, audio_url, audio, part, download_id, &vtmp, &atmp,
    )
    .await;
    let _ = tokio::fs::remove_file(&vtmp).await;
    let _ = tokio::fs::remove_file(&atmp).await;
    r
}

async fn download_hls_muxed_inner(
    app: &AppHandle,
    client: &reqwest::Client,
    video_base: &Url,
    video: &MediaPlaylist,
    audio_url: &Url,
    audio: &MediaPlaylist,
    part: &PathBuf,
    download_id: &str,
    vtmp: &PathBuf,
    atmp: &PathBuf,
) -> Result<(u64, f64), String> {
    let video_init_uri = video
        .init
        .as_deref()
        .ok_or("视频清单缺少初始化段")?;
    let audio_init_uri = audio
        .init
        .as_deref()
        .ok_or("音频清单缺少初始化段")?;
    let video_init = fetch_bytes(
        client,
        video_base
            .join(video_init_uri)
            .map_err(|_| "播放清单地址无效".to_string())?
            .as_str(),
    )
    .await?;
    let audio_init = fetch_bytes(
        client,
        audio_url
            .join(audio_init_uri)
            .map_err(|_| "播放清单地址无效".to_string())?
            .as_str(),
    )
    .await?;
    let (merged_init, audio_id_map) = fmp4::merge_inits(&video_init, &audio_init)?;
    let timescales = fmp4::track_timescales(&merged_init)?;
    let total_segments = video.segments.len() + audio.segments.len();
    let mut downloaded: u64 = (video_init.len() + audio_init.len()) as u64;

    // 视频分片：顺序取，记（临时文件里的偏移，长度，解码时间）。
    let mut vfile = File::create(vtmp)
        .await
        .map_err(|e| format!("无法创建下载文件：{e}"))?;
    let mut vtimes: Vec<(u64, u64, f64)> = Vec::with_capacity(video.segments.len());
    let mut voff: u64 = 0;
    for (i, (uri, _)) in video.segments.iter().enumerate() {
        let seg_url = video_base
            .join(uri)
            .map_err(|_| "播放清单地址无效".to_string())?;
        let bytes = fetch_bytes(client, seg_url.as_str()).await?;
        let t = fmp4::fragment_time(&bytes, &timescales)?;
        vfile
            .write_all(&bytes)
            .await
            .map_err(|e| format!("写入失败：{e}"))?;
        vtimes.push((voff, bytes.len() as u64, t));
        voff += bytes.len() as u64;
        downloaded += bytes.len() as u64;
        emit_progress(app, download_id, downloaded, None, Some(i + 1), Some(total_segments));
    }
    vfile.flush().await.map_err(|e| format!("写入失败：{e}"))?;

    // 音频分片：同理，进度接着数。
    let mut afile = File::create(atmp)
        .await
        .map_err(|e| format!("无法创建下载文件：{e}"))?;
    let mut atimes: Vec<(u64, u64, f64)> = Vec::with_capacity(audio.segments.len());
    let mut aoff: u64 = 0;
    for (j, (uri, _)) in audio.segments.iter().enumerate() {
        let seg_url = audio_url
            .join(uri)
            .map_err(|_| "播放清单地址无效".to_string())?;
        let mut bytes = fetch_bytes(client, seg_url.as_str()).await?;
        // 音频 init 的 track_ID 常与视频撞号（都从 1 开始），分片里的
        // tfhd / sidx 须按同一张表换成合轨后的新 ID，否则按旧 ID 查
        // timescale 会算错时间、交错顺序全乱。
        for (old, new) in &audio_id_map {
            fmp4::remap_fragment_track_ids(&mut bytes, *old, *new)?;
        }
        let t = fmp4::fragment_time(&bytes, &timescales)?;
        afile
            .write_all(&bytes)
            .await
            .map_err(|e| format!("写入失败：{e}"))?;
        atimes.push((aoff, bytes.len() as u64, t));
        aoff += bytes.len() as u64;
        downloaded += bytes.len() as u64;
        emit_progress(
            app,
            download_id,
            downloaded,
            None,
            Some(video.segments.len() + j + 1),
            Some(total_segments),
        );
    }
    afile.flush().await.map_err(|e| format!("写入失败：{e}"))?;

    // 写盘：先写合并后的初始化段，再按时间交错拷分片（同时间视频在前）。
    let mut out = File::create(part)
        .await
        .map_err(|e| format!("无法创建下载文件：{e}"))?;
    out.write_all(&merged_init)
        .await
        .map_err(|e| format!("写入失败：{e}"))?;
    let mut written: u64 = merged_init.len() as u64;
    let vtimes_s: Vec<f64> = vtimes.iter().map(|(_, _, t)| *t).collect();
    let atimes_s: Vec<f64> = atimes.iter().map(|(_, _, t)| *t).collect();
    for (is_video, idx) in fmp4::interleave_order(&vtimes_s, &atimes_s) {
        let (tmp, off, len) = if is_video {
            (vtmp, vtimes[idx].0, vtimes[idx].1)
        } else {
            (atmp, atimes[idx].0, atimes[idx].1)
        };
        copy_range(tmp, off, len, &mut out).await?;
        written += len;
    }
    out.flush().await.map_err(|e| format!("写入失败：{e}"))?;
    let duration: f64 = video.segments.iter().map(|(_, d)| d).sum();
    Ok((written, duration))
}

/// 从临时文件里把 [offset, offset+len) 拷进输出文件。
async fn copy_range(src: &PathBuf, offset: u64, len: u64, out: &mut File) -> Result<(), String> {
    let mut f = File::open(src)
        .await
        .map_err(|e| format!("读取临时文件失败：{e}"))?;
    f.seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(|e| format!("读取临时文件失败：{e}"))?;
    let mut limited = f.take(len);
    tokio::io::copy(&mut limited, out)
        .await
        .map_err(|e| format!("写入失败：{e}"))?;
    Ok(())
}

/// 取一个小文件（初始化段/分片）进内存；设 64 MiB 上限防异常。
async fn fetch_bytes(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, String> {
    const MAX_FRAGMENT_BYTES: usize = 64 * 1024 * 1024;
    let mut resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| download_error(&e))?;
    check_status(resp.status())?;
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| download_error(&e))? {
        if out.len() + chunk.len() > MAX_FRAGMENT_BYTES {
            return Err("分片过大，暂不支持下载".to_string());
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
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

/// 已下载的文件（供设置里的下载管理列出、分享、删除）。
/// folder 是文件所在的资源文件夹名（多集资源按文件夹归类时才有，
/// 如作品标题）；单文件资源散放在 Downloads 下，folder 为 null。
#[derive(Serialize, Clone)]
pub struct DownloadedFile {
    pub name: String,
    pub path: String,
    pub bytes: u64,
    /// 修改时间的 Unix 秒；取不到时为 null。
    pub modified: Option<u64>,
    pub folder: Option<String>,
}

/// 列出文稿/Downloads 里的已完成下载（跳过 .part 未完成文件），按修改时间倒序。
/// 目录不存在时返回空列表（还没下过东西，不是错误）。
/// 只往下走一层：顶层文件是单文件资源，子目录是一个资源的分集文件夹，
/// 里面的文件带上 folder；更深的嵌套不会出现（下载只建一层文件夹）。
#[tauri::command]
pub async fn list_downloads(app: AppHandle) -> Result<Vec<DownloadedFile>, String> {
    let dir = app
        .path()
        .document_dir()
        .map_err(|e| e.to_string())?
        .join("Downloads");
    let mut entries = match tokio::fs::read_dir(&dir).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("无法读取下载目录：{e}")),
    };
    let mut files = Vec::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|e| format!("无法读取下载目录：{e}"))?
    {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("part") {
            continue;
        }
        let meta = match entry.metadata().await {
            Ok(m) => m,
            _ => continue,
        };
        if meta.is_dir() {
            // 资源文件夹：列出里面的一层文件。
            let folder_name = entry.file_name().to_string_lossy().into_owned();
            let mut sub = match tokio::fs::read_dir(&path).await {
                Ok(sub) => sub,
                Err(_) => continue,
            };
            while let Some(sub_entry) = sub
                .next_entry()
                .await
                .map_err(|e| format!("无法读取下载目录：{e}"))?
            {
                let sub_path = sub_entry.path();
                if sub_path.extension().and_then(|e| e.to_str()) == Some("part") {
                    continue;
                }
                let sub_meta = match sub_entry.metadata().await {
                    Ok(m) if m.is_file() => m,
                    _ => continue,
                };
                files.push(DownloadedFile {
                    name: sub_entry.file_name().to_string_lossy().into_owned(),
                    path: path_string(&sub_path),
                    bytes: sub_meta.len(),
                    modified: file_modified(&sub_meta),
                    folder: Some(folder_name.clone()),
                });
            }
            continue;
        }
        if !meta.is_file() {
            continue;
        }
        files.push(DownloadedFile {
            name: entry.file_name().to_string_lossy().into_owned(),
            path: path_string(&path),
            bytes: meta.len(),
            modified: file_modified(&meta),
            folder: None,
        });
    }
    files.sort_by_key(|f| std::cmp::Reverse(f.modified.unwrap_or(0)));
    Ok(files)
}

fn file_modified(meta: &std::fs::Metadata) -> Option<u64> {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

/// 删除一个已下载文件。只允许删 Downloads 目录里的文件（路径须落在该目录下），
/// 防止误删应用外文件。
#[tauri::command]
pub async fn delete_download(app: AppHandle, path: String) -> Result<(), String> {
    let dir = app
        .path()
        .document_dir()
        .map_err(|e| e.to_string())?
        .join("Downloads");
    let dir = tokio::fs::canonicalize(&dir)
        .await
        .map_err(|_| "下载目录不存在".to_string())?;
    let target = tokio::fs::canonicalize(PathBuf::from(&path))
        .await
        .map_err(|_| "文件不存在".to_string())?;
    if !target.starts_with(&dir) {
        return Err("只能删除下载目录里的文件".to_string());
    }
    if !target.is_file() {
        return Err("只能删除文件".to_string());
    }
    tokio::fs::remove_file(&target)
        .await
        .map_err(|e| format!("删除失败：{e}"))
}
