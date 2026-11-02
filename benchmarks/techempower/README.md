# TechEmpower-style service: Zell vs Go, .NET and Rust

One service written five times, with the scenarios of the
[TechEmpower Framework Benchmarks](https://github.com/TechEmpower/FrameworkBenchmarks)
(their schema and rules, our own load script):

| path | work per request |
|---|---|
| `/plaintext` | `Hello, World!` |
| `/json` | `{"message":"Hello, World!"}` serialized per request |
| `/db` | one random row of `world` as JSON |
| `/queries?queries=N` | N separate queries (1..500) |
| `/fortunes` | all rows of `fortune` + one added at run time, sorted, HTML-escaped table |
| `/updates?queries=N` | N reads, then N single-row updates |
| `/cached-queries?count=N` | N random rows from an in-memory copy of `world` loaded at start |

- `zell/main.zl`: `net.http` + `database.postgres` (`database.sql` pool)
- `go/main.go`: `net/http` + pgx (`pgxpool`)
- `dotnet/Program.cs`: ASP.NET Core minimal APIs + Npgsql
- `rust/actix`: actix-web 4, and `rust/axum`: axum 0.8 (Tokio + hyper); both
  use `rust/common` (deadpool-postgres + tokio-postgres, statements prepared
  once per connection, one pooled connection per request). Release profile
  with `lto = true`, `codegen-units = 1`.

Each uses a pool of 64 connections (`DB_POOL`), `PORT` (8080) and `DB_DSN`.

## Setup (Ubuntu)

PostgreSQL with the TechEmpower database: user `benchmarkdbuser` /
`benchmarkdbpass`, database `hello_world`, tables
`world(id int primary key, randomnumber int)` with 10000 rows and
`fortune(id int primary key, message varchar(2048))` with the 12 standard
messages; `synchronous_commit = off`, `max_connections = 1000`.

```sh
zell build --release benchmarks/techempower/zell/main.zl -o /root/te-zell
(cd benchmarks/techempower/go && go build -o /root/te-go .)
(cd benchmarks/techempower/dotnet && dotnet publish -c Release -o /root/te-dotnet-out)
(cd benchmarks/techempower/rust && cargo build --release &&
    cp target/release/te-actix target/release/te-axum /root/)
benchmarks/techempower/run.sh 3       # wrk -t8 -c256, 5 s warm-up + 15 s per test
APPS="zell go dotnet actix axum" benchmarks/techempower/run.sh 3
```

`run.sh` pins the service to CPUs 0-15, PostgreSQL (systemd `CPUAffinity`)
to 16-23 and wrk to 24-31, and prints req/s, p50, p99 and peak RSS.
`pipelined` is `/plaintext` with 16 requests per write (`pipeline.lua`), as in
TechEmpower's plaintext test; the plain `plaintext` row sends one at a time.

## Results

### 2026-11-02, short run (one round, 10 s per test)

EPYC 9554 server (32 vCPU, Ubuntu 26.04, Linux 7.0, PostgreSQL 18.6 on the
same host), pinning as above, `wrk -t8 -c256`. Zell is master `0d05f3e`
(`--release`, no LTO); Go 1.27.3; .NET 10.0.114; Rust 1.99.1 (actix-web 4,
axum 0.8). One round only: treat differences under ~5% as noise. Requests
per second, p99 in parentheses (wrk reports no p99 for pipelined .NET and
actix).

| test | Zell | Go | .NET | actix | axum |
|---|---:|---:|---:|---:|---:|
| plaintext | **635k** (0.44 ms) | 466k (4.25 ms) | 520k (1.36 ms) | 579k (0.46 ms) | 623k (0.72 ms) |
| pipelined ×16 | 2.46M (2.92 ms) | 594k (108 ms) | 2.97M | **6.87M** | 1.42M (5.21 ms) |
| json | **638k** (0.44 ms) | 455k (4.42 ms) | 535k (1.11 ms) | 585k (0.45 ms) | 626k (0.72 ms) |
| db | 209k (1.62 ms) | 181k (3.76 ms) | 213k (2.69 ms) | **237k** (1.61 ms) | 235k (1.32 ms) |
| queries ×20 | 12.7k (21.1 ms) | 12.0k (24.2 ms) | 11.6k (24.1 ms) | 12.1k (22.8 ms) | **12.9k** (21.8 ms) |
| fortunes | 181k (1.93 ms) | 130k (4.59 ms) | 190k (2.86 ms) | **231k** (1.52 ms) | 222k (1.41 ms) |
| updates ×20 | **5.18k** (52.2 ms) | 4.78k (63.5 ms) | 4.74k (58.1 ms) | 4.89k (56.4 ms) | 5.16k (53.7 ms) |
| cached-queries ×20 | 566k (0.90 ms) | 340k (8.00 ms) | 496k (2.45 ms) | 582k (0.46 ms) | **594k** (0.81 ms) |

Peak RSS (VmHWM): without the database Zell 15–20 MB, Go 41–51 MB, .NET
138–176 MB, actix 13–19 MB, axum 14–18 MB; over the whole run Zell 148 MB,
Go 57 MB, .NET 195 MB, actix 21 MB, axum 25 MB.

The Rust services hold one pooled connection for the whole request; the
Zell, Go and .NET ones take a connection from the pool per query, which
matters for `queries` and `updates`.
