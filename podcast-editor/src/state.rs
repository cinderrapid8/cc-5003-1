//! 运行时应用状态。
//!
//! 关键防护：
//! * 每个工程一把互斥锁，编辑串行化；波形/预览读取在锁外做重活，靠 version 判定新旧。
//! * 波形与预览响应携带 project.version；前端在版本变化后丢弃旧响应，
//!   撤销后仍在计算的旧波形不会冒充现状。
//! * 导出任务持有 CancelToken；取消通过 /api/export/cancel 触发，
//!   导出函数保证不留半成品。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use dashmap::DashMap;

use crate::db::Db;
use crate::domain::{Project, Source};
use crate::render::CancelToken;
use crate::wav::WavData;

pub struct AppState {
    pub db: Db,
    pub data_dir: String,
    /// project_id -> 工程锁（保护内存中的 Project 与编辑序列）。
    pub locks: DashMap<String, Arc<Mutex<()>>>,
    /// source_id -> 已加载的原始音频（只读，全局共享）。
    pub loaded: DashMap<String, Arc<WavData>>,
    /// project_id -> 源元数据缓存。
    pub sources: DashMap<String, HashMap<String, Source>>,
    /// 进行中的导出任务：任务键 -> 取消令牌。
    pub exports: DashMap<String, Arc<CancelToken>>,
    /// 已请求取消、但任务可能尚未注册的工程集合（消除取消/启动竞态）。
    pub pending_cancels: DashMap<String, ()>,
}

impl AppState {
    pub fn new(db: Db, data_dir: String) -> Arc<Self> {
        std::fs::create_dir_all(&data_dir).ok();
        Arc::new(AppState {
            db,
            data_dir,
            locks: DashMap::new(),
            loaded: DashMap::new(),
            sources: DashMap::new(),
            exports: DashMap::new(),
            pending_cancels: DashMap::new(),
        })
    }

    pub fn lock_for(&self, project_id: &str) -> Arc<Mutex<()>> {
        self.locks
            .entry(project_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// 载入（或取缓存）源音频。原文件只读打开。
    pub fn load_source(&self, source_id: &str, meta: &Source) -> anyhow::Result<Arc<WavData>> {
        if let Some(w) = self.loaded.get(source_id) {
            return Ok(w.clone());
        }
        let wav = Arc::new(crate::wav::read_wav(&meta.path)?);
        self.loaded
            .insert(source_id.to_string(), wav.clone());
        Ok(wav)
    }

    pub fn source_rate_map(
        &self,
        project_id: &str,
    ) -> HashMap<String, u32> {
        self.sources
            .get(project_id)
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.original_rate)).collect())
            .unwrap_or_default()
    }

    pub fn sources_meta(&self, project_id: &str) -> HashMap<String, Source> {
        self.sources.get(project_id).map(|m| m.clone()).unwrap_or_default()
    }

    pub fn put_sources(&self, project_id: &str, map: HashMap<String, Source>) {
        self.sources.insert(project_id.to_string(), map);
    }
}

/// 带版本戳的响应包装。前端据 version 判断是否仍为当前状态。
#[derive(serde::Serialize)]
pub struct Versioned<T: serde::Serialize> {
    pub version: i64,
    pub data: T,
}

pub fn versioned<T: serde::Serialize>(project: &Project, data: T) -> Versioned<T> {
    Versioned {
        version: project.version,
        data,
    }
}
