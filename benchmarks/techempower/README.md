# TechEmpower-style service: Zell vs Go vs .NET

One service written three times, with the scenarios of the
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

- `zell/main.zl`: `net.http` + `database.postgres` (`database.sql` pool)
- `go/main.go`: `net/http` + pgx (`pgxpool`)
- `dotnet/Program.cs`: ASP.NET Core minimal APIs + Npgsql

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
benchmarks/techempower/run.sh 3       # wrk -t8 -c256, 5 s warm-up + 15 s per test
```

`run.sh` pins the service to CPUs 0-15, PostgreSQL (systemd `CPUAffinity`)
to 16-23 and wrk to 24-31, and prints req/s, p50, p99 and peak RSS.
Plaintext is measured without HTTP pipelining.
