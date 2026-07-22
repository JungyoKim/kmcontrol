mod admin_ws;
mod agent_ws;
mod db;
mod routes;
mod state;
mod util;

use anyhow::{Context, Result};
use axum::routing::{get, post};
use axum::Router;
use clap::{Parser, Subcommand};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use state::AppState;

#[derive(Parser)]
#[command(name = "kmc-hub", about = "노트북 관리 hub-server")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// 서버 구동 (기본).
    Serve,
    /// 관리자 계정 추가.
    Admin {
        #[command(subcommand)]
        cmd: AdminCmd,
    },
}

#[derive(Subcommand)]
enum AdminCmd {
    Add {
        #[arg(long)]
        username: String,
        #[arg(long)]
        password: String,
    },
}

fn db_path() -> String {
    std::env::var("KMC_HUB_DB").unwrap_or_else(|_| "hub.db".to_string())
}

fn listen_addr() -> String {
    std::env::var("KMC_HUB_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,kmc_hub=debug".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.cmd.unwrap_or(Cmd::Serve) {
        Cmd::Serve => serve().await,
        Cmd::Admin { cmd } => match cmd {
            AdminCmd::Add { username, password } => {
                let conn = db::open(&db_path())?;
                db::add_admin(&conn, &username, &password)
                    .with_context(|| "add admin")?;
                println!("admin '{username}' added");
                Ok(())
            }
        },
    }
}

async fn serve() -> Result<()> {
    let conn = db::open(&db_path())?;
    let state = AppState::new(conn);

    let app = Router::new()
        .route("/provision", post(routes::provision))
        .route("/enroll/{secret}", get(routes::enroll))
        .route("/auth/login", post(routes::login))
        .route("/agents", get(routes::list_agents))
        .route("/session/request", post(routes::session_request))
        .route("/session/release", post(routes::session_release))
        .route("/agents/{id}/command", post(routes::run_command))
        .route("/agent/ws", get(agent_ws::handler))
        .route("/admin/ws", get(admin_ws::handler))
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr = listen_addr();
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    tracing::info!(%addr, "kmc-hub listening");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
    .context("serve")?;
    Ok(())
}
