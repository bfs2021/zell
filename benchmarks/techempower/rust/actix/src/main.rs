//! TechEmpower Framework Benchmarks scenarios on actix-web (the same service
//! as ../../zell/main.zl); the database code is in ../common.
use actix_web::error::ErrorInternalServerError as e500;
use actix_web::middleware::DefaultHeaders;
use actix_web::{web, App, HttpRequest, HttpResponse, HttpServer, Result};
use te_common::{Message, Pool, World};

struct St {
    pool: Pool,
    cache: Vec<World>,
}

type Data = web::Data<St>;

async fn plaintext() -> HttpResponse {
    HttpResponse::Ok().content_type("text/plain; charset=utf-8").body("Hello, World!")
}

async fn json() -> HttpResponse {
    HttpResponse::Ok().json(Message { message: "Hello, World!" })
}

async fn db(st: Data) -> Result<HttpResponse> {
    let c = st.pool.get().await.map_err(e500)?;
    Ok(HttpResponse::Ok().json(te_common::load_world(&c).await.map_err(e500)?))
}

async fn queries(st: Data, req: HttpRequest) -> Result<HttpResponse> {
    let n = te_common::count(Some(req.query_string()), "queries");
    let c = st.pool.get().await.map_err(e500)?;
    Ok(HttpResponse::Ok().json(te_common::queries(&c, n).await.map_err(e500)?))
}

async fn updates(st: Data, req: HttpRequest) -> Result<HttpResponse> {
    let n = te_common::count(Some(req.query_string()), "queries");
    let c = st.pool.get().await.map_err(e500)?;
    Ok(HttpResponse::Ok().json(te_common::updates(&c, n).await.map_err(e500)?))
}

async fn cached(st: Data, req: HttpRequest) -> HttpResponse {
    let n = te_common::count(Some(req.query_string()), "count");
    HttpResponse::Ok().json(te_common::cached(&st.cache, n))
}

async fn fortunes(st: Data) -> Result<HttpResponse> {
    let c = st.pool.get().await.map_err(e500)?;
    let page = te_common::fortunes(&c).await.map_err(e500)?;
    Ok(HttpResponse::Ok().content_type("text/html; charset=utf-8").body(page))
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let pool = te_common::pool();
    let cache = te_common::load_cache(&pool).await;
    let st = web::Data::new(St { pool, cache });
    let port = te_common::port();
    eprintln!("listening on {port}");
    HttpServer::new(move || {
        App::new()
            .app_data(st.clone())
            .wrap(DefaultHeaders::new().add(("Server", "actix")))
            .route("/plaintext", web::get().to(plaintext))
            .route("/json", web::get().to(json))
            .route("/db", web::get().to(db))
            .route("/queries", web::get().to(queries))
            .route("/updates", web::get().to(updates))
            .route("/fortunes", web::get().to(fortunes))
            .route("/cached-queries", web::get().to(cached))
    })
    .bind(("0.0.0.0", port))?
    .run()
    .await
}
