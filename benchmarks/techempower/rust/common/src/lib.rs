//! Database work shared by the actix-web and axum services: a deadpool pool
//! of tokio-postgres connections (DB_POOL, 64) with statements prepared once
//! per connection, the World/Fortune queries and the fortunes page. Env:
//! DB_DSN, DB_POOL, PORT (8080).
use deadpool_postgres::{Client, Manager, ManagerConfig, RecyclingMethod};
pub use deadpool_postgres::Pool;
use rand::Rng;
use serde::Serialize;
use std::fmt::Write;
use tokio_postgres::NoTls;

pub use deadpool_postgres::PoolError;
pub use tokio_postgres::Error as DbError;

const SELECT_WORLD: &str = "select id, randomnumber from world where id = $1";
const UPDATE_WORLD: &str = "update world set randomnumber = $1 where id = $2";
const SELECT_FORTUNES: &str = "select id, message from fortune";

#[derive(Serialize)]
pub struct Message {
    pub message: &'static str,
}

#[derive(Serialize, Clone, Copy)]
pub struct World {
    pub id: i32,
    #[serde(rename = "randomNumber")]
    pub random_number: i32,
}

pub fn port() -> u16 {
    std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8080)
}

pub fn pool() -> Pool {
    let dsn = std::env::var("DB_DSN").unwrap_or_else(|_| {
        "postgres://benchmarkdbuser:benchmarkdbpass@127.0.0.1:5432/hello_world".into()
    });
    let size = std::env::var("DB_POOL").ok().and_then(|p| p.parse().ok()).unwrap_or(64);
    let cfg: tokio_postgres::Config = dsn.parse().expect("DB_DSN");
    let mgr = Manager::from_config(cfg, NoTls, ManagerConfig { recycling_method: RecyclingMethod::Fast });
    Pool::builder(mgr).max_size(size).build().expect("pool")
}

pub fn random_id() -> i32 {
    rand::rng().random_range(1..=10000)
}

/// The value of `key` in a query string, clamped to 1..500 (1 when absent or invalid).
pub fn count(query: Option<&str>, key: &str) -> usize {
    for kv in query.unwrap_or("").split('&') {
        if let Some(v) = kv.strip_prefix(key).and_then(|r| r.strip_prefix('=')) {
            return v.parse::<i64>().map(|n| n.clamp(1, 500) as usize).unwrap_or(1);
        }
    }
    1
}

pub async fn load_cache(pool: &Pool) -> Vec<World> {
    let c = pool.get().await.expect("db");
    let rows = c.query("select id, randomnumber from world order by id", &[]).await.expect("cache");
    rows.iter().map(|r| World { id: r.get(0), random_number: r.get(1) }).collect()
}

pub async fn load_world(c: &Client) -> Result<World, DbError> {
    let st = c.prepare_cached(SELECT_WORLD).await?;
    let row = c.query_one(&st, &[&random_id()]).await?;
    Ok(World { id: row.get(0), random_number: row.get(1) })
}

pub async fn queries(c: &Client, n: usize) -> Result<Vec<World>, DbError> {
    let mut ws = Vec::with_capacity(n);
    for _ in 0..n {
        ws.push(load_world(c).await?);
    }
    Ok(ws)
}

pub async fn updates(c: &Client, n: usize) -> Result<Vec<World>, DbError> {
    let mut ws = queries(c, n).await?;
    for w in ws.iter_mut() {
        w.random_number = random_id();
    }
    let st = c.prepare_cached(UPDATE_WORLD).await?;
    for w in &ws {
        c.execute(&st, &[&w.random_number, &w.id]).await?;
    }
    Ok(ws)
}

pub fn cached(cache: &[World], n: usize) -> Vec<World> {
    (0..n).map(|_| cache[(random_id() - 1) as usize]).collect()
}

pub async fn fortunes(c: &Client) -> Result<String, DbError> {
    let st = c.prepare_cached(SELECT_FORTUNES).await?;
    let rows = c.query(&st, &[]).await?;
    let mut fs: Vec<(i32, String)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    fs.push((0, "Additional fortune added at request time.".into()));
    fs.sort_by(|a, b| a.1.cmp(&b.1));
    let mut s = String::with_capacity(2048);
    s.push_str("<!DOCTYPE html><html><head><title>Fortunes</title></head><body><table><tr><th>id</th><th>message</th></tr>");
    for (id, msg) in &fs {
        let _ = write!(s, "<tr><td>{id}</td><td>");
        escape(&mut s, msg);
        s.push_str("</td></tr>");
    }
    s.push_str("</table></body></html>");
    Ok(s)
}

fn escape(out: &mut String, s: &str) {
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&#34;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
}
