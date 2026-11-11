mod db;
mod domain;
mod edit;
mod export;
mod link;
mod render;
mod resample;
mod server;
mod signals;
mod state;
mod wav;
mod waveform;

use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let data_dir = std::env::var("PE_DATA_DIR").unwrap_or_else(|_| "./data".to_string());
    let db_path = format!("{data_dir}/editor.sqlite");
    std::fs::create_dir_all(&data_dir)?;
    std::fs::create_dir_all("static").ok();

    let db = db::Db::open(&db_path)?;
    let state = state::AppState::new(db, data_dir);
    let app = server::router(state.clone());

    let addr = std::env::var("PE_BIND").unwrap_or_else(|_| "127.0.0.1:8080".to_string());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!("播客剪辑台已启动: http://{addr}  (数据目录 {})", state.data_dir);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
    println!("\n收到关闭信号，停止接受新请求…");
}

// 供集成测试复用的构建入口。
pub fn build_test_state(dir: &str) -> Arc<state::AppState> {
    let db = db::Db::open(&format!("{dir}/editor.sqlite")).unwrap();
    state::AppState::new(db, dir.to_string())
}
