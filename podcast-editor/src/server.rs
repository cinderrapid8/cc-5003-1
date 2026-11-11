//! HTTP 路由与处理器。前端为静态单页（static/index.html），后端只提供 JSON/文件 API。

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Multipart, Path as AxumPath, Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use tower_http::services::ServeDir;

use crate::domain::{
    frames_to_rate, project_frame_to_source, EditGroup, Item, Project, Source, Track,
};
use crate::edit;
use crate::export::export_wav;
use crate::link::{self, ZeroPolicy};
use crate::render::{render, CancelToken};
use crate::state::{versioned, AppState, Versioned};
use crate::wav::write_wav;
use crate::waveform::peaks;

type ApiError = (StatusCode, String);
fn err<E: std::fmt::Display>(e: E) -> ApiError {
    (StatusCode::BAD_REQUEST, e.to_string())
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/projects", post(create_project).get(list_projects))
        .route("/api/projects/{pid}", get(load_project))
        .route("/api/projects/{pid}/sources", post(upload_source).get(list_sources))
        .route("/api/projects/{pid}/sources/register", post(register_source))
        .route("/api/projects/{pid}/items", post(add_item).get(get_items))
        .route("/api/projects/{pid}/items/delete", post(delete_item))
        .route("/api/projects/{pid}/trim", post(trim))
        .route("/api/projects/{pid}/split", post(split))
        .route("/api/projects/{pid}/reorder", post(reorder))
        .route("/api/projects/{pid}/crossfade", post(crossfade))
        .route("/api/projects/{pid}/silence", post(add_silence))
        .route("/api/projects/{pid}/undo", post(undo))
        .route("/api/projects/{pid}/redo", post(redo))
        .route("/api/projects/{pid}/versions", get(versions))
        .route("/api/projects/{pid}/render", get(render_preview))
        .route("/api/projects/{pid}/waveform", get(waveform))
        .route("/api/projects/{pid}/locate", get(locate))
        .route("/api/projects/{pid}/export", post(do_export))
        .route("/api/projects/{pid}/export/stems", post(do_export_stems))
        .route("/api/projects/{pid}/export/cancel", post(cancel_export))
        .route("/api/projects/{pid}/tracks", get(list_tracks))
        .route("/api/projects/{pid}/tracks/{tid}", post(update_track))
        .route("/api/projects/{pid}/groups", get(list_groups).post(create_group))
        .route("/api/projects/{pid}/groups/{gid}/items", post(group_add_items))
        .route("/api/projects/{pid}/groups/{gid}/parent", post(set_group_parent))
        .route("/api/projects/{pid}/groups/{gid}/move", post(move_group))
        .route("/api/projects/{pid}/groups/{gid}/crossfade", post(group_crossfade))
        .route("/api/projects/{pid}/groups/status", get(group_status))
        .route("/api/test-signal", post(make_test_signal))
        .fallback_service(ServeDir::new("static"))
        .with_state(state)
}

// ---------- 项目 ----------

#[derive(Deserialize)]
struct CreateReq {
    name: String,
    sample_rate: u32,
    channels: u16,
}

async fn create_project(
    State(st): State<Arc<AppState>>,
    Json(req): Json<CreateReq>,
) -> Result<Json<Project>, ApiError> {
    let p = Project {
        id: edit::new_id("proj"),
        name: req.name,
        sample_rate: req.sample_rate,
        channels: req.channels,
        version: 1,
        tracks: Project::default_tracks(),
        groups: vec![],
        items: vec![],
        silences: vec![],
    };
    st.db.create_project(&p).map_err(err)?;
    st.put_sources(&p.id, HashMap::new());
    Ok(Json(p))
}

async fn list_projects(State(st): State<Arc<AppState>>) -> Json<Vec<String>> {
    Json(st.db.list_project_ids().unwrap_or_default())
}

async fn load_project(
    State(st): State<Arc<AppState>>,
    AxumPath(pid): AxumPath<String>,
) -> Result<Json<Project>, ApiError> {
    let _lk = st.lock_for(&pid);
    let _g = _lk.lock().unwrap();
    let p = st.db.load_project(&pid).map_err(err)?;
    let metas = st.db.list_sources(&pid).map_err(err)?;
    let map: HashMap<String, Source> = metas.into_iter().map(|s| (s.id.clone(), s)).collect();
    st.put_sources(&pid, map);
    Ok(Json(p))
}

// ---------- 素材 ----------

#[axum::debug_handler]
async fn upload_source(
    State(st): State<Arc<AppState>>,
    AxumPath(pid): AxumPath<String>,
    mut mp: Multipart,
) -> Result<Json<Source>, ApiError> {
    // 先在无锁状态完成 body 读取（await 期间不能持有非 Send 的 std 守卫）。
    let field = mp
        .next_field()
        .await
        .map_err(err)?
        .ok_or_else(|| err("缺少上传文件字段"))?;
    let original_name = field.file_name().unwrap_or("upload.wav").to_string();
    let bytes = field.bytes().await.map_err(err)?.to_vec();

    // 解析（不信任上传内容）。
    let wav = crate::wav::parse_wav(&bytes).map_err(err)?;
    let source_id = edit::new_id("src");
    // 以内容生成只读文件名，绝不覆盖任何已有文件或用户原录音。
    let safe: String = original_name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let stored = format!("{}/{}_{}", st.data_dir, source_id, safe);

    // 仅在同步落库阶段持锁。
    let _lk = st.lock_for(&pid);
    let _g = _lk.lock().unwrap();
    std::fs::write(&stored, &bytes).map_err(err)?;
    let meta = Source {
        id: source_id.clone(),
        path: stored,
        original_rate: wav.sample_rate,
        channels: wav.channels,
        frame_count: wav.frames() as i64,
    };
    st.db.upsert_source(&meta, &pid).map_err(err)?;
    st.loaded.insert(source_id.clone(), Arc::new(wav));
    let mut map = st.sources_meta(&pid);
    map.insert(source_id.clone(), meta.clone());
    st.put_sources(&pid, map);
    Ok(Json(meta))
}

async fn list_sources(
    State(st): State<Arc<AppState>>,
    AxumPath(pid): AxumPath<String>,
) -> Json<Vec<Source>> {
    Json(st.db.list_sources(&pid).unwrap_or_default())
}

#[derive(Deserialize)]
struct RegisterReq {
    path: String,
    #[serde(default)]
    name: Option<String>,
}

/// 登记数据目录内由 /api/test-signal 生成的 WAV（仍只读、仍生成独立副本名）。
/// 拒绝目录外路径，避免任意文件读取。
async fn register_source(
    State(st): State<Arc<AppState>>,
    AxumPath(pid): AxumPath<String>,
    Json(req): Json<RegisterReq>,
) -> Result<Json<Source>, ApiError> {
    let data_dir = std::fs::canonicalize(&st.data_dir).map_err(err)?;
    let canon = std::fs::canonicalize(&req.path).map_err(err)?;
    if !canon.starts_with(&data_dir) {
        return Err(err("只允许登记数据目录内的文件"));
    }
    let _lk = st.lock_for(&pid);
    let _g = _lk.lock().unwrap();
    let wav = crate::wav::read_wav(canon.to_str().unwrap()).map_err(err)?;
    let source_id = edit::new_id("src");
    let meta = Source {
        id: source_id.clone(),
        path: canon.to_string_lossy().to_string(),
        original_rate: wav.sample_rate,
        channels: wav.channels,
        frame_count: wav.frames() as i64,
    };
    st.db.upsert_source(&meta, &pid).map_err(err)?;
    st.loaded.insert(source_id.clone(), Arc::new(wav));
    let mut map = st.sources_meta(&pid);
    map.insert(source_id.clone(), meta.clone());
    st.put_sources(&pid, map);
    let _ = req.name;
    Ok(Json(meta))
}

// ---------- 编辑 ----------

#[derive(Deserialize)]
struct AddItemReq {
    source_id: String,
    source_start: i64,
    source_end: i64,
    /// 目标轨道；缺省时旧单轨工程落到 legacy 轨，新工程落到主持人轨。
    #[serde(default)]
    track_id: Option<String>,
    /// 显式工程帧起点；缺省时追加到该轨当前最后一条片段之后。
    #[serde(default)]
    timeline_start: Option<i64>,
}

async fn add_item(
    State(st): State<Arc<AppState>>,
    AxumPath(pid): AxumPath<String>,
    Json(req): Json<AddItemReq>,
) -> Result<Json<Project>, ApiError> {
    let _lk = st.lock_for(&pid);
    let _g = _lk.lock().unwrap();
    let mut p = st.db.load_project(&pid).map_err(err)?;
    let map = st.sources_meta(&pid);
    let src = map
        .get(&req.source_id)
        .ok_or_else(|| err("素材不存在"))?;
    if req.source_start < 0 || req.source_end > src.frame_count || req.source_end <= req.source_start
    {
        return Err(err("引用范围超出素材或为空"));
    }
    let track_id = match req.track_id {
        Some(t) => {
            if p.track(&t).is_none() {
                return Err(err(format!("轨道 {t} 不存在")));
            }
            t
        }
        None => {
            if p.tracks.len() == 1 && p.tracks[0].role == "legacy" {
                crate::domain::TRACK_LEGACY.to_string()
            } else {
                crate::domain::TRACK_HOST.to_string()
            }
        }
    };
    if p.is_locked(&track_id) {
        return Err(err(format!("轨道 {track_id} 已锁定，不能加入片段")));
    }
    let rates = st.source_rate_map(&pid);
    let pos = if let Some(pos) = req.timeline_start {
        pos
    } else {
        let last = p
            .items
            .iter()
            .filter(|i| i.track_id == track_id)
            .map(|i| link::item_end(i, &rates, p.sample_rate))
            .max()
            .unwrap_or_else(|| {
                p.track(&track_id).map(|t| t.start_offset).unwrap_or(0).max(0)
            });
        last
    };
    p.items.push(Item {
        id: edit::new_id("item"),
        source_id: req.source_id,
        track_id,
        source_start: req.source_start,
        source_end: req.source_end,
        timeline_start: pos,
        crossfade_out: 0,
    });
    p.version += 1;
    st.db.save_version(&p).map_err(err)?;
    Ok(Json(p))
}

async fn get_items(
    State(st): State<Arc<AppState>>,
    AxumPath(pid): AxumPath<String>,
) -> Result<Json<Versioned<Vec<Item>>>, ApiError> {
    let p = st.db.load_project(&pid).map_err(err)?;
    Ok(Json(versioned(&p, p.items.clone())))
}

#[derive(Deserialize)]
struct TrimReq {
    item_id: String,
    source_start: i64,
    source_end: i64,
}

async fn trim(
    State(st): State<Arc<AppState>>,
    AxumPath(pid): AxumPath<String>,
    Json(req): Json<TrimReq>,
) -> Result<Json<Project>, ApiError> {
    let _lk = st.lock_for(&pid);
    let _g = _lk.lock().unwrap();
    let p = st.db.load_project(&pid).map_err(err)?;
    // 单轨修剪：只改引用范围，不重排时间线，其他轨道工程位置原样保留。
    let mut p = link::trim_item(p, &req.item_id, req.source_start, req.source_end).map_err(err)?;
    p.version += 1;
    st.db.save_version(&p).map_err(err)?;
    Ok(Json(p))
}

#[derive(Deserialize)]
struct SplitReq {
    item_id: String,
    at_source_frame: i64,
}

async fn split(
    State(st): State<Arc<AppState>>,
    AxumPath(pid): AxumPath<String>,
    Json(req): Json<SplitReq>,
) -> Result<Json<Project>, ApiError> {
    let _lk = st.lock_for(&pid);
    let _g = _lk.lock().unwrap();
    let p = st.db.load_project(&pid).map_err(err)?;
    let rates = st.source_rate_map(&pid);
    let mut p = link::split_item(p, &rates, &req.item_id, req.at_source_frame).map_err(err)?;
    p.version += 1;
    st.db.save_version(&p).map_err(err)?;
    Ok(Json(p))
}

#[derive(Deserialize)]
struct ReorderReq {
    item_id: String,
    target_index: usize,
}

async fn reorder(
    State(st): State<Arc<AppState>>,
    AxumPath(pid): AxumPath<String>,
    Json(req): Json<ReorderReq>,
) -> Result<Json<Project>, ApiError> {
    let _lk = st.lock_for(&pid);
    let _g = _lk.lock().unwrap();
    let mut p = st.db.load_project(&pid).map_err(err)?;
    let pos = p
        .items
        .iter()
        .position(|i| i.id == req.item_id)
        .ok_or_else(|| err(format!("片段 {} 不存在", req.item_id)))?;
    if p.is_locked(&p.items[pos].track_id) {
        return Err(err(format!(
            "轨道 {} 已锁定，不能调整顺序",
            p.items[pos].track_id
        )));
    }
    // 仅在同一轨道内调整 Vec 顺序；时间线位置保持各自绝对值，不触发全局重排。
    let track_id = p.items[pos].track_id.clone();
    let track_positions: Vec<usize> = p
        .items
        .iter()
        .enumerate()
        .filter(|(_, i)| i.track_id == track_id)
        .map(|(idx, _)| idx)
        .collect();
    if req.target_index >= track_positions.len() {
        return Err(err(format!(
            "目标索引 {} 越界（该轨共 {} 条片段）",
            req.target_index,
            track_positions.len()
        )));
    }
    let item = p.items.remove(pos);
    let new_track_positions: Vec<usize> = p
        .items
        .iter()
        .enumerate()
        .filter(|(_, i)| i.track_id == track_id)
        .map(|(idx, _)| idx)
        .collect();
    let insert_at = new_track_positions
        .get(req.target_index)
        .copied()
        .unwrap_or(p.items.len());
    p.items.insert(insert_at, item);
    p.version += 1;
    st.db.save_version(&p).map_err(err)?;
    Ok(Json(p))
}

#[derive(Deserialize)]
struct XfReq {
    item_id: String,
    frames: i64,
}

async fn crossfade(
    State(st): State<Arc<AppState>>,
    AxumPath(pid): AxumPath<String>,
    Json(req): Json<XfReq>,
) -> Result<Json<CrossfadeResp>, ApiError> {
    let _lk = st.lock_for(&pid);
    let _g = _lk.lock().unwrap();
    let p = st.db.load_project(&pid).map_err(err)?;
    if p.is_locked(
        &p.items
            .iter()
            .find(|i| i.id == req.item_id)
            .map(|i| i.track_id.clone())
            .unwrap_or_default(),
    ) {
        return Err(err("该片段所在轨道已锁定"));
    }
    // 同轨相邻才构成淡化对：按轨道分组排序后取下一条。
    let map = st.sources_meta(&pid);
    let margin = {
        let cur = p.items.iter().find(|i| i.id == req.item_id);
        let next = cur.and_then(|c| {
            p.items
                .iter()
                .filter(|i| i.track_id == c.track_id && i.timeline_start > c.timeline_start)
                .min_by_key(|i| i.timeline_start)
        });
        match (cur, next) {
            (Some(c), Some(n)) => {
                let mut candidate = c.clone();
                candidate.crossfade_out = req.frames;
                Some(crate::domain::check_crossfade(
                    &candidate,
                    n,
                    &map,
                    p.sample_rate,
                ))
            }
            _ => None,
        }
    };

    let mut p = edit::set_crossfade(p, &req.item_id, req.frames).map_err(err)?;
    p.version += 1;
    st.db.save_version(&p).map_err(err)?;
    Ok(Json(CrossfadeResp {
        project: p,
        margin,
    }))
}

#[derive(serde::Serialize)]
struct CrossfadeResp {
    project: Project,
    margin: Option<crate::domain::CrossfadeMargin>,
}

#[derive(Deserialize)]
struct SilenceReq {
    length: i64,
}

async fn add_silence(
    State(st): State<Arc<AppState>>,
    AxumPath(pid): AxumPath<String>,
    Json(req): Json<SilenceReq>,
) -> Result<Json<Project>, ApiError> {
    let _lk = st.lock_for(&pid);
    let _g = _lk.lock().unwrap();
    let mut p = st.db.load_project(&pid).map_err(err)?;
    let rates = st.source_rate_map(&pid);
    let start = crate::domain::timeline_length(&p, &rates);
    p = edit::append_silence(p, req.length).map_err(err)?;
    if let Some(s) = p.silences.last_mut() {
        s.timeline_start = start;
    }
    p.version += 1;
    st.db.save_version(&p).map_err(err)?;
    Ok(Json(p))
}

// ---------- 撤销 / 重做（快照版本切换） ----------

async fn undo(
    State(st): State<Arc<AppState>>,
    AxumPath(pid): AxumPath<String>,
) -> Result<Json<Project>, ApiError> {
    let _lk = st.lock_for(&pid);
    let _g = _lk.lock().unwrap();
    let cur = st.db.load_project(&pid).map_err(err)?;
    let versions = st.db.list_versions(&pid).map_err(err)?;
    let prev = versions
        .iter()
        .filter(|&&v| v < cur.version)
        .max()
        .copied()
        .ok_or_else(|| err("没有更早的版本可撤销"))?;
    let p = st.db.restore_version(&pid, prev).map_err(err)?;
    Ok(Json(p))
}

async fn redo(
    State(st): State<Arc<AppState>>,
    AxumPath(pid): AxumPath<String>,
) -> Result<Json<Project>, ApiError> {
    let _lk = st.lock_for(&pid);
    let _g = _lk.lock().unwrap();
    let cur = st.db.load_project(&pid).map_err(err)?;
    let versions = st.db.list_versions(&pid).map_err(err)?;
    let next = versions
        .iter()
        .filter(|&&v| v > cur.version)
        .min()
        .copied()
        .ok_or_else(|| err("没有更新的版本可重做"))?;
    let p = st.db.restore_version(&pid, next).map_err(err)?;
    Ok(Json(p))
}

async fn versions(
    State(st): State<Arc<AppState>>,
    AxumPath(pid): AxumPath<String>,
) -> Json<Vec<i64>> {
    Json(st.db.list_versions(&pid).unwrap_or_default())
}

// ---------- 渲染 / 波形 / 定位 ----------

fn collect_loaded(
    st: &AppState,
    pid: &str,
) -> Result<(HashMap<String, Source>, crate::render::LoadedSources), ApiError> {
    let metas = st.db.list_sources(pid).map_err(err)?;
    let mut meta_map = HashMap::new();
    let mut loaded = crate::render::LoadedSources::new();
    for m in metas {
        // 读取失败（源文件丢失/损坏）不在此抛裸 IO 错误：保留元数据、跳过加载，
        // 交给 render::preflight 产出携带 item_id 与原因的结构化「缺失」错误。
        if let Ok(w) = st.load_source(&m.id, &m) {
            loaded.insert(m.id.clone(), w);
        }
        meta_map.insert(m.id.clone(), m);
    }
    Ok((meta_map, loaded))
}

#[derive(Deserialize)]
struct RenderQuery {
    #[serde(default)]
    since_version: Option<i64>,
}

/// 预览渲染：与导出共用 render::render。响应带版本戳；
/// 若请求方声明的 since_version 已落后，前端应丢弃并用新版本重取。
async fn render_preview(
    State(st): State<Arc<AppState>>,
    AxumPath(pid): AxumPath<String>,
    Query(q): Query<RenderQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let p = st.db.load_project(&pid).map_err(err)?;
    if let Some(sv) = q.since_version {
        if sv > p.version {
            return Err((StatusCode::CONFLICT, "客户端版本比工程更新".into()));
        }
    }
    let (metas, loaded) = collect_loaded(&st, &pid)?;
    // 重活在阻塞线程池跑，避免卡住异步运行时。
    let pr = p.sample_rate;
    let ch = p.channels;
    let version = p.version; // 渲染所依据的版本，随响应返回，不再回库重读。
    let rendered = tokio::task::spawn_blocking(move || render(&p, &metas, &loaded, None))
        .await
        .map_err(|e| err(e))?
        .map_err(err)?;
    Ok(Json(serde_json::json!({
        "version": version,
        "sample_rate": pr,
        "channels": ch,
        "frame_count": rendered.len() / ch as usize,
        "frames": rendered,
    })))
}

#[derive(Deserialize)]
struct WaveQuery {
    bucket_frames: usize,
    #[serde(default)]
    source_id: Option<String>,
    #[serde(default)]
    since_version: Option<i64>,
}

/// 波形峰值。可针对整条时间线渲染结果，或针对单个源素材（原素材视图）。
async fn waveform(
    State(st): State<Arc<AppState>>,
    AxumPath(pid): AxumPath<String>,
    Query(q): Query<WaveQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let version_now = st.db.load_project(&pid).map(|x| x.version).unwrap_or(-1);
    let buckets = if let Some(sid) = q.source_id.clone() {
        // 源素材波形（原始采样率，按源帧分桶）。
        let metas = st.db.list_sources(&pid).map_err(err)?;
        let meta = metas
            .into_iter()
            .find(|m| m.id == sid)
            .ok_or_else(|| err("素材不存在"))?;
        let w = st.load_source(&sid, &meta).map_err(err)?;
        peaks(&w.samples, w.channels as usize, q.bucket_frames)
    } else {
        let p = st.db.load_project(&pid).map_err(err)?;
        let (metas, loaded) = collect_loaded(&st, &pid)?;
        let ch = p.channels;
        let rendered = tokio::task::spawn_blocking(move || render(&p, &metas, &loaded, None))
            .await
            .map_err(err)?
            .map_err(err)?;
        peaks(&rendered, ch as usize, q.bucket_frames)
    };
    // 计算完成后再次读取版本：若期间发生编辑/撤销，版本已变，标记 stale。
    let version_after = st.db.load_project(&pid).map(|x| x.version).unwrap_or(-1);
    let stale = q.since_version.is_some_and(|sv| sv != version_after)
        || version_now != version_after;
    Ok(Json(serde_json::json!({
        "version": version_after,
        "stale": stale,
        "bucket_frames": q.bucket_frames,
        "buckets": buckets,
    })))
}

#[derive(Deserialize)]
struct LocateQuery {
    /// 方向：timeline->source 或 source->timeline。
    direction: String,
    frame: i64,
    #[serde(default)]
    source_id: Option<String>,
    #[serde(default)]
    item_id: Option<String>,
}

/// 试听游标 <-> 原素材位置 互相定位。全部用采样帧整数换算。
async fn locate(
    State(st): State<Arc<AppState>>,
    AxumPath(pid): AxumPath<String>,
    Query(q): Query<LocateQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let p = st.db.load_project(&pid).map_err(err)?;
    let map = st.sources_meta(&pid);
    match q.direction.as_str() {
        // 工程时间线帧 -> 所在片段的源帧。
        "timeline->source" | "to_source" => {
            let item_id = q.item_id.ok_or_else(|| err("需要 item_id"))?;
            let it = p
                .items
                .iter()
                .find(|i| i.id == item_id)
                .ok_or_else(|| err("片段不存在"))?;
            let src = map
                .get(&it.source_id)
                .ok_or_else(|| err("素材元数据缺失"))?;
            // 工程帧减去片段起点得到片段内局部工程帧，再换算到源帧并加回引用起点。
            let local_project = (q.frame - it.timeline_start).max(0);
            let source_frame =
                project_frame_to_source(local_project, src.original_rate, p.sample_rate)
                    + it.source_start;
            Ok(Json(serde_json::json!({
                "source_id": it.source_id,
                "source_frame": source_frame,
                "project_frame": q.frame,
            })))
        }
        // 源帧 -> 工程时间线帧。
        "source->timeline" | "to_timeline" => {
            let sid = q.source_id.ok_or_else(|| err("需要 source_id"))?;
            let src = map.get(&sid).ok_or_else(|| err("素材不存在"))?;
            // 找到引用该源、且包含该源帧的片段。
            let it = p
                .items
                .iter()
                .find(|i| {
                    i.source_id == sid && q.frame >= i.source_start && q.frame < i.source_end
                })
                .ok_or_else(|| err("没有片段引用该源帧位置"))?;
            let local_source = q.frame - it.source_start;
            let local_project =
                frames_to_rate(local_source, src.original_rate, p.sample_rate);
            Ok(Json(serde_json::json!({
                "item_id": it.id,
                "project_frame": it.timeline_start + local_project,
                "source_frame": q.frame,
            })))
        }
        other => Err(err(format!("未知 direction {other}"))),
    }
}

// ---------- 导出 ----------

#[derive(Deserialize)]
struct ExportReq {
    dest: String,
}

async fn do_export(
    State(st): State<Arc<AppState>>,
    AxumPath(pid): AxumPath<String>,
    Json(req): Json<ExportReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let key = format!("{pid}:{}", edit::new_id("job"));
    let token = Arc::new(CancelToken::default());
    // 若取消请求先于本任务到达，直接继承取消意图。
    if st.pending_cancels.remove(&pid).is_some() {
        token.cancel();
    }
    st.exports.insert(key.clone(), token.clone());

    let p = st.db.load_project(&pid).map_err(err)?;
    let (metas, loaded) = collect_loaded(&st, &pid)?;
    let db_path_for_task = st.data_dir.clone();
    let pid2 = pid.clone();
    let dest = req.dest.clone();
    let st2 = st.clone();

    let outcome = tokio::task::spawn_blocking(move || {
        // 栅栏：给「任务注册后、渲染开始前」到达的取消一个可介入窗口，
        // 同时已取消的任务在此立即短路。
        for _ in 0..5 {
            if token.is_cancelled() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // 导出函数内部保证取消/失败清理临时文件。
        let db = crate::db::Db::open(&format!("{db_path_for_task}/editor.sqlite")).ok();
        export_wav(&p, &metas, &loaded, &dest, token, db.as_ref())
    })
    .await
    .map_err(|e| err(e))?;

    st.exports.remove(&key);
    match outcome {
        Ok(o) => {
            if !o.cancelled {
                // 正常完成，清除可能残留的取消意图，避免污染后续导出。
                st.pending_cancels.remove(&pid);
            }
            Ok(Json(serde_json::json!({
                "cancelled": o.cancelled,
                "path": o.path,
                "frames": o.frames,
            })))
        }
        Err(e) => {
            let _ = st2.db.log_export(&pid2, &req.dest, -1, "failed");
            Err(err(e))
        }
    }
}

/// 分轨导出：混音 + 每轨 WAV，全部引用同一工程修订（project.version）。
async fn do_export_stems(
    State(st): State<Arc<AppState>>,
    AxumPath(pid): AxumPath<String>,
    Json(req): Json<ExportReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let key = format!("{pid}:{}", edit::new_id("job"));
    let token = Arc::new(CancelToken::default());
    if st.pending_cancels.remove(&pid).is_some() {
        token.cancel();
    }
    st.exports.insert(key.clone(), token.clone());

    let p = st.db.load_project(&pid).map_err(err)?;
    let (metas, loaded) = collect_loaded(&st, &pid)?;
    let db_path_for_task = st.data_dir.clone();
    let dest = req.dest.clone();

    let outcome = tokio::task::spawn_blocking(move || {
        for _ in 0..5 {
            if token.is_cancelled() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let db = crate::db::Db::open(&format!("{db_path_for_task}/editor.sqlite")).ok();
        crate::export::export_stems(&p, &metas, &loaded, &dest, token, db.as_ref())
    })
    .await
    .map_err(|e| err(e))?;

    st.exports.remove(&key);
    match outcome {
        Ok(o) => Ok(Json(serde_json::json!({
            "cancelled": o.cancelled,
            "version": o.version,
            "mix_path": o.mix_path,
            "stems": o.stems.iter().map(|(tid, path, frames)| serde_json::json!({
                "track_id": tid, "path": path, "frames": frames,
            })).collect::<Vec<_>>(),
        }))),
        Err(e) => Err(err(e)),
    }
}

async fn cancel_export(
    State(st): State<Arc<AppState>>,
    AxumPath(pid): AxumPath<String>,
) -> Json<serde_json::Value> {    let mut n = 0;
    st.exports.retain(|k, t| {
        if k.starts_with(&format!("{pid}:")) {
            t.cancel();
            n += 1;
            false
        } else {
            true
        }
    });
    // 无论任务是否已注册，都登记取消意图，覆盖「取消先于任务启动」的竞态。
    st.pending_cancels.insert(pid.clone(), ());
    // 至少有一个取消意图被记录即视为成功。
    let jobs = n.max(1);
    Json(serde_json::json!({ "cancelled_jobs": jobs }))
}

// ---------- 多轨：轨道管理 ----------

async fn list_tracks(
    State(st): State<Arc<AppState>>,
    AxumPath(pid): AxumPath<String>,
) -> Result<Json<Vec<Track>>, ApiError> {
    let p = st.db.load_project(&pid).map_err(err)?;
    Ok(Json(p.tracks))
}

#[derive(Deserialize)]
struct TrackReq {
    #[serde(default)]
    locked: Option<bool>,
    #[serde(default)]
    hidden: Option<bool>,
    #[serde(default)]
    start_offset: Option<i64>,
}

async fn update_track(
    State(st): State<Arc<AppState>>,
    AxumPath((pid, tid)): AxumPath<(String, String)>,
    Json(req): Json<TrackReq>,
) -> Result<Json<Project>, ApiError> {
    let _lk = st.lock_for(&pid);
    let _g = _lk.lock().unwrap();
    let mut p = st.db.load_project(&pid).map_err(err)?;
    if let Some(v) = req.locked {
        p = link::set_track_locked(p, &tid, v).map_err(err)?;
    }
    if let Some(v) = req.hidden {
        p = link::set_track_hidden(p, &tid, v).map_err(err)?;
    }
    if let Some(v) = req.start_offset {
        p = link::set_track_offset(p, &tid, v).map_err(err)?;
    }
    p.version += 1;
    st.db.save_version(&p).map_err(err)?;
    Ok(Json(p))
}

// ---------- 多轨：剪辑组 ----------

async fn list_groups(
    State(st): State<Arc<AppState>>,
    AxumPath(pid): AxumPath<String>,
) -> Result<Json<Vec<EditGroup>>, ApiError> {
    let p = st.db.load_project(&pid).map_err(err)?;
    Ok(Json(p.groups))
}

#[derive(Deserialize)]
struct CreateGroupReq {
    name: String,
    item_ids: Vec<String>,
    #[serde(default)]
    parent_id: Option<String>,
}

async fn create_group(
    State(st): State<Arc<AppState>>,
    AxumPath(pid): AxumPath<String>,
    Json(req): Json<CreateGroupReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let _lk = st.lock_for(&pid);
    let _g = _lk.lock().unwrap();
    let mut p = st.db.load_project(&pid).map_err(err)?;
    let (p2, gid) =
        link::create_group(p, &req.name, req.item_ids, req.parent_id).map_err(err)?;
    p = p2;
    p.version += 1;
    st.db.save_version(&p).map_err(err)?;
    Ok(Json(serde_json::json!({ "group_id": gid, "project": p })))
}

#[derive(Deserialize)]
struct GroupItemsReq {
    item_ids: Vec<String>,
}

async fn group_add_items(
    State(st): State<Arc<AppState>>,
    AxumPath((pid, gid)): AxumPath<(String, String)>,
    Json(req): Json<GroupItemsReq>,
) -> Result<Json<Project>, ApiError> {
    let _lk = st.lock_for(&pid);
    let _g = _lk.lock().unwrap();
    let mut p = st.db.load_project(&pid).map_err(err)?;
    p = link::add_items_to_group(p, &gid, req.item_ids).map_err(err)?;
    p.version += 1;
    st.db.save_version(&p).map_err(err)?;
    Ok(Json(p))
}

#[derive(Deserialize)]
struct GroupParentReq {
    #[serde(default)]
    parent_id: Option<String>,
}

async fn set_group_parent(
    State(st): State<Arc<AppState>>,
    AxumPath((pid, gid)): AxumPath<(String, String)>,
    Json(req): Json<GroupParentReq>,
) -> Result<Json<Project>, ApiError> {
    let _lk = st.lock_for(&pid);
    let _g = _lk.lock().unwrap();
    let mut p = st.db.load_project(&pid).map_err(err)?;
    p = link::set_group_parent(p, &gid, req.parent_id).map_err(err)?;
    p.version += 1;
    st.db.save_version(&p).map_err(err)?;
    Ok(Json(p))
}

#[derive(Deserialize)]
struct GroupMoveReq {
    delta: i64,
    #[serde(default = "default_policy")]
    zero_policy: String,
}

fn default_policy() -> String {
    "reject".to_string()
}

fn parse_policy(s: &str) -> Result<ZeroPolicy, ApiError> {
    match s {
        "reject" => Ok(ZeroPolicy::Reject),
        "clip" => Ok(ZeroPolicy::Clip),
        other => Err(err(format!("未知零点策略 {other}（允许 reject / clip）"))),
    }
}

async fn move_group(
    State(st): State<Arc<AppState>>,
    AxumPath((pid, gid)): AxumPath<(String, String)>,
    Json(req): Json<GroupMoveReq>,
) -> Result<Json<Project>, ApiError> {
    let _lk = st.lock_for(&pid);
    let _g = _lk.lock().unwrap();
    let mut p = st.db.load_project(&pid).map_err(err)?;
    let policy = parse_policy(&req.zero_policy)?;
    let rates = st.source_rate_map(&pid);
    p = link::move_group(p, &rates, &gid, req.delta, policy).map_err(err)?;
    p.version += 1;
    st.db.save_version(&p).map_err(err)?;
    Ok(Json(p))
}

#[derive(Deserialize)]
struct GroupXfReq {
    frames: i64,
}

async fn group_crossfade(
    State(st): State<Arc<AppState>>,
    AxumPath((pid, gid)): AxumPath<(String, String)>,
    Json(req): Json<GroupXfReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let _lk = st.lock_for(&pid);
    let _g = _lk.lock().unwrap();
    let p = st.db.load_project(&pid).map_err(err)?;
    let map = st.sources_meta(&pid);
    // 评估始终返回每轨真实余量，便于界面展示冲突原因。
    let margins = link::evaluate_group_crossfades(&p, &gid, req.frames, &map).map_err(err)?;
    let feasible = margins.iter().all(|m| m.feasible);
    let mut p = match link::set_group_crossfade(p, &gid, req.frames, &map) {
        Ok(p) => p,
        Err(e) => {
            // 余量不足：整单拒绝，工程零变更（不产生新版本）。
            return Err(err(e));
        }
    };
    p.version += 1;
    st.db.save_version(&p).map_err(err)?;
    Ok(Json(serde_json::json!({ "project": p, "margins": margins, "feasible": feasible })))
}

async fn group_status(
    State(st): State<Arc<AppState>>,
    AxumPath(pid): AxumPath<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let p = st.db.load_project(&pid).map_err(err)?;
    let rates = st.source_rate_map(&pid);
    let statuses = link::group_statuses(&p, &rates);
    Ok(Json(serde_json::json!({ "version": p.version, "groups": statuses })))
}

// ---------- 多轨：联动删除 ----------

#[derive(Deserialize)]
struct DeleteReq {
    item_id: String,
    #[serde(default)]
    ripple: bool,
    #[serde(default = "default_policy")]
    zero_policy: String,
}

async fn delete_item(
    State(st): State<Arc<AppState>>,
    AxumPath(pid): AxumPath<String>,
    Json(req): Json<DeleteReq>,
) -> Result<Json<Project>, ApiError> {
    let _lk = st.lock_for(&pid);
    let _g = _lk.lock().unwrap();
    let mut p = st.db.load_project(&pid).map_err(err)?;
    let policy = parse_policy(&req.zero_policy)?;
    let rates = st.source_rate_map(&pid);
    // 原子保存：纯函数先完整算出新工程，成功后一次性写一个版本快照；
    // 中途任何冲突（锁轨/零点）都在写入前返回，库里的当前修订不受影响。
    p = link::delete_item(p, &rates, &req.item_id, req.ripple, policy).map_err(err)?;
    p.version += 1;
    st.db.save_version(&p).map_err(err)?;
    Ok(Json(p))
}

// ---------- 测试信号 ----------

#[derive(Deserialize)]
struct SignalReq {
    kind: String, // speech | pulse | sine
    sample_rate: u32,
    frames: usize,
    #[serde(default)]
    param: Option<f32>,
}

/// 生成测试信号 WAV 并落盘（用于自动化与手工试听），返回可上传的文件路径。
async fn make_test_signal(
    State(st): State<Arc<AppState>>,
    Json(req): Json<SignalReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let wav = match req.kind.as_str() {
        "speech" => {
            let gap = req.param.unwrap_or(2400.0) as usize;
            crate::signals::speech_rhythm(req.sample_rate, req.frames, gap.max(1))
        }
        "pulse" => crate::signals::single_pulse(
            req.sample_rate,
            req.frames,
            req.param.unwrap_or(0.0) as usize,
        ),
        "sine" => crate::signals::phase_sine(
            req.sample_rate,
            req.frames,
            req.param.unwrap_or(440.0),
        ),
        other => return Err(err(format!("未知信号类型 {other}"))),
    };
    let id = edit::new_id("sig");
    let path = format!("{}/{}.wav", st.data_dir, id);
    write_wav(&path, req.sample_rate, 1, &wav.samples).map_err(err)?;
    Ok(Json(serde_json::json!({ "path": path, "frames": wav.frames() })))
}
