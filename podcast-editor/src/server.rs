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
    frames_to_rate, project_frame_to_source, Item, Project, Source,
};
use crate::edit;
use crate::export::export_wav;
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
        .route("/api/projects/{pid}/export/cancel", post(cancel_export))
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
    p.items.push(Item {
        id: edit::new_id("item"),
        source_id: req.source_id,
        source_start: req.source_start,
        source_end: req.source_end,
        timeline_start: 0,
        crossfade_out: 0,
    });
    p.version += 1;
    let rates = st.source_rate_map(&pid);
    let p = edit::layout(p, &rates);
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
    let rates = st.source_rate_map(&pid);
    let mut p = edit::trim_item(p, &rates, &req.item_id, req.source_start, req.source_end)
        .map_err(err)?;
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
    let mut p = edit::split_item(p, &rates, &req.item_id, req.at_source_frame).map_err(err)?;
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
    let p = st.db.load_project(&pid).map_err(err)?;
    let rates = st.source_rate_map(&pid);
    let mut p = edit::reorder(p, &rates, &req.item_id, req.target_index).map_err(err)?;
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
    // 先基于「请求的新淡化长度」计算真实余量，前端据此提示素材到头时的可用长度。
    let map = st.sources_meta(&pid);
    let sorted = {
        let mut v = p.items.clone();
        v.sort_by_key(|i| i.timeline_start);
        v
    };
    let pos = sorted.iter().position(|i| i.id == req.item_id);
    let margin = if let Some(i) = pos {
        if let Some(next) = sorted.get(i + 1) {
            // 用请求值构造临时片段来评估，而不是用数据库里的旧值。
            let mut candidate = sorted[i].clone();
            candidate.crossfade_out = req.frames;
            Some(crate::domain::check_crossfade(
                &candidate, next, &map, p.sample_rate,
            ))
        } else {
            None
        }
    } else {
        None
    };

    let mut p = edit::set_crossfade(p, &req.item_id, req.frames).map_err(err)?;
    let rates = st.source_rate_map(&pid);
    p = edit::layout(p, &rates);
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
    let p = st.db.load_project(&pid).map_err(err)?;
    let rates = st.source_rate_map(&pid);
    let mut p = edit::append_silence(p, req.length).map_err(err)?;
    p = edit::layout(p, &rates);
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
        let w = st.load_source(&m.id, &m).map_err(err)?;
        loaded.insert(m.id.clone(), w);
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

async fn cancel_export(
    State(st): State<Arc<AppState>>,
    AxumPath(pid): AxumPath<String>,
) -> Json<serde_json::Value> {
    let mut n = 0;
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
