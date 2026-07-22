//! kmc-mcp: kmc 노트북 관리 hub를 MCP(stdio)로 AI(Claude 등)에 노출하는 서버.
//!
//! Claude가 이 MCP에 붙어 다수 노트북을 한 번에 진단·제어한다. hub REST를 감싸는 얇은 어댑터.
//!
//! 설정(env):
//!   KMC_HUB_URL      hub 주소 (기본 http://127.0.0.1:8080)
//!   KMC_MCP_USER     hub admin 사용자명 (필수)
//!   KMC_MCP_PASSWORD hub admin 비밀번호 (필수)
//!
//! 등록: claude mcp add --transport stdio kmc -- kmc-mcp

mod hub;
mod server;

use std::sync::Arc;

use anyhow::Result;
use rmcp::{transport::stdio, ServiceExt};
use tracing_subscriber::EnvFilter;

use crate::hub::HubClient;
use crate::server::KmcServer;

#[tokio::main]
async fn main() -> Result<()> {
    // 로그는 stderr로 (stdout은 MCP JSON-RPC 프로토콜 전용).
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let hub = Arc::new(HubClient::from_env()?);
    tracing::info!("kmc-mcp starting (stdio)");

    let service = KmcServer::new(hub)
        .serve(stdio())
        .await
        .inspect_err(|e| tracing::error!("serve error: {e:?}"))?;
    service.waiting().await?;
    Ok(())
}
