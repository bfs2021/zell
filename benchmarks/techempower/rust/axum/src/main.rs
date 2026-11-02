//! TechEmpower Framework Benchmarks scenarios on axum (Tokio + hyper; the
//! same service as ../../zell/main.zl); the database code is in ../common.
use axum::extract::{RawQuery, State};
use axum::http::header::{CONTENT_TYPE, SERVER};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::serve::ListenerExt;
use axum::{Json, Router};
use std::sync::Arc;
use te_common::{Message, Pool, World};

struct St {
    pool: Pool,
    cache: Vec<World>,
}

type S = State<Arc<St>>;

fn e500<E: std::fmt::Display>(e: E) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

type R<T> = Result<T, (StatusCode, String)>;

async fn plaintext() -> impl IntoResponse {
    ([(CONTENT_TYPE, "text/plain; charset=utf-8")], "Hello, World!")
}

async fn json() -> Json<Message> {
    Json(Message { message: "Hello, World!" })
}

async fn db(State(st): S) -> R<Json<World>> {
    let c = st.pool.get().await.map_err(e500)?;
    Ok(Json(te_common::load_world(&c).await.map_err(e500)?))
}

async fn queries(State(st): S, RawQuery(q): RawQuery) -> R<Json<Vec<World>>> {
    let n = te_common::count(q.as_deref(), "queries");
    let c = st.pool.get().await.map_err(e500)?;
    Ok(Json(te_common::queries(&c, n).await.map_err(e500)?))
}

async fn updates(State(st): S, RawQuery(q): RawQuery) -> R<Json<Vec<World>>> {
    let n = te_common::count(q.as_deref(), "queries");
    let c = st.pool.get().await.map_err(e500)?;
    Ok(Json(te_common::updates(&c, n).await.map_err(e500)?))
}

async fn cached(State(st): S, RawQuery(q): RawQuery) -> Json<Vec<World>> {
    Json(te_common::cached(&st.cache, te_common::count(q.as_deref(), "count")))
}

async fn fortunes(State(st): S) -> R<Html<String>> {
    let c = st.pool.get().await.map_err(e500)?;
    Ok(Html(te_common::fortunes(&c).await.map_err(e500)?))
}

async fn server_header(mut res: Response) -> Response {
    res.headers_mut().insert(SERVER, HeaderValue::from_static("axum"));
    res
}

#[tokio::main]
async fn main() {
    let pool = te_common::pool();
    let cache = te_common::load_cache(&pool).await;
    let st = Arc::new(St { pool, cache });
    let app = Router::new()
        .route("/plaintext", get(plaintext))
        .route("/json", get(json))
        .route("/db", get(db))
        .route("/queries", get(queries))
        .route("/updates", get(updates))
        .route("/fortunes", get(fortunes))
        .route("/cached-queries", get(cached))
        .layer(axum::middleware::map_response(server_header))
        .with_state(st);
    let port = te_common::port();
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .expect("bind")
        .tap_io(|tcp| {
            let _ = tcp.set_nodelay(true);
        });
    eprintln!("listening on {port}");
    axum::serve(listener, app).await.expect("serve");
}
