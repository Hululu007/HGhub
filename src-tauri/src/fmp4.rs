//! fMP4 合轨：把视频分片与独立音频分片拼成一个带双轨的 .mp4。
//!
//! 背景：部分 HLS 源把音频放在独立的 EXT-X-MEDIA 清单里（主清单的
//! #EXT-X-STREAM-INF 带 AUDIO="..." 指向音频组），视频媒体清单的分片
//! 只有视频轨。只下视频分片拼出的文件没有音轨，本机播就没声音；
//! moov 声明了音轨却无音频样本时，AVPlayer 读轨还可能直接报失败。
//! 这里把两路的初始化段合并（moov 里同时放视频 trak 与音频 trak），
//! 再把两路媒体分片按解码时间（moof 里 tfdt，换算到同一时间基）交错
//! 写盘。分片本身按原样搬运（moof/mdat 不动），只要求 tfhd 带
//! default-base-is-moof（HLS fMP4 的常规写法）；不带时样本偏移在重排
//! 后会错位，直接报错、不拼坏文件。
//!
//! 纯字节操作，不依赖音视频编解码；只读 box 头与少量字段。

/// 一个 MP4 box：类型、整 box 在数据里的范围、内容起始。
#[derive(Clone, Copy, Debug)]
struct Child {
    typ: [u8; 4],
    /// 内容（payload）起始偏移。
    start: usize,
    /// 整 box 结束偏移（不含）。
    end: usize,
    /// 整 box 起始偏移。
    box_start: usize,
}

/// 从 offset 处读一个 box 头；越界或大小非法返回 None。
/// 返回（box 信息，内容起始偏移）。
fn read_head(data: &[u8], offset: usize) -> Option<(Child, usize)> {
    if offset + 8 > data.len() {
        return None;
    }
    let mut size = u32::from_be_bytes(data[offset..offset + 4].try_into().ok()?) as u64;
    let typ: [u8; 4] = data[offset + 4..offset + 8].try_into().ok()?;
    let mut header_len = 8usize;
    if size == 1 {
        // largesize：后 8 字节是 64 位大小。
        if offset + 16 > data.len() {
            return None;
        }
        size = u64::from_be_bytes(data[offset + 8..offset + 16].try_into().ok()?);
        header_len = 16;
    } else if size == 0 {
        // size 0 = 占到数据尾（只对顶层 box 合法）。
        size = (data.len() - offset) as u64;
    }
    if size < header_len as u64 {
        return None;
    }
    let end = offset.checked_add(size as usize)?;
    if end > data.len() {
        return None;
    }
    Some((
        Child {
            typ,
            start: offset + header_len,
            end,
            box_start: offset,
        },
        offset + header_len,
    ))
}

/// 列出 data[start..end] 里的直接子 box。
fn children(data: &[u8], start: usize, end: usize) -> Result<Vec<Child>, String> {
    if end > data.len() || start > end {
        return Err("box 超出数据范围".to_string());
    }
    let mut out = Vec::new();
    let mut pos = start;
    while pos < end {
        let (head, _) = read_head(data, pos).ok_or("box 头损坏")?;
        if head.end <= pos {
            return Err("box 大小非法".to_string());
        }
        out.push(head);
        pos = head.end;
    }
    if pos != end {
        return Err("子 box 未对齐".to_string());
    }
    Ok(out)
}

/// 顶层第一个指定类型的 box。
fn top(data: &[u8], typ: &[u8; 4]) -> Option<Child> {
    children(data, 0, data.len())
        .ok()?
        .into_iter()
        .find(|c| &c.typ == typ)
}

fn find<'a>(list: &'a [Child], typ: &[u8; 4]) -> Option<&'a Child> {
    list.iter().find(|c| &c.typ == typ)
}

/// 按类型拼一个 box：4 字节大端 size + 类型 + 内容。
fn wrap(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let size = (payload.len() + 8) as u32;
    let mut out = Vec::with_capacity(payload.len() + 8);
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(typ);
    out.extend_from_slice(payload);
    out
}

fn be_u32(data: &[u8], offset: usize) -> Option<u32> {
    data.get(offset..offset + 4)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_be_bytes)
}

fn be_u64(data: &[u8], offset: usize) -> Option<u64> {
    data.get(offset..offset + 8)
        .and_then(|b| b.try_into().ok())
        .map(u64::from_be_bytes)
}

/// 合并两路初始化段：ftyp 取视频的，moov 里同时放视频 trak 与音频
/// trak，mvex 里同时放两路的 trex。返回（合并后的初始化段，
/// 音频轨道的重映射表 [(旧 track_ID，新 track_ID)]）。
/// 两路 init 的 track_ID 常常撞号（都从 1 开始），音频轨必须换一个
/// 没被视频占用的 ID，否则合出的 moov 里两条 trak 同号，播放器分不
/// 清音视频轨；调用方须用同一张表把音频分片里的 tfhd / sidx 也改掉。
pub fn merge_inits(
    video_init: &[u8],
    audio_init: &[u8],
) -> Result<(Vec<u8>, Vec<(u32, u32)>), String> {
    let ftyp = top(video_init, b"ftyp").ok_or("视频初始化段缺少 ftyp")?;
    let vmoov = top(video_init, b"moov").ok_or("视频初始化段缺少 moov")?;
    let amoov = top(audio_init, b"moov").ok_or("音频初始化段缺少 moov")?;
    let vmc = children(video_init, vmoov.start, vmoov.end)?;
    let amc = children(audio_init, amoov.start, amoov.end)?;
    let mvhd = find(&vmc, b"mvhd").ok_or("视频初始化段缺少 mvhd")?;
    let vtraks: Vec<&Child> = vmc.iter().filter(|c| &c.typ == b"trak").collect();
    let atraks: Vec<&Child> = amc.iter().filter(|c| &c.typ == b"trak").collect();
    if vtraks.is_empty() {
        return Err("视频初始化段缺少 trak".to_string());
    }
    if atraks.is_empty() {
        return Err("音频初始化段缺少 trak".to_string());
    }
    // 音频轨换号：取没被视频占用的最小正整数；先改音频 init 的副本，
    // 再从副本里取 trak / trex，保证合出的 moov 与分片用的是一张表。
    let mut used: Vec<u32> = Vec::new();
    for t in &vtraks {
        let tc = children(video_init, t.start, t.end)?;
        let tkhd = find(&tc, b"tkhd").ok_or("trak 缺少 tkhd")?;
        let version = video_init.get(tkhd.start).copied().unwrap_or(0);
        let id_off = if version == 1 { 20 } else { 12 };
        used.push(be_u32(video_init, tkhd.start + id_off).ok_or("tkhd 损坏")?);
    }
    let mut audio_init_owned = audio_init.to_vec();
    let mut mapping: Vec<(u32, u32)> = Vec::new();
    for t in &atraks {
        let tc = children(audio_init, t.start, t.end)?;
        let tkhd = find(&tc, b"tkhd").ok_or("trak 缺少 tkhd")?;
        let version = audio_init.get(tkhd.start).copied().unwrap_or(0);
        let id_off = if version == 1 { 20 } else { 12 };
        let old = be_u32(audio_init, tkhd.start + id_off).ok_or("tkhd 损坏")?;
        if used.contains(&old) {
            let mut new = 1u32;
            while used.contains(&new) {
                new += 1;
            }
            remap_init_track_ids(&mut audio_init_owned, old, new)?;
            used.push(new);
            mapping.push((old, new));
        }
    }
    let audio_init = &audio_init_owned[..];
    let amoov = top(audio_init, b"moov").ok_or("音频初始化段缺少 moov")?;
    let amc = children(audio_init, amoov.start, amoov.end)?;
    let atraks: Vec<&Child> = amc.iter().filter(|c| &c.typ == b"trak").collect();
    // mvex：视频的原样保留（除 trex 外的子项，如 mehd），再把两路的
    // trex 都放进去（按 track_ID 去重——有的打包会把两轨的 trex 都
    // 写进视频 init 的 mvex）；两路都没有 mvex 时报错，不拼缺默认值
    // 的 moov。
    let vmvex = find(&vmc, b"mvex");
    let amvex = find(&amc, b"mvex");
    let mut mvex_payload: Vec<u8> = Vec::new();
    let mut seen_trex: Vec<u32> = Vec::new();
    let push_trex = |data: &[u8], m: &Child, payload: &mut Vec<u8>, seen: &mut Vec<u32>| -> Result<(), String> {
        for c in children(data, m.start, m.end)? {
            if &c.typ == b"trex" {
                let id = be_u32(data, c.start + 4).unwrap_or(0);
                if seen.contains(&id) {
                    continue;
                }
                seen.push(id);
                payload.extend_from_slice(&data[c.box_start..c.end]);
            }
        }
        Ok(())
    };
    if let Some(m) = vmvex {
        for c in children(video_init, m.start, m.end)? {
            if &c.typ != b"trex" {
                mvex_payload.extend_from_slice(&video_init[c.box_start..c.end]);
            }
        }
        push_trex(video_init, m, &mut mvex_payload, &mut seen_trex)?;
    }
    if let Some(m) = amvex {
        push_trex(audio_init, m, &mut mvex_payload, &mut seen_trex)?;
    }
    if seen_trex.is_empty() {
        return Err("初始化段缺少 mvex/trex".to_string());
    }
    let mut moov_payload: Vec<u8> = Vec::new();
    moov_payload.extend_from_slice(&video_init[mvhd.box_start..mvhd.end]);
    for t in &vtraks {
        moov_payload.extend_from_slice(&video_init[t.box_start..t.end]);
    }
    for t in &atraks {
        moov_payload.extend_from_slice(&audio_init[t.box_start..t.end]);
    }
    moov_payload.extend_from_slice(&wrap(b"mvex", &mvex_payload));
    // 其余视频 moov 子项（如 udta）原样保留在最后。
    for c in &vmc {
        if &c.typ != b"mvhd" && &c.typ != b"trak" && &c.typ != b"mvex" {
            moov_payload.extend_from_slice(&video_init[c.box_start..c.end]);
        }
    }
    let mut out = Vec::new();
    out.extend_from_slice(&video_init[ftyp.box_start..ftyp.end]);
    out.extend_from_slice(&wrap(b"moov", &moov_payload));
    Ok((out, mapping))
}

/// 把一份 init 里指定 track_ID 全改成新 ID：trak 的 tkhd 与 mvex 的
/// trex。只改等于旧 ID 的那几处，别的不动。
fn remap_init_track_ids(init: &mut [u8], old_id: u32, new_id: u32) -> Result<(), String> {
    let moov = top(init, b"moov").ok_or("初始化段缺少 moov")?;
    let mc = children(init, moov.start, moov.end)?;
    for trak in mc.iter().filter(|c| &c.typ == b"trak") {
        let tc = children(init, trak.start, trak.end)?;
        let tkhd = find(&tc, b"tkhd").ok_or("trak 缺少 tkhd")?;
        let version = init.get(tkhd.start).copied().unwrap_or(0);
        let id_off = if version == 1 { 20 } else { 12 };
        if be_u32(init, tkhd.start + id_off).ok_or("tkhd 损坏")? == old_id {
            put_u32(init, tkhd.start + id_off, new_id);
        }
    }
    if let Some(mvex) = find(&mc, b"mvex") {
        for c in children(init, mvex.start, mvex.end)? {
            if &c.typ == b"trex" && be_u32(init, c.start + 4).unwrap_or(0) == old_id {
                put_u32(init, c.start + 4, new_id);
            }
        }
    }
    Ok(())
}

/// 把一个媒体分片里指定 track_ID 全改成新 ID：各 traf 的 tfhd 与
/// 顶层的 sidx（reference_ID）。分片其余字节不动。
pub fn remap_fragment_track_ids(
    fragment: &mut [u8],
    old_id: u32,
    new_id: u32,
) -> Result<(), String> {
    let list = children(fragment, 0, fragment.len())?;
    for c in &list {
        if &c.typ == b"sidx" {
            // sidx 内容：version+flags(4) + reference_ID(4) + timescale(4)…。
            if be_u32(fragment, c.start + 4).ok_or("sidx 损坏")? == old_id {
                put_u32(fragment, c.start + 4, new_id);
            }
        }
        if &c.typ == b"moof" {
            let mc = children(fragment, c.start, c.end)?;
            for traf in mc.iter().filter(|x| &x.typ == b"traf") {
                let tc = children(fragment, traf.start, traf.end)?;
                let tfhd = find(&tc, b"tfhd").ok_or("traf 缺少 tfhd")?;
                if be_u32(fragment, tfhd.start + 4).ok_or("tfhd 损坏")? == old_id {
                    put_u32(fragment, tfhd.start + 4, new_id);
                }
            }
        }
    }
    Ok(())
}

fn put_u32(data: &mut [u8], offset: usize, value: u32) {
    if let Some(slot) = data.get_mut(offset..offset + 4) {
        slot.copy_from_slice(&value.to_be_bytes());
    }
}

/// 从初始化段里读每条轨道的（track_ID，mdhd timescale）。
/// tkhd 与 mdhd 的 version 决定字段偏移：v0 的 creation/modification
/// 各 4 字节，v1 各 8 字节；track_ID / timescale 都在其后。
pub fn track_timescales(init: &[u8]) -> Result<Vec<(u32, u32)>, String> {
    let moov = top(init, b"moov").ok_or("初始化段缺少 moov")?;
    let mc = children(init, moov.start, moov.end)?;
    let mut out = Vec::new();
    for trak in mc.iter().filter(|c| &c.typ == b"trak") {
        let tc = children(init, trak.start, trak.end)?;
        let tkhd = find(&tc, b"tkhd").ok_or("trak 缺少 tkhd")?;
        let version = init.get(tkhd.start).copied().unwrap_or(0);
        let id_off = if version == 1 { 20 } else { 12 };
        let track_id = be_u32(init, tkhd.start + id_off).ok_or("tkhd 损坏")?;
        let mdia = find(&tc, b"mdia").ok_or("trak 缺少 mdia")?;
        let mdc = children(init, mdia.start, mdia.end)?;
        let mdhd = find(&mdc, b"mdhd").ok_or("trak 缺少 mdhd")?;
        let mversion = init.get(mdhd.start).copied().unwrap_or(0);
        let ts_off = if mversion == 1 { 20 } else { 12 };
        let timescale = be_u32(init, mdhd.start + ts_off).ok_or("mdhd 损坏")?;
        if timescale == 0 {
            return Err("轨道 timescale 为 0".to_string());
        }
        out.push((track_id, timescale));
    }
    if out.is_empty() {
        return Err("初始化段里没有轨道".to_string());
    }
    Ok(out)
}

/// 一个媒体分片的解码时间（秒）：取 moof 里各 traf 的 tfdt 最小值，
/// 按各自轨道的 mdhd timescale 换算。要求每个 traf 的 tfhd 都带
/// default-base-is-moof，否则重排后样本偏移会错，直接报错。
pub fn fragment_time(fragment: &[u8], timescales: &[(u32, u32)]) -> Result<f64, String> {
    let moof = top(fragment, b"moof").ok_or("分片缺少 moof")?;
    let mc = children(fragment, moof.start, moof.end)?;
    let mut best: Option<f64> = None;
    for traf in mc.iter().filter(|c| &c.typ == b"traf") {
        let tc = children(fragment, traf.start, traf.end)?;
        let tfhd = find(&tc, b"tfhd").ok_or("traf 缺少 tfhd")?;
        let flags = be_u32(fragment, tfhd.start).ok_or("tfhd 损坏")? & 0x00ff_ffff;
        if flags & 0x020000 == 0 {
            return Err("分片不用 default-base-is-moof，无法重排".to_string());
        }
        let track_id = be_u32(fragment, tfhd.start + 4).ok_or("tfhd 损坏")?;
        let tfdt = find(&tc, b"tfdt").ok_or("traf 缺少 tfdt")?;
        let version = fragment.get(tfdt.start).copied().unwrap_or(0);
        let raw = if version == 1 {
            be_u64(fragment, tfdt.start + 4).ok_or("tfdt 损坏")? as f64
        } else {
            be_u32(fragment, tfdt.start + 4).ok_or("tfdt 损坏")? as f64
        };
        let timescale = timescales
            .iter()
            .find(|(id, _)| *id == track_id)
            .map(|(_, ts)| *ts as f64)
            .ok_or("分片的轨道不在初始化段里")?;
        let t = raw / timescale;
        best = Some(match best {
            Some(b) if b <= t => b,
            _ => t,
        });
    }
    best.ok_or("分片里没有 traf".to_string())
}

/// 两路分片按解码时间交错的顺序：返回（是否视频，路内下标）。
/// 每路内部已按时间排好；时间相同时视频在前。
pub fn interleave_order(video_times: &[f64], audio_times: &[f64]) -> Vec<(bool, usize)> {
    let mut out = Vec::with_capacity(video_times.len() + audio_times.len());
    let (mut vi, mut ai) = (0usize, 0usize);
    while vi < video_times.len() || ai < audio_times.len() {
        let take_video = match (video_times.get(vi), audio_times.get(ai)) {
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (Some(v), Some(a)) => v <= a,
            (None, None) => false,
        };
        if take_video {
            out.push((true, vi));
            vi += 1;
        } else {
            out.push((false, ai));
            ai += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// box(类型，内容)——测试夹具用的最小拼装。
    fn bx(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        wrap(typ, payload)
    }

    /// full box：version(1) + flags(3) + 内容。
    fn full(typ: &[u8; 4], version: u8, flags: u32, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![version, (flags >> 16) as u8, (flags >> 8) as u8, flags as u8];
        p.extend_from_slice(payload);
        bx(typ, &p)
    }

    fn tkhd(track_id: u32) -> Vec<u8> {
        // v0：creation(4) + modification(4) + track_ID(4) + reserved(4) + duration(4)。
        let mut p = vec![0u8; 8];
        p.extend_from_slice(&track_id.to_be_bytes());
        p.extend_from_slice(&[0u8; 8]);
        full(b"tkhd", 0, 0x000007, &p)
    }

    fn mdhd(timescale: u32) -> Vec<u8> {
        // v0：creation(4) + modification(4) + timescale(4) + duration(4) + language(2) + pre_defined(2)。
        let mut p = vec![0u8; 8];
        p.extend_from_slice(&timescale.to_be_bytes());
        p.extend_from_slice(&[0u8; 8]);
        full(b"mdhd", 0, 0, &p)
    }

    fn trak(track_id: u32, timescale: u32) -> Vec<u8> {
        let mut mdia = Vec::new();
        mdia.extend_from_slice(&mdhd(timescale));
        mdia.extend_from_slice(&bx(b"hdlr", &[]));
        bx(b"trak", &[tkhd(track_id), bx(b"mdia", &mdia)].concat())
    }

    fn trex(track_id: u32) -> Vec<u8> {
        // v0 full box：track_ID(4) + 其余默认字段。
        let mut p = track_id.to_be_bytes().to_vec();
        p.extend_from_slice(&[0u8; 16]);
        full(b"trex", 0, 0, &p)
    }

    fn init_with(track_id: u32, timescale: u32) -> Vec<u8> {
        let mut moov = Vec::new();
        moov.extend_from_slice(&full(b"mvhd", 0, 0, &[0u8; 96]));
        moov.extend_from_slice(&trak(track_id, timescale));
        moov.extend_from_slice(&bx(b"mvex", &trex(track_id)));
        [bx(b"ftyp", b"isom"), bx(b"moov", &moov)].concat()
    }

    fn fragment(track_id: u32, tfdt: u64, version: u8) -> Vec<u8> {
        // moof(mfhd + traf(tfhd + tfdt)) + mdat（空内容，只占位）。
        let mut tfhd_p = track_id.to_be_bytes().to_vec();
        let tfhd = full(b"tfhd", 0, 0x020000, &tfhd_p);
        let tfdt_p: Vec<u8> = if version == 1 {
            tfdt.to_be_bytes().to_vec()
        } else {
            (tfdt as u32).to_be_bytes().to_vec()
        };
        let tfdt_b = full(b"tfdt", version, 0, &tfdt_p);
        let traf = bx(b"traf", &[tfhd, tfdt_b].concat());
        let moof = bx(b"moof", &[bx(b"mfhd", &[0u8; 4]), traf].concat());
        [moof, bx(b"mdat", &[])].concat()
    }

    #[test]
    fn merge_keeps_both_tracks() {
        let v = init_with(1, 90000);
        let a = init_with(2, 48000);
        let (merged, mapping) = merge_inits(&v, &a).unwrap();
        assert!(mapping.is_empty(), "轨道号本来就不同，不用换号");
        let ts = track_timescales(&merged).unwrap();
        assert_eq!(ts, vec![(1, 90000), (2, 48000)]);
        // 两路的 trex 都在合并后的 mvex 里。
        let moov = top(&merged, b"moov").unwrap();
        let mc = children(&merged, moov.start, moov.end).unwrap();
        let mvex = find(&mc, b"mvex").unwrap();
        let xc = children(&merged, mvex.start, mvex.end).unwrap();
        assert_eq!(xc.iter().filter(|c| &c.typ == b"trex").count(), 2);
    }

    #[test]
    fn merge_remaps_colliding_audio_track_id() {
        // 真实打包常两路都从 1 开始：视频轨 1@90000，音频轨 1@48000。
        let v = init_with(1, 90000);
        let a = init_with(1, 48000);
        let (merged, mapping) = merge_inits(&v, &a).unwrap();
        assert_eq!(mapping, vec![(1, 2)]);
        let ts = track_timescales(&merged).unwrap();
        assert_eq!(ts, vec![(1, 90000), (2, 48000)]);
        // 音频分片按旧 ID 1 写，tfdt 96000 @48000 = 2.0s；换号后再算
        // 时间必须用音频的 timescale，错用视频的会算成 96000/90000。
        let mut frag = fragment(1, 96000, 0);
        // 再给分片加一个 sidx（reference_ID 同样是 1），换号须一起改。
        let mut sidx_payload = 1u32.to_be_bytes().to_vec();
        sidx_payload.extend_from_slice(&[0u8; 14]);
        let sidx = full(b"sidx", 0, 0, &sidx_payload);
        frag.splice(0..0, sidx);
        remap_fragment_track_ids(&mut frag, 1, 2).unwrap();
        assert!((fragment_time(&frag, &ts).unwrap() - 2.0).abs() < 1e-9);
        let list = children(&frag, 0, frag.len()).unwrap();
        let s = find(&list, b"sidx").unwrap();
        assert_eq!(be_u32(&frag, s.start + 4).unwrap(), 2);
    }

    #[test]
    fn merge_rejects_missing_moov() {
        let v = init_with(1, 90000);
        assert!(merge_inits(&v, b"not a box at all........").is_err());
        assert!(merge_inits(b"junkjunkjunkjunk", &v).is_err());
    }

    #[test]
    fn fragment_time_scales_by_track() {
        let ts = vec![(1u32, 90000u32), (2u32, 48000u32)];
        // 视频轨 tfdt 180000 @90000 = 2.0s；音频轨 tfdt 96000 @48000 = 2.0s。
        assert!((fragment_time(&fragment(1, 180000, 0), &ts).unwrap() - 2.0).abs() < 1e-9);
        assert!((fragment_time(&fragment(2, 96000, 0), &ts).unwrap() - 2.0).abs() < 1e-9);
        // v1 的 64 位 tfdt。
        assert!((fragment_time(&fragment(1, 90000, 1), &ts).unwrap() - 1.0).abs() < 1e-9);
        // 未知轨道、缺 moof、不用 default-base-is-moof 都报错。
        assert!(fragment_time(&fragment(9, 0, 0), &ts).is_err());
        assert!(fragment_time(b"no moof here", &ts).is_err());
        let mut tfhd_p = 3u32.to_be_bytes().to_vec();
        let bad = {
            let tfhd = full(b"tfhd", 0, 0x000000, &tfhd_p);
            let tfdt_b = full(b"tfdt", 0, 0, &(0u32).to_be_bytes());
            let traf = bx(b"traf", &[tfhd, tfdt_b].concat());
            bx(b"moof", &[bx(b"mfhd", &[0u8; 4]), traf].concat())
        };
        assert!(fragment_time(&bad, &ts).is_err());
        let _ = &mut tfhd_p;
    }

    #[test]
    fn interleave_orders_by_time_video_first_on_ties() {
        let order = interleave_order(&[0.0, 2.0, 4.0], &[1.0, 3.0]);
        assert_eq!(
            order,
            vec![(true, 0), (false, 0), (true, 1), (false, 1), (true, 2)]
        );
        // 同一时刻视频在前。
        assert_eq!(
            interleave_order(&[1.0], &[1.0]),
            vec![(true, 0), (false, 0)]
        );
        assert_eq!(interleave_order(&[], &[0.5]), vec![(false, 0)]);
        assert!(interleave_order(&[], &[]).is_empty());
    }
}
