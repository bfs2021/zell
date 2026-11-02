# Zell Framework: HTTP, данные, логирование, диагностика

Цель: из коробки давать уровень ASP.NET Core + EF Core + Serilog + pprof,
но проще, быстрее и без магии рантайма. Всё строится на четырёх свойствах
языка: generics (мономорфизация), compile-time рефлексия
(`reflect.Fields<T>()`), типизированные атрибуты и `async/await`.
Там, где .NET использует JIT и Reflection.Emit, Zell генерирует код при
компиляции; там, где Go использует строковые теги и `interface{}`, Zell
использует атрибуты и generics.

Разделение: стандартная библиотека (корни `io`, `net`, `encoding`, …) содержит базовые протоколы и
форматы; фреймворк `zell.data`, `zell.log`, `zell.diag` и будущий
`zell-lang/web` — отдельные пакеты в реестре с той же командой разработки.

## 1. Форматы: `encoding.{json,xml,yaml,toml}`

Единая модель для всех четырёх форматов:

```zell
@json(name: "user")
struct User {
    @json(name: "id")                 Id       int
    @json(omitEmpty: true)            Email    string
    @json(skip: true)                 password string
    @xml(attr: true)                  Role     string
    @yaml(flow: true)                 Tags     []string
    CreatedAt time.Time               // форматируется как RFC 3339
}

var u = json.Parse<User>(text)?          // строгая типизация, ошибка с путём: "users[2].id: expected int"
var text = json.Format(u)
var pretty = json.Format(u, json.Options{Indent: 2})

var any = json.ParseValue(text)?         // динамическое дерево json.Value, когда схемы нет
switch any {
    case json.Object o: ...
    case json.Array a:  ...
}
```

- **Compile-time кодогенерация**: `json.Parse<T>` раскрывается через
  `reflect.Fields<T>()` в прямой код чтения полей без `reflect.Value` и
  boxing. Скорость уровня `serde_json`/`System.Text.Json` с source generators.
- **Потоковый режим**: `json.Reader`/`json.Writer` для больших документов
  и NDJSON, `xml.Reader` (pull-парсер, как XmlReader).
- **Общие атрибуты**: `@serde(name:, skip:, omitEmpty:, default:, flatten:)`
  работает для всех форматов, формат-специфичные (`@json`, `@xml(attr)`,
  `@yaml`, `@toml`) уточняют.
- **Пользовательские конвертеры**: интерфейс `json.Converter<T>` с
  `Read(r *json.Reader) T!` и `Write(w *json.Writer, v T)`; регистрация
  атрибутом `@json(converter: MoneyConverter)`.
- **Валидация при разборе**: `@validate(required: true, min: 1, max: 100,
  pattern: "...")` проверяется в том же проходе, ошибки собираются списком.
- **Схемы**: `json.Schema<T>()` генерирует JSON Schema из типа и атрибутов
  `@json` (✓ в `encoding.json`; ключевые слова валидации и OpenAPI вернутся
  вместе с `zell-lang/web`).
- **Enum**: сериализуется по имени варианта; `@json(value: 1)` для чисел;
  enum с полями — как tagged union (`{"kind": "Ok", "value": 1}`,
  настраивается `@json(tag: "type", content: "data")`).
- Полиморфизм через интерфейсы: `@json(discriminator: "type")` на интерфейсе
  и `@json(name: "circle")` на реализациях.
- TOML и YAML дополнительно: сохранение комментариев при round-trip
  (`toml.Document`), якоря YAML, многодокументные YAML-потоки.

## 2. HTTP-сервер: `net.http`

`net.http` — уровень `Kestrel`; уровень ASP.NET Core (атрибутная
маршрутизация, DI, OpenAPI) вынесен из дерева, см. ниже.

### `net.http` (низкий уровень)

- HTTP/1.1, HTTP/2 (h2 и h2c), HTTP/3 позже; TLS через `net.tls`.
- Полностью асинхронный: один запрос — одна задача на пуле потоков, без
  потока на соединение. Zero-copy чтение тела, streaming ответов,
  backpressure.
- `http.Server{Addr, Handler, ReadTimeout, ...}`, интерфейс
  `Handler { async func Serve(ctx *http.Context) void! }`.
- `http.Client` с пулом соединений, таймаутами, retry-политикой, HTTP/2,
  `http.Get<T>(url)` с автоматическим разбором JSON.
- WebSocket, Server-Sent Events, graceful shutdown, Unix-сокеты.

**Статус (реализовано, `net.http`, `net.http.hpack`,
`net.http.test`, `net.ws`; см. DESIGN.md «Сеть и протоколы» и
`runtime/ABI.md` §16.4–16.5):**

- ✓ HTTP/1.1 сервер: задача на соединение, keep-alive, разбор заголовков в
  буфере соединения на C (без строки на строку заголовка), один `write` на
  ответ с кэшированным `Date`; на `benchmarks/http_hello` — на уровне Go
  `net/http` (см. `benchmarks/README.md`).
- ✓ HTTP/2 (RFC 9113) сервер и клиент: h2 по ALPN поверх `net.tls`,
  h2c с prior knowledge (`Server{H2C: true}`, `Transport{H2C: true}`),
  HPACK со статической/динамической таблицами и Хаффманом, управление
  потоком, SETTINGS/PING/GOAWAY/RST_STREAM, задача на поток, тот же
  `Handler`; приоритеты игнорируются, server push не реализован. Interop:
  `curl --http2`, `curl --http2-prior-knowledge`, клиент и сервер Go
  `net/http` (`compiler/tests/net_http.rs`).
- ✓ Клиент: пул соединений (`Transport`: на хост, `MaxIdleConnsPerHost`,
  `MaxConnsPerHost`, `IdleConnTimeout`, повтор на «протухшем» соединении,
  TLS-сессии), прокси (CONNECT, `HTTP_PROXY`/`NO_PROXY`), таймауты
  (подключение, TLS, заголовок ответа, весь запрос — через контекст задачи),
  gzip, `CookieJar`, построитель запроса, потоковое тело (`io.AsyncReader`),
  `JSON<T>`; `http.GetJSON<T>(url)`.
- ✓ WebSocket (`net.ws`), Server-Sent Events (`ServerSentEvents`,
  `EventSource`), `ServeFile`/`ServeDir` (диапазоны, ETag, sendfile),
  middleware в роутере (`Recover`, `Logging` через `log`, `CORS`,
  `Gzip`, `BodyLimit`, `TimeoutHandler`), лимиты заголовков и тел, graceful
  shutdown (HTTP/2 — GOAWAY), Unix-сокеты (`Addr: "unix:/path"`),
  `net.http.test` (`NewServer`, `NewRecorder`, `NewRequest`).
- Нет: HTTP/3, server push, permessage-deflate, `http.Context`-обработчики
  (`Handler` пока `Serve(req, res)`), WebSocket поверх HTTP/2 (RFC 8441),
  (потоковое чтение тела запроса на сервере есть: `Server{StreamBodies}` +
  `req.Reader`, лимит `MaxBodyBytes`; `SendFile` по HTTP/2 читает файл
  кусками по 64 KB, а не целиком).

### TLS

Собственная реализация, сразу, без BoringSSL/OpenSSL:

- **TLS 1.3**: cipher suites `AES_128_GCM_SHA256`, `AES_256_GCM_SHA384`,
  `CHACHA20_POLY1305_SHA256`; обмен ключами X25519 и P-256; подписи Ed25519,
  ECDSA P-256, RSA-PSS. 0-RTT выключен.
- **TLS 1.2 в современном подмножестве**: только ECDHE (X25519, P-256) и
  только AEAD: `ECDHE_ECDSA/ECDHE_RSA_WITH_AES_128/256_GCM_SHA256/384`,
  `ECDHE_*_WITH_CHACHA20_POLY1305_SHA256`; подписи ECDSA, RSA-PKCS1 и RSA-PSS;
  `extended_master_secret` и `encrypt_then_mac` обязательны, session tickets
  (RFC 5077). Без RSA key exchange, CBC, RC4, 3DES, SHA-1-suites, compression,
  renegotiation, SSLv3/TLS 1.0/1.1. По умолчанию `MinVersion: tls.V12`;
  клиент предлагает 1.3 и откатывается на 1.2 по правилам RFC 8446 с защитой
  от downgrade (`DOWNGRD` sentinel в ServerHello.random).
- **Протокол на Zell**: машина состояний handshake, record layer, HKDF и
  расписание ключей, X.509-парсер (DER) и проверка цепочек, SNI, ALPN,
  session tickets, OCSP stapling, горячая перезагрузка сертификатов.
- **Примитивы на C** в рантайме с константным временем (AES-GCM,
  ChaCha20-Poly1305, X25519, P-256, Ed25519, RSA-PSS, SHA-2, HMAC, HKDF);
  AES-NI/NEON через intrinsics; в wasm — чистый C.
- **Тесты**: трассы RFC 8448 (1.3) байт в байт, тестовые handshake 1.2 против OpenSSL с фиксированными ключами, векторы Wycheproof для каждого
  примитива, interop с `openssl s_server`/`s_client` и `curl` в CI, fuzzing
  record layer и X.509 через libFuzzer. До внешнего аудита — пометка в
  документации.
- **Интеграция**: шифрование в буферы, ввод-вывод — наши async-сокеты, без
  потока на соединение; `http.Get("https://...")`, `http.Server{TLS: ...}`,
  `h2` через ALPN; системные корни через `runtime/src/platform/`;
  ACME (`AutoTLS`) — в будущем пакете `zell-lang/web`.
- **Результат**: TLS добавляет ~300–400 KB к бинарнику, одна сборка под
  все платформы, wasm и мобильные без C-зависимостей.

**Статус (реализовано, `net.tls` + `crypto.*`, см. DESIGN.md
«Сеть и протоколы» / «Криптография» и `runtime/ABI.md` §19):**

- ✓ TLS 1.3 и TLS 1.2 (ECDHE + AEAD), клиент и сервер, все перечисленные
  выше наборы, группы X25519/P-256, подписи ECDSA P-256, Ed25519, RSA-PSS,
  RSA PKCS#1 (1.2 и сертификаты); HelloRetryRequest, SNI, ALPN, KeyUpdate,
  сертификаты клиента, PSK-возобновление (1.3) и session tickets (1.2,
  RFC 5077), `extended_master_secret` обязателен, `DOWNGRD`-защита, 0-RTT
  выключен, renegotiation нет.
- ✓ Примитивы на C с постоянным временем: bitsliced AES + GHASH без таблиц,
  AES-NI/PCLMULQDQ по cpuid (на этой машине AES-128-GCM ~1 GB/s с AES-NI и
  ~70 MB/s без него, ChaCha20-Poly1305 ~2.4 GB/s с AVX2 (`ZELL_NO_AVX2=1` отключает), ~270 MB/s без него), X25519 и Ed25519 (поле
  2^255-19 на 51-битных лимбах; Ed25519 с таблицей базовой точки: подпись ~43k, проверка ~21k оп/с, 1.3x и 1.2x от Go), P-256 (Монтгомери, полные формулы RCB, таблица базовой точки: подпись ~23k, проверка ~6.3k оп/с, 1.9x и 3.2x от Go),
  RSA (CRT + ослепление экспоненты), SHA-2 (SHA-256 через SHA-NI по cpuid,
  `ZELL_NO_SHANI=1` отключает), HMAC, HKDF. NEON — нет.
- ✓ X.509 на Zell: DER/PEM, цепочки, сроки, CA/pathlen, keyUsage/EKU,
  dNSName name constraints, проверка имени хоста; системные корни через
  `runtime/src/platform/certroots.c` (Linux/BSD; macOS/Windows — только
  `$SSL_CERT_FILE`).
- ✓ Интеграция: `http.Get("https://...")`, `http.Client{TLS}`,
  `http.Server{TLS: &tls.ServerConfig{...}}`.
- ✓ Тесты: векторы RFC/NIST/Wycheproof-стиля для каждого примитива
  (`runtime/tests/test_crypto.c`), расписание ключей и записи RFC 8448 байт в
  байт (`tests/spec/stdlib/tls_rfc8448.zl`), полная трасса рукопожатия
  TLS 1.3 «The Illustrated TLS 1.3 Connection» (AES-256-GCM-SHA384, RSA-PSS:
  секреты, расшифровка записей, CertificateVerify, оба Finished —
  `tests/spec/stdlib/tls_trace13.zl`), interop с `openssl s_server` /
  `s_client` в обе стороны для 1.3 и 1.2 (`compiler/tests/tls.rs`),
  fuzz-smoke парсеров X.509, record layer и рукопожатий (без паник и утечек).
  libFuzzer-цели и interop с `curl` — пока нет.
- Нет: OCSP stapling, горячая перезагрузка сертификатов, `h2` (нет HTTP/2),
  ACME (`AutoTLS`), P-384, wasm-сборка не проверялась. Внешнего аудита не
  было (`docs/SECURITY.md`).

### Фреймворк поверх `net.http`: вынесен из дерева

Атрибутный фреймворк `web` (контроллеры `@route`/`@get…`, биндинг и
`@validate`, DI, middleware, JWT, `web.config`, OpenAPI, `TestServer`)
жил в stdlib, а таблицу маршрутов и DI-контейнер строил компилятор
(`sema/web.rs`) при каждой сборке. Решение (2026-10-24): до появления
`reflect.TypesWith<A>()` (`docs/REFLECTION_PLAN.md`) его нет в дереве;
он вернётся отдельным пакетом `zell-lang/web` (план в конце документа).
`TypesWith` реализован 2026-10-24 — следующий шаг — сам пакет.
Полные исходники, спек-тесты (`tests/spec/web`), пример
`examples/web_app`, бенчмарк `benchmarks/web_api` и это описание
сохранены тегом git **`web-in-tree`**; последний тулчейн со встроенным
`web` — `v0.1.0-rc.3`. Корень `web` больше не зарезервирован за stdlib:
пакет может объявить пространство имён `web`.

Без фреймворка HTTP-сервисы пишутся на `net.http` (роутер по методу и
пути, middleware `CORS`/`Gzip`, HTTP/2, WebSocket) и `net.http.test`.

## 3. Данные: `database.sql` и `zell.data` (ORM уровня EF Core)

### `database.sql`

- Единый интерфейс `sql.DB`, `Conn`, `Tx`, `Rows`; асинхронный; пул
  соединений; драйверы PostgreSQL, MySQL, SQLite (нативные, на Zell),
  SQL Server и ClickHouse позже.
- Типизированные запросы: `db.Query<User>("select ... where id = $1", id)`
  маппит строки в структуры по именам столбцов и `@column`.

#### Статус реализации (`database.sql`)

Реализовано (Zell: `sema/stdlib_src/sql.zl`, драйверы `postgres.zl`,
`postgres_types.zl`, `sqlite.zl`; C — только SQLite и сигнальные ошибки,
`runtime/ABI.md` §23):

- Интерфейс драйвера: `Driver.Open(dsn) Task<DriverConn!>`, `DriverConn
  {Prepare, Exec, Query, Begin, Ping, Close, IsValid, Placeholder}`,
  `DriverStmt {Exec, Query, NumInput, Close}`, `DriverRows {Columns,
  ColumnTypes, Next, Close}`, `DriverTx {Commit, Rollback}`; значения —
  `nil`, `int`, `float`, `bool`, `string`, `[]byte`, `time.Time`, `[]any`.
  `sql.Register(name, d)`, `sql.Drivers()`, `sql.Open(name, dsn) *DB!`,
  `sql.OpenDriver(d, dsn)`; `postgres.Open(dsn)`, `sqlite.Open(dsn)` —
  без регистрации.
- `DB` — асинхронный пул: `SetMaxOpenConns` (ожидающие задачи — FIFO,
  ожидание отменяется контекстом; возвращаемое соединение передаётся
  старейшему ожидающему напрямую, минуя список простаивающих, так что
  пришедший позже не перехватывает его), `SetMaxIdleConns` (по умолчанию 2),
  `SetConnMaxIdleTime`, `SetConnMaxLifetime`, `SetHealthCheck` (ping
  простоявшего соединения перед выдачей, по умолчанию после 30 с),
  `SetStmtCacheSize` (кэш подготовленных выражений на соединение, 64),
  `Stats()`, `Close()`. Сломанные соединения (`IsValid() == false` после
  ошибки ввода-вывода или отмены) выбрасываются. Фоновой задачи-уборщика
  нет: просроченные соединения закрываются при выдаче и возврате.
- `Exec(q, args) Result!` (`RowsAffected`, `LastInsertId`), `QueryRows(q,
  args) Rows!` (`Next`, `Scan([&a, &b])`, `Get<T>(i)`, `Row<T>()`,
  `Values`, `Columns`, `ColumnTypes`, `Close`; соединение возвращается в
  пул на `Close`, в конце строк или при уничтожении `Rows`), `QueryRow`
  (`Row.Scan`, `ErrNoRows`), `Query<T>(q, args) []T!` и `QueryOne<T>` —
  столбцы в поля по `@sql.column("x")`, имени или snake_case имени
  (`@sql.column(skip: true)`), либо скаляр для одного столбца;
  преобразования в целые (с проверкой диапазона), `float`, `bool`,
  `string`, `[]byte`, `time.Time` (в том числе из текста), `big.Decimal`,
  `json.Value`, enum по имени, срезы (массивы), `*T` и `sql.Null<T>` для
  NULL, свои типы через `sql.Scanner`/`sql.Valuer`. Методы называются
  `QueryRows`/`Query<T>`, а не `Query`/`Query<T>`: перегрузки нет.
- Аргументы — `[]any`; именованные `:name` (`sql.Named("name", v)` или один
  `map[string]any`) переписываются в `$1..` или `?` по драйверу
  (`sql.RewriteNamed`; строки, идентификаторы в кавычках, комментарии и
  `::` пропускаются). Запрос без аргументов идёт через кэш подготовленных
  выражений, как запрос с аргументами, если в нём нет `;` и это не `COPY`;
  иначе — напрямую драйверу (у PostgreSQL — simple query: несколько команд,
  текстовые результаты).
- `Begin() Tx!` (`Exec`, `QueryRows`, `QueryRow`, `Query<T>`, `QueryOne<T>`,
  `Commit`, `Rollback`; идиома `defer tx.Rollback()` — после `Commit`
  ничего не делает и возвращает `ErrTxDone`), `Transact(f)`, `Prepare(q)
  Stmt!`, `Conn()` (выделенное соединение), `Raw(f)` (доступ к соединению
  драйвера). Таймауты — `task.WithTimeout`: отменённый запрос прерывается,
  его соединение выбрасывается.
- PostgreSQL (`database.postgres`, на Zell поверх `net`/
  `net.tls`): DSN `postgres://user:pass@host:port/db?sslmode=...` и
  `key=value`, переменные `PG*`, Unix-сокеты (`host=/dir`); sslmode
  `disable/allow/prefer/require/verify-ca/verify-full` (`sslrootcert`);
  аутентификация trust, password, MD5, SCRAM-SHA-256 (без channel
  binding); simple query (`Exec` без аргументов, запросы с `;` и `COPY`) и extended
  protocol с именованными подготовленными выражениями, бинарные результаты
  для bool/int2/4/8/oid/float4/8/bytea/uuid/timestamp(tz)/date/text/json(b),
  текст для остальных (numeric → строка → `big.Decimal`, interval, массивы
  → `[]any`); ошибки `postgres.Error{Severity, Code, Msg, Detail, Hint,
  Position, Table, Column, Constraint, ...}`; `Driver{OnNotice,
  OnNotification}`; `postgres.CopyFrom(db, "COPY ... FROM STDIN", data)`,
  `COPY ... TO STDOUT` — строки столбца `data`. Проверено против
  PostgreSQL 16 (`compiler/tests/database.rs`) и mock-сервера
  (`tests/spec/stdlib4/postgres_mock.zl`). Нет: CancelRequest при отмене
  (соединение просто закрывается), бинарного кодирования аргументов
  (передаются текстом, `[]byte` — бинарно), многомерных массивов,
  SCRAM-SHA-256-PLUS.
- SQLite (`database.sqlite`): системная libsqlite3 через C-обёртку;
  компилятор добавляет `-lsqlite3` только программам, использующим драйвер
  (`stdlib.rs` `LINK_LIBS`). DSN — путь, `:memory:` или URI `file:`, опции
  `_busy_timeout`, `_foreign_keys`, `_journal_mode`, `_synchronous`,
  `_txlock`; столбцы DATE/TIME → `time.Time`, BOOLEAN → `bool`; ошибки
  `sqlite.Error{Code, ExtendedCode, Msg}` с `IsConstraint()`. Вызовы
  синхронные (выполняются на потоке воркера).

### `zell.data` (ORM)

```zell
@table("users")
struct User {
    @key @autoincrement Id      int
    @column("email") @index(unique: true) Email string
    @maxLength(100)             Name    string
    @hasMany(foreignKey: "UserId") Orders  []Order
    @default("now()")           CreatedAt time.Time
}

struct ShopContext {
    data.Context
    Users  data.Set<User>
    Orders data.Set<Order>

    func Configure(m *data.ModelBuilder) {
        m.Entity<User>().HasIndex((u) => u.Email).IsUnique()
    }
}

var users = await ctx.Users
    .Where((u) => u.CreatedAt > since && strings.Contains(u.Name, "a"))
    .Include((u) => u.Orders)
    .OrderBy((u) => u.Email)
    .Skip(20).Take(10)
    .ToList()?

var u = await ctx.Users.Find(id)?
u.Name = "new"
await ctx.SaveChanges()?        // change tracking: UPDATE только изменённых полей
```

- **LINQ-подобные запросы на деревьях выражений.** Лямбда в `Where` — не
  замыкание, а дерево выражений (`Where(p expr<func(*T) bool>)`, DESIGN.md
  «Деревья выражений»), которое библиотека переводит в SQL; перевод
  кэшируется по месту лямбды. Предикат можно хранить в переменной,
  вернуть из функции, собрать в цикле (`expr.OrAll(ps)`) и использовать
  повторно. Что нельзя перевести в SQL — ошибка (компиляции, если её видно
  по типам; иначе запроса), а не молчаливая загрузка всей таблицы в
  память.
- Change tracking, unit of work, транзакции, оптимистичная блокировка
  (`@concurrencyToken`), ленивые и явные загрузки, split queries,
  пакетные вставки, raw SQL с маппингом, compiled queries по умолчанию.
- **Миграции**: `zell data migration add AddUsers` сравнивает модель со
  снимком и генерирует миграцию на Zell; `zell data update` применяет;
  миграции — обычный код в репозитории.
- Провайдеры: PostgreSQL, MySQL, SQLite; шаблон провайдера для остальных.
- Лёгкая альтернатива в том же пакете: `data.Dapper`-стиль
  (`db.Query<T>(sql, args)`) без трекинга.
- NoSQL: `zell.data.redis`, `zell.data.mongo` отдельными пакетами.

#### Расширенный SQL (2026-10-26)

| Возможность | SQLite | PostgreSQL | API |
|---|---|---|---|
| Подзапросы `EXISTS`/`NOT EXISTS` (в т.ч. коррелированные) | ✓ | ✓ | `data.Exists(data.Where(orders, (o) => o.UserId == u.Id))`, `data.NotExists(q)` |
| `IN (SELECT …)` | ✓ | ✓ | `data.In(u.Id, ctx.Orders.Select((o) => o.UserId))` |
| Скалярные подзапросы | ✓ | ✓ | `data.CountOf(q)`, `SumOf/MinOf/MaxOf/AverageOf(q, (o) => o.Total)`, `data.Value(proj)` |
| `LEFT JOIN`, `CROSS JOIN` | ✓ | ✓ | `q.LeftJoin(ctx.Orders, ok, ik, (u, o) => (u.Name, data.Nullable(o.Total)))`, `o == nil`, `q.CrossJoin(s, (a, b) => …)` |
| `RIGHT JOIN`, `FULL JOIN` | ≥ 3.39 (ошибка при выполнении иначе) | ✓ | `q.RightJoin(...)`, `q.FullJoin(...)` |
| `GROUP BY` + `HAVING`, составной ключ, `ORDER BY` агрегата | ✓ | ✓ | `GroupBy((o) => (o.A, o.B)).Where((g) => g.Sum((o) => o.Qty) > 3).OrderByDescending((g) => g.Count())` |
| `UNION [ALL]`, `INTERSECT`, `EXCEPT` | ✓ | ✓ | `a.Union(b)`, `UnionAll`, `Intersect`, `Except` (запросы и проекции) |
| CTE, `WITH RECURSIVE` | ✓ | ✓ | `q.AsCte("name")`, `q.Recursive((child, parent) => child.ParentId == parent.Id)` |
| Оконные функции | ✓ | ✓ (с приведениями) | `data.RowNumber/Rank/DenseRank(w)`, `Lag/Lead(x, w)` (указатель), `SumOver/AverageOver/MinOver/MaxOver(x, w)`, `CountOver(w)`, `w = data.Over().PartitionBy(…).OrderBy(…)` |
| `ON CONFLICT … DO UPDATE/DO NOTHING` | ✓ | ✓ | `Upsert`, `UpsertBy`, `UpsertWith(e, (x) => x.Email, (x) => x.Name)`, `InsertOrIgnore(e, (x) => x.Email)` |
| `RETURNING` в UPDATE/DELETE | ≥ 3.35 | ✓ | `q.ExecuteUpdateReturning(f, (u) => u.Id)`, `q.ExecuteDeleteReturning((u) => (u.Id, u.Name))` |
| Сырые фрагменты (параметры связаны) | ✓ | ✓ | `data.Sql1<string>("lower(?)", u.Name)`, `Sql`, `Sql2`, `Sql3`; `??` — буквальный `?` |
| `LIKE … ESCAPE` | ✓ | ✓ | `data.Like(u.Name, data.EscapeLike(q) + "%")` |
| `jsonb`-документы, пути | JSON-текст, `json_extract` | `jsonb`, `->>`/`#>>` с приведением | `@data.jsonb Meta Meta`; `u.Meta.Limits.Seats > 1` |
| Операторы `jsonb` | ошибка | ✓ | `JsonContains` (`@>`), `JsonContainedBy` (`<@`), `JsonHasKey` (`?`), `JsonHasAnyKey` (`?\|`), `JsonHasAllKeys` (`?&`), `JsonPathExists` (`@?`), `JsonPathMatch` (`@@`), `JsonSet` (`jsonb_set`) |
| Массивы `[]T` | JSON-массив, `len` | `text[]`/`bigint[]`… | `ArrayContains` (`@>`), `ArrayContainedBy`, `ArrayOverlaps` (`&&`), `x == data.AnyOf(u.Tags)`, `len` (`cardinality`) — операторы только PostgreSQL |
| `ILIKE`, регулярные выражения | ошибка | ✓ | `data.ILike`, `data.Regex` (`~`), `data.IRegex` (`~*`) |
| Полнотекстовый поиск | ошибка | ✓ | `data.Matches(data.ToTsVector("english", u.Bio), data.PlainToTsQuery("english", q))`, `WebSearchToTsQuery`, `ToTsQuery`, `TsRank` |
| `DISTINCT ON` | ошибка | ✓ | `q.DistinctOn((o) => o.UserId)` |
| Блокировки строк | ошибка | ✓ | `q.ForUpdate()`, `ForShare()`, `SkipLocked()`, `NoWait()` |
| `LISTEN`/`NOTIFY` | ошибка | ✓ | `await ctx.Notify("ch", "x")`, `var s = await ctx.Listen("ch")?; await s.C.Recv()`, `s.Close()` |
| GIN-индексы | обычный индекс | `USING gin` | `@data.index(using: "gin")`, `Migrator.CreateIndexUsing` |

#### Типы столбцов и NULL (2026-10-26)

Поле со значением — `NOT NULL`; `*T` и `database.sql.Null<T>` —
допускающий NULL столбец (`nil` ↔ SQL NULL, `x == nil` → `IS NULL`).

| Поле | SQLite | PostgreSQL | nil | Запрос |
|---|---|---|---|---|
| `@data.jsonb Meta Meta` (структура, map) | `TEXT` (JSON) | `jsonb` | — (`NOT NULL`) | `u.Meta.Plan == "pro"`: `json_extract(c, '$.Plan')` / `c ->> 'Plan'` |
| `@data.jsonb Meta *Meta` | `TEXT` | `jsonb` | SQL NULL (не JSON `null`) | `u.Meta == nil` → `IS NULL` |
| `*T`, `[]T`, `*time.Time` внутри документа | JSON `null` | JSON `null` | `nil` ↔ `null` | `u.Meta.Owner == nil` → `json_extract(c, '$.Owner') IS NULL` / `(c #>> '{Owner}') IS NULL` |
| `@data.jsonb Items []Item` | `TEXT` (JSON-массив) | `jsonb` | — (`nil` пишется как `[]`) | `data.Any(u.Items, (it) => it.Qty > 2)` → `EXISTS (SELECT 1 FROM json_each(c) AS e1 WHERE json_extract(e1.value, '$.Qty') > ?)` / `jsonb_array_elements(c) AS e1 WHERE ((e1 ->> 'Qty'))::bigint > $1`; `data.All/Count/Sum/Min/Max/Average`; `len(u.Items)` → `json_array_length` / `jsonb_array_length` |
| `@data.jsonb Extra *[]Item` | `TEXT` | `jsonb` | SQL NULL ≠ `[]` | `u.Extra == nil`; `data.Any(*u.Extra, …)` (у NULL нет элементов), `len(*u.Extra)` — NULL |
| `[]Item` внутри документа | JSON | JSON | `null` | `data.Any(u.Meta.Lines, …)`, `len(u.Meta.Lines)`: путь `#>` (в PostgreSQL — NULL, если там не массив) |
| `[]Item` без `@data.jsonb` | — | — | — | навигация, если у `Item` есть ключ (`Id`, `ItemId`, `@key`); иначе ошибка компиляции «… mark it @data.jsonb …» |
| `[]T` скаляров (`[]string`, `[]int`…) | `TEXT` (JSON-массив) | `text[]`, `bigint[]`… | — (`NOT NULL`, `nil` пишется как пустой массив) | `len(u.Tags)` → `json_array_length` / `cardinality` |
| `*[]T` скаляров | `TEXT` | `text[]`… | SQL NULL ≠ пустой массив | `u.Tags == nil` → `IS NULL`; `len(*u.Tags)` — NULL для NULL |
| `Status` (enum без полей) | `TEXT` (имя варианта) | `text` | — | `u.State == Status.Active` (имя — параметр) |
| `*Status` | `TEXT` | `text` | SQL NULL | `u.Review == nil`, `*u.Review == Status.Active`; родной `CREATE TYPE … AS ENUM` — через `@data.column(type: "mood")` (параметр текстом, тип выводит сервер) |
| `@data.uuidColumn Id string` (`*string`) | `TEXT` | `uuid` | SQL NULL для `*string` | `u.Ref == x`, `data.In` (строковые функции в PostgreSQL требуют `::text`); значения — `uuid.New()` (строка) |
| `time.Duration` (`*time.Duration`) | `INTEGER` (наносекунды) | `interval` (точность — микросекунды) | SQL NULL для указателя | `u.Ttl > limit`: параметр — наносекунды / `'N microseconds'` |
| `math.big.Decimal` (`*…`) | `TEXT` | `numeric` | SQL NULL для указателя | `u.Price > x` |

Правило массивов: `[]T` — всегда `NOT NULL`, `nil` и пустой срез
неразличимы (в Zell `[] == nil`) и пишутся пустым массивом; если нужно
различать «нет значения» и «пусто», поле — `*[]T` (`nil` — SQL NULL,
указатель на пустой срез — пустой массив). Внутри документа срез —
JSON-массив, а `nil` (или пустой срез) — JSON `null`. `interval` из
PostgreSQL читается в стиле `IntervalStyle = postgres` (`1 day
02:03:04.5`; месяц — 30 дней, год — 365.25 дня, как `extract(epoch)`).
Тип `Option<T>` в стандартной библиотеке отсутствует (в DESIGN.md это
только пример перечисления), поэтому столбцы `Option<T>` не
поддерживаются: используйте `*T`.

Правило `nil` для члена документа одно: член равен `nil`, если ключа нет
или в нём JSON `null` (оба оператора извлечения текста — `json_extract`,
`#>>` — дают тогда SQL NULL). Путь через NULL-документ даёт NULL
(семантика SQL): `u.Meta.Plan != "pro"` для строки с `Meta == nil` не
истинно, как и `u.Meta.Plan == "pro"`. Путь `u.Meta.Owner.Name` — один
путь `#>> '{Owner,Name}'` / `'$.Owner.Name'`; листья приводятся к типу поля
в PostgreSQL (`::bigint`, `::double precision`, `::boolean`,
`::timestamptz`). `encoding.json` пишет `time.Time` как RFC 3339 с
наносекундами, `time.Duration` — целым числом наносекунд (как Go).

Ошибки «не поддерживается» — при выполнении запроса (провайдер
известен только во время выполнения): `data: ... ILike is not supported
by sqlite (a PostgreSQL function)`; ошибки компиляции — у неверных типов
(`Sum` строк, `data.Sql` с нелитеральным текстом — при выполнении).
Захваченный запрос в лямбде делает её план некэшируемым (SQL зависит от
запроса).

#### Диалекты (провайдеры): плагин SQL

Ядро ORM (будущий `zell-lang/db`) не ветвится по провайдеру: оно обходит
деревья выражений и пишет общий SQL (SELECT, JOIN, GROUP BY/HAVING,
подзапросы, CTE, операции над множествами, стандартные оконные функции,
INSERT/UPDATE/DELETE), а всё, что различается, спрашивает у
`interface Dialect` (`data_dialect.zl`, как провайдеры EF Core; DDL —
встроенный `DdlDialect`, открытие соединений — `Opener`). Диалект
реализует все методы; ANSI-версии — функции `ansi<Метод>(d, …)`, к
которым он делегирует (методы интерфейса по умолчанию проверялись бы в
каждой программе):

| Метод | Что |
|---|---|
| `Name()` | `"sqlite"`, `"postgres"`: ключ реестра и имя в ошибках |
| `Open(dsn)` (`Opener`) | пул соединений драйвера |
| `BindArray(xs)` | значение столбца-массива (PostgreSQL — массив, SQLite — JSON) |
| `JsonExtract(col, keys, asJson)`, `JsonEach(arr, alias)`, `JsonElement(alias, t)`, `JsonLength(arr)` | пути в документ (`#>`/`#>>`, `json_extract`), элементы JSON-массива (`jsonb_array_elements`, `json_each`) и его длина — для `nil`-проверок членов, `data.Any/All` и `len` |
| `Quote(ident)`, `Placeholder(n)` | кавычки идентификаторов (DDL; запросы ядро пишет в ANSI-кавычках), `?` / `$n` |
| `Paging(take, skip)` | `LIMIT/OFFSET` (MSSQL/Oracle: `OFFSET … FETCH`) |
| `MaxParams()`, `MultiRowDefaultValues()` | пакетная вставка |
| `CastType(kind)`, `AggregateCast(f, isInt)` | типы `CAST`, приведение результатов `SUM`/`AVG` |
| `TranslateCall(b, name, args, t) bool!`, `TranslateMember(b, base, name, t) bool!` | вызовы и члены, которые ядро не знает или пишет иначе; `false` — к общей трансляции; `b *SqlBuilder` переводит поддеревья (`Value`, `Pred`), значения остаются параметрами |
| `Returning()`, `Lock(mode, wait)`, `DistinctOn()`, `Require(feature)` | `RETURNING`, `FOR UPDATE/SHARE`, `DISTINCT ON`, проверка версии (SQLite < 3.39 — без `RIGHT/FULL JOIN`) |
| `ColumnType`, `DefaultSql`, `ColumnDef`, `ZeroLiteral`, `AlterColumn`, `AddForeignKey`, `DropForeignKey`, `CreateIndex` | DDL миграций |

`Provider` (`data.Provider.Postgres`/`.Sqlite`) разрешается в `Dialect`
по имени через реестр (`data.RegisterDialect(d)`, `data.DialectByName`);
встроенные диалекты — `data_sqlite.zl` (`SqliteDialect`) и
`data_postgres.zl` (`PostgresDialect`), по файлу на будущий репозиторий
драйвера. Функции, которые есть только в одной базе (PostgreSQL:
`ILike`, `Json*`, `Array*`, `Matches`, …), объявлены в файле своего
диалекта с `@expr.call` и реализацией в памяти для `Compile()`; их
переводит только `TranslateCall` этого диалекта, другой отвечает ошибкой
`ILike is not supported by sqlite`. Планы кэшируются по месту лямбды и
диалекту.

#### Статус реализации (`data`)

Реализовано как `data` (пока нет реестра пакетов; `import data` —
короткие имена `@table`, `Context`, `Set<T>`, или полный путь
`data.X`) — обычный библиотечный код без поддержки в компиляторе
(2026-10-26; до этого модель и перевод лямбд жили в `sema/orm*.rs`).
Исходники — `compiler/src/sema/stdlib_src/data*.zl`: `data.zl`
(контекст, наборы, трекинг), `data_model.zl` (модель из рефлексии и
`Configure`, типизированный доступ к полям), `data_sql.zl` (перевод
деревьев выражений в SQL, кэш планов), `data_query.zl` (запросы),
`data_save.zl`, `data_migrate.zl`; CLI — `compiler/src/datacli.rs`
(собирает проект со сгенерированным `main`, печатающим
`data.ModelJson<C>()`); тесты — `tests/spec/data/` и
`compiler/tests/data.rs` (настоящий PostgreSQL 16 и CLI миграций).

- ✓ **Модель из рефлексии.** Атрибуты `@table("users")`, `@key`
  (`@key(generated: false)`), `@autoincrement`, `@column("email", type:
  "varchar(20)")`, `@index` / `@index(unique: true, name:)`,
  `@maxLength(n)`, `@required`, `@default("now()")`, `@concurrencyToken`,
  `@ignore`, `@hasMany(foreignKey:)`, `@hasOne(foreignKey:)`,
  `@belongsTo(foreignKey:)`, `@manyToMany(through:)`. Соглашения: ключ —
  `Id` или `<Тип>Id` (целый ключ генерируется базой), таблица — snake_case
  во множественном числе (`OrderLine` → `order_lines`), столбцы —
  snake_case, внешний ключ — `<Сущность>Id` (у `[]Order` — `Order.UserId`,
  у `*User` — `UserId` этой сущности), коллекции с обеих сторон без
  внешнего ключа — многие-ко-многим через `<a>_<bs>`; индексы на внешних
  ключах, `ON DELETE CASCADE` (`SET NULL` для nullable). Типы столбцов:
  целые, `float`, `bool`, `string`, `[]byte`, `time.Time`, `big.Decimal`,
  `json.Value`, простые enum (по имени), nullable — `*T` и `sql.Null<T>`.
  Метод `Configure(m *data.ModelBuilder)` контекста выполняется один раз,
  когда строится модель типа контекста (первый `data.Open<C>()`):
  `m.Entity<User>().HasIndex((u) => u.Email).IsUnique()`, `ToTable`,
  `HasKey`, `Ignore`, `Property(...)` с `HasColumnName/HasMaxLength/
  IsRequired/HasDefaultSql/IsConcurrencyToken/HasColumnType/
  ValueGeneratedOnAdd/Never`, `HasIndex(...)` с `IsUnique/HasName`,
  `HasMany/HasOne/BelongsTo/HasManyToMany(...)` с `WithForeignKey/
  UsingTable` (селекторы — деревья выражений, неизвестное поле — ошибка
  компиляции). Модель строится из таблиц рефлексии (`reflect.TypeOf<T>()`,
  атрибуты полей) для каждой сущности, достижимой из наборов контекста, и
  кэшируется на тип контекста; чтение строк и значения столбцов
  развёрнуты по `reflect.Fields<T>()` (без рефлексии во время
  выполнения). Ошибки типов полей (неотображаемый тип, `@maxLength` не у
  строки, `@autoincrement` не у целого) — ошибки компиляции из библиотеки
  (`reflect.Error`); поле-структура по значению — не навигация (нужен
  `*E` или `[]E`). Ошибки отношений (нет внешнего ключа, несовместимые
  типы ключей) — ошибка `data.Open` (`OpenDB` паникует).
- ✓ **Контекст и наборы.** `data.Open<C>(provider, dsn) *C!` и
  `data.OpenDB<C>(provider, *sql.DB) *C` (у SQLite `:memory:` — одно
  соединение); `ctx.Users.Add/AddRange/Attach/Update/Remove`,
  `Find(key)` (сначала карта идентичности), `Local()`, `Upsert(e)` и
  `UpsertBy(e, (u) => u.Email)` (`INSERT ... ON CONFLICT ... DO UPDATE`),
  `await ctx.SaveChanges()` — единица работы в одной транзакции:
  вставки пакетами до 100 строк одним `INSERT ... VALUES (...), (...)
  RETURNING` (ключи и значения `@default` читаются обратно), обновления
  только изменившихся столбцов (сравнение со снимком значений, снятым при
  загрузке или последнем сохранении), удаления; оптимистичная блокировка
  `@concurrencyToken` (целый токен увеличивается при каждом UPDATE; нет
  затронутой строки — `data.ConcurrencyError`, `data.IsConcurrencyError`,
  ничего не сохраняется). `ctx.Transaction(async () => { ... })` (вложенные
  присоединяются), `AsNoTracking()`, `ctx.Entry(e)` (`State()`,
  `ChangedColumns()`, `Reload()`, `Detach()`,
  `Collection((u) => u.Orders).Load()`, `Reference((o) => o.User).Load()`),
  `ClearTracker()`, `LogTo(func(string))` — журнал SQL с аргументами,
  `ctx.Raw<T>(sql, args)` (Dapper-стиль через `sql.Query<T>`, без
  трекинга), `ExecuteSql`, `EnsureCreated()`, `CreateScript()`.
- ✓ **Запросы на деревьях выражений** (DESIGN.md, «Перевод лямбд в
  SQL»): `Where`, `OrderBy/OrderByDescending/ThenBy/ThenByDescending`
  (значение или кортеж), `Skip/Take` (`LIMIT/OFFSET`), `Distinct`,
  `Select` в кортеж, структуру или скаляр, `Include` (по умолчанию
  `LEFT JOIN` с подзапросом при постраничной выборке и дедупликацией,
  `AsSplitQuery()` — отдельный запрос `IN (...)` на навигацию),
  `ToList/First/FirstOrDefault/Single/SingleOrDefault/Count/Any` (с
  предикатом и без), `All`, `Sum/Min/Max/Average`, `GroupBy(...)` с
  `Select((g) => (g.Key, g.Count(), g.Sum(...)))` и `Where` как `HAVING`,
  `Join(ctx.Orders, outerKey, innerKey, result)`,
  `ExecuteUpdate((s) => s.Set((u) => u.Name, v).SetWith((u) => u.N, (u) =>
  u.N + 1))`, `ExecuteDelete()`, `ToSql()`/`Args()`, `AsEnumerable()` —
  дальше обычные функции в памяти. В лямбдах: сравнения, `&&`/`||`/`!`,
  арифметика, `+` строк (`||`), `nil`-проверки и `*u.Age` у nullable,
  `time.Time` (`<`, `==`), `strings.HasPrefix/HasSuffix/Contains/ToUpper/
  ToLower/Trim`, `len`, `float()/int()`, `slices.Contains(ids, u.Id)`
  (`IN`), навигации (`data.Any(u.Orders, (o) => ...)`, `data.All`,
  `data.Count`, `data.Sum/Min/Max/Average`, `len(u.Orders)`,
  `o.User.Name`, `o.User == nil`), в `GroupBy` — `g.Key`, `g.Count()`,
  `g.Sum((o) => ...)`; константы и захваченные значения — всегда
  параметры (`?`/`$n`). Перевод лямбды кэшируется по её месту в модели
  контекста. Предикаты комбинируются (`expr.And/Or/AndAll/OrAll/Not`).
  Непереводимое, но видимое по типам (вызов своей функции, метод) —
  ошибка компиляции дерева выражений; зависящее от модели (поле
  `@ignore`) — ошибка запроса `expression cannot be translated to SQL:
  <почему>`.
- ✓ **Провайдеры** PostgreSQL и SQLite: кавычки идентификаторов,
  `$n`/`?`, `LIMIT/OFFSET` (`LIMIT -1` в SQLite), `RETURNING`, `ON
  CONFLICT`, `strpos`/`instr`, `starts_with`/`substr`, приведения сумм и
  средних в PostgreSQL, типы DDL (`bigint GENERATED BY DEFAULT AS
  IDENTITY`/`INTEGER PRIMARY KEY AUTOINCREMENT`, `timestamptz`, `numeric`,
  `jsonb`, `bytea`, `boolean`, `varchar(n)`), `now()` → `CURRENT_TIMESTAMP`.
- ✓ **Миграции.** `zell data migration add AddUsers` (`--project`,
  `--dir`, `--context`) сравнивает модель со снимком
  `migrations/snapshot.json` и пишет `migrations/<время>_AddUsers.zl` —
  структуру с `@migration("<время>_AddUsers")` и методами `Up/Down(m
  *data.Migrator)` (`CreateTable` со столбцами и внешними ключами,
  `DropTable`, `RenameTable`, `AddColumn`, `DropColumn`, `RenameColumn`,
  `AlterColumn`, `CreateIndex`, `DropIndex`, `AddForeignKey`,
  `DropForeignKey`, `Sql`, `SqlFor`); `zell data update [цель|0]` применяет
  или откатывает (`Down`) миграции, каждую в транзакции, с учётом в таблице
  `__migrations`; `zell data list`, `zell data script` (SQL для
  провайдера); `ctx.Migrate()` и `data.Migrations()` во время выполнения
  (список `@migration`-структур программы составляет компилятор).
- Нет (пока): MySQL и шаблона провайдера, составных ключей, ленивой
  загрузки, `ThenInclude`, сохранения графов (вставка связанных сущностей и
  строк join-таблиц вместе с владельцем), исправления обратных навигаций,
  `Where`/`OrderBy` после `Select` (до проекции — можно), анонимных записей
  в `Select` (кортежи и структуры есть), составного `g.Key` вне `Select`,
  кэша готовых statement-ов (кэшируется перевод лямбд, склейка запроса —
  при каждом вызове), интеграции журнала с `log`, `AlterColumn` в SQLite
  (комментарий в скрипте). Суммы `big.Decimal` в SQLite считаются в
  `REAL`. Отслеживаемые сущности, загруженные в обе стороны
  (`u.Orders[i].User == u`), держат друг друга (сборщика мусора нет):
  когда контекст освобождается, ссылочные и многие-ко-многим навигации его
  сущностей (и отсоединённых им) сбрасываются, чтобы разорвать циклы.

## 4. Логирование: `zell.log` уровня Serilog

```zell
var logger = log.New()
    .MinimumLevel(log.Debug)
    .MinimumLevelOverride("zell.data", log.Warning)
    .Enrich.WithProperty("App", "shop")
    .Enrich.WithMachineName()
    .Enrich.WithRequestId()
    .Enrich.FromContext()
    .WriteTo.Console(log.ConsoleOptions{Template: "{Timestamp:HH:mm:ss} [{Level:u3}] {Message}{NewLine}{Exception}"})
    .WriteTo.File("logs/app-.log", log.FileOptions{RollingInterval: log.Day, Retain: 7})
    .WriteTo.Seq("http://seq:5341")
    .WriteTo.OpenTelemetry()
    .Filter.ByExcluding((e) => e.Properties["Path"] == "/health")
    .CreateLogger()

logger.Info("user {UserId} bought {@Order} for {Amount:0.00}", id, order, total)
logger.Error(err, "payment failed for {UserId}", id)

using (log.Context.Push("RequestId", reqId)) { ... }   // или через async-контекст
```

- **Структурированные события, не строки**: шаблон сообщения разбирается
  **при компиляции** (это константа), имена свойств и их значения
  становятся полями события. `{@Order}` — сериализовать объект целиком,
  `{Amount:0.00}` — формат. Несовпадение числа плейсхолдеров и аргументов —
  ошибка компиляции.
- **Нулевая стоимость для отключённых уровней**: проверка уровня до
  вычисления аргументов, без аллокаций.
- **Sinks** (приёмники): Console (цветной, шаблон), File (ролл по времени и
  размеру, retention), Seq, Elasticsearch/OpenSearch, Loki, Splunk, Syslog,
  OpenTelemetry (OTLP), Email, Slack/Telegram webhook, любой через
  интерфейс `log.Sink` с батчингом и асинхронной очередью
  (`WriteTo.Async(...)`), пользовательские форматтеры (JSON, Compact JSON,
  Logfmt, текстовый шаблон).
- **Enrichers** (обогатители): свойство, машина, процесс, поток, задача,
  request id, trace/span id из `zell.diag`, окружение, версия, `FromContext`
  для `log.Context.Push` (контекст живёт в async-задаче, не в потоке).
- **Фильтры**, уровни по источникам, динамическое изменение уровня в
  рантайме, `log.Logger` как интерфейс для DI с категорией `log.For<T>()`.
- **Destructuring**: `{@Obj}` через compile-time рефлексию с учётом
  `@log(skip: true)` для секретов, политики для типов
  (`Destructure.ByTransforming<User>((u) => {Id: u.Id})`).
- Интеграция: `zell.data` логирует SQL с параметрами; журнал запросов
  одной строкой (как Serilog RequestLogging) вернётся с `zell-lang/web`.

### Статус реализации (`log`)

Пока нет реестра пакетов, логгер живёт в стандартной библиотеке как
`log` (`compiler/src/sema/stdlib_src/log.zl`, C-помощники файлового
приёмника — `runtime/src/diag.c`). Пример — `examples/log.zl`, тесты —
`compiler/tests/log_diag.rs`, `tests/spec/stdlib/log_*`.

- **Шаблоны компилируются.** Вызов `logger.Info("...", args)` (у любого
  `log.Logger` и у `*log.Pipeline`) и `log.Info("...", args)` (логгер по
  умолчанию) разбирает `sema/logtmpl.rs`: шаблон обязан быть строковым
  литералом, число плейсхолдеров должно совпадать с числом аргументов
  (иначе ошибка компиляции: `Logger.Info: the template has 2 placeholders
  ({UserId}, {Amount}) but 1 argument is given`), синтаксис проверяется
  (`{Name}`, `{@Name}`, `{$Name}`, `{0}`, `,align`, `:format`, `{{`/`}}`).
  Вызов опускается в `log.enabled(l, lvl) && log.emit(l, lvl, err, text,
  parts, names, formats, values)`: литеральные куски, имена и форматы —
  константы, значения упакованы в `any` (`{@X}` — через
  `log.destructure<T>` на compile-time рефлексии с `@log(skip: true)`,
  `{$X}` — текст `println`, числа/строки/bool/ошибки — напрямую, прочее —
  `log.capture<T>`). Уровень проверяется **до** вычисления аргументов, если
  получатель — переменная/поле (иначе получатель вычисляется один раз, а
  уровень проверяет `emit`).
- Уровни — `log.Level.Verbose/Debug/Information/Warning/Error/Fatal`
  (а не `log.Debug`: это имя занято функцией `log.Debug("...")`); методы
  `Verbose`, `Debug`, `Information`/`Info`, `Warning`/`Warn`, `Error`,
  `Fatal`, каждый принимает ошибку первым аргументом (`Error(err, ...)`
  добавляет свойства `ErrorMessage` и `ErrorTrace` = `errors.Trace(err)`).
  Текстовые синки в `{Exception}` пишут сообщение и одну строку — где
  ошибка создана; всю трассу возврата — только у событий `Debug` и
  `Verbose` (свойство `ErrorTrace` в JSON её хранит всегда).
- Конфигурация: `log.New()`, `MinimumLevel`, `MinimumLevelOverride(prefix,
  lvl)` (по категории, самый длинный префикс), `MinimumLevelControlledBy(
  log.NewLevelSwitch(lvl))` (смена уровня на ходу), `Clock(f)` (тестовые
  часы), `Enrich.WithProperty/WithMachineName/WithProcessId/FromContext/
  With(e)/WithFunc(f)`, `WriteTo.Console(log.ConsoleOptions{Template,
  Stdout, Colors})`, `WriteTo.File(path, log.FileOptions{Template, Json,
  RollingInterval: log.RollingInterval.Day|Hour|Month|Year, FileSizeLimit,
  Retain})`, `WriteTo.Json(log.JsonOptions{Path, Stdout})` (CLEF:
  `@t @mt @m @l @x` + свойства), `WriteTo.Sink(s)`, `WriteTo.Async(s,
  capacity)` (канал + фоновая задача; `await logger.Close()` дренирует),
  `Filter.ByExcluding/ByIncludingOnly(func(*log.Event) bool)`,
  `CreateLogger() log.Logger`.
- `log.Logger` — интерфейс (`IsEnabled`, `Write`, `ForContext`,
  `ForCategory`, `Close`); `log.For<T>()` — логгер по умолчанию с
  категорией `SourceContext` = полное имя `T`; `log.Global()` /
  `log.SetGlobal(l)` вместо присваиваемого `Log.Logger` (в исходниках stdlib
  нет глобальных переменных; состояние — в слотах рантайма);
  `log.CloseAndFlush()`. По умолчанию — `INFO msg` в stderr с уровня
  Information.
- `log.Event{Timestamp (Unix мс), Level, Template, Message, Properties
  map[string]any, Error, Category}` плюс `Keys()` (порядок свойств), `Get`,
  `Has`, `Set`, `AddIfAbsent`, `Render(literal)`. `Message` отрисован как
  `{Message:lj}` (строки без кавычек); `{Message}` в шаблоне вывода — с
  кавычками, как в Serilog. Шаблон вывода: `Timestamp:<формат .NET>`,
  `Level:u3|w3|t3|u4|w4|u|w`, `Message[:lj]`, `NewLine`, `Exception`,
  `Properties[:j]`, `SourceContext`/`Category`, любое свойство; форматы
  чисел `0.00`, `F2`, `N2`, `E2`, `P1`, `D5`, `x`/`X`.
- Стоимость: встроенные приёмники (`Console`, `File`, `Json`, `Async` над
  ними) и обогатители (`WithProperty`, `FromContext`) собирают строку
  записи прямо из частей скомпилированного шаблона, без карты свойств и
  без промежуточной строки сообщения; `Event.Properties` и `Event.Message`
  заполняются только в логгере с фильтром, пользовательским обогатителем
  или пользовательским приёмником (до их вызова). JSON-запись с четырьмя
  свойствами и двумя свойствами контекста — ~10 аллокаций: 6 в вызове
  (срезы `parts`/`names`/`formats`/`values` и упаковка строковых
  аргументов в `any`) и 4 в `log` (событие, добавленные свойства, строка
  записи, форматирование `float`); выключенный уровень — 0
  (`benchmarks/logging`).
- `log.Context.Push(name, value)` кладёт свойство в контекст задачи (кадр
  `task`-контекста с ключом -101, наследуется дочерними задачами) и
  возвращает `ContextScope`; `scope.Pop()` (нет `using` — пишите `defer
  scope.Pop()`). `Enrich.FromContext()` добавляет эти свойства и
  `TraceId`/`SpanId` текущего span'а `diag`.
- Нет: Seq, Elasticsearch/Loki/Splunk/Syslog/OTLP-logs/Email/webhook
  приёмников, батчинга (кроме `Async`), `Destructure.ByTransforming`,
  `Enrich.WithRequestId`/потока/окружения, logfmt, интеграции с
  `zell.data`, `{@X}` для enum с полями (печатается имя типа).

## 5. Диагностика: `zell.diag` (аналог pprof и dotnet-counters)

- **Профилирование CPU**: сэмплирующий профилировщик в рантайме
  (сигнал/таймер, стек через frame pointers, без остановки мира):
  `diag.StartCPUProfile(w)`, HTTP-эндпоинт `/debug/pprof/profile` в
  формате pprof (`profile.proto`), совместим с `go tool pprof`, `pprof`
  (Google) и Speedscope; `zell tool pprof` — своя обёртка с flame graph.
- **Память**: подсчёт ссылок даёт точные данные без снимков GC: живые
  объекты по типу и месту аллокации (`/debug/pprof/heap`, `/debug/heap`),
  аллокации с сэмплированием (`allocs`), детектор утечек (`--leak-check`)
  и циклов, метрики сборщика циклов.
- **Задачи**: `/debug/pprof/tasks` — все живые async-задачи с их трассами
  ожидания (аналог goroutine dump), блокировки (`mutex`, `block`).
- **Трассировка выполнения**: `diag.Trace` — события планировщика,
  ввода-вывода, GC-подобных пауз нет, но есть паузы на синхронизацию;
  просмотр в Perfetto UI (формат Chrome trace).
- **Метрики**: `diag.Counter/Gauge/Histogram`, экспорт Prometheus
  (`/metrics`) и OpenTelemetry; встроенные метрики рантайма (память,
  задачи, очередь планировщика, HTTP, SQL).
- **Распределённая трассировка**: OpenTelemetry-совместимые `Span`,
  распространение контекста через async-задачи, W3C traceparent в HTTP и
  SQL автоматически.
- **CLI**: `zell tool pprof`, `zell tool trace`, `zell tool counters
  <pid>` (как dotnet-counters: живые метрики процесса), `zell tool dump
  <pid>` (снимок задач и памяти), `zell tool stack <pid>`. Подключение к
  запущенному процессу через Unix-сокет диагностики, включается
  `ZELL_DIAG=1` или `diag.Enable()`.
- **Бенчмарки**: `@bench` функции, `zell bench` с статистикой
  (среднее, p50/p99, аллокации), сравнение с базой (`--compare`).
- **Отладка**: DWARF по умолчанию, `zell tool debug` поверх lldb с
  форматтерами типов Zell (см. docs/TOOLING.md).

### Статус реализации (`diag`)

Живёт в `diag` (`compiler/src/sema/stdlib_src/diag.zl`, рантайм —
`runtime/src/diag.c`, `runtime/src/profile.c`, сэмплер в
`runtime/src/platform/posix.c`). Примеры — `examples/metrics.zl`,
`examples/tracing.zl`, `examples/cpuprofile.zl`; тесты —
`compiler/tests/log_diag.rs`.

- **Метрики**: `diag.NewCounter(name, help, labels)` (`Inc`, `Add`,
  `With(values)`, `Value`), `NewGauge` (`Set`, `Inc`, `Dec`, `Add`, `Sub`),
  `NewHistogram(name, help, labels, buckets)` (`Observe`; `DefBuckets()`);
  повторная регистрация имени возвращает то же семейство.
  `diag.PrometheusText()` — формат 0.0.4; `diag.ServeMetrics` — функция-
  обработчик для `Router` (`r.Get("/metrics", diag.ServeMetrics)`),
  `diag.MetricsHandler()` — `http.Handler`. Рантайм: `diag.Runtime()`
  (`LiveObjects`, `Allocations`, `Tasks`, `QueueLength`, `Workers`, а также
  память кучи в байтах: `HeapInUse` — страницы с объектами, средние и
  большие объекты; `HeapRetained` — свободная, но ещё не отданная ОС память
  (резерв, входит в RSS); `HeapReleased` — свободные страницы, уже отданные
  ОС, чьи адреса сохранены; `runtime/ABI.md` §3.2) и
  `diag.EnableRuntimeMetrics()` (метрики `zell_*` в `PrometheusText`, в
  том числе `zell_heap_inuse_bytes`, `zell_heap_retained_bytes`,
  `zell_heap_released_bytes`). `runtime.FreeOSMemory()` отдаёт ОС всю
  свободную память кучи сразу.
- **Трассировка**: `diag.StartSpan(name)` / `StartSpanKind(name, kind)` →
  `*diag.Span` (поля OpenTelemetry: `TraceId` 16 байт hex, `SpanId` 8,
  `ParentSpanId`, `Kind`, `StartNanos`/`EndNanos`, `Attributes`, `Status`,
  `StatusMessage`), `SetAttribute`, `SetStatus`, `RecordError`, `defer
  span.End()`. Текущий span — кадр контекста задачи (ключ -102):
  наследуется дочерними задачами, а изменения внутри `async`-функции не
  видны вызывающему (рантайм восстанавливает контекст вызывающего при
  первой приостановке, ABI.md 10.8). `diag.AddExporter(e)`, интерфейс
  `diag.Exporter`, `diag.ConsoleExporter{ShowDuration}`,
  `diag.NewOtlpExporter(endpoint)` (OTLP/HTTP JSON на `/v1/traces`,
  пачками по `BatchSize`, остаток — `await diag.Shutdown()`),
  `diag.SetTraceSeed(seed, clock)` для воспроизводимых id и времени.
  W3C `traceparent`: `http.Client` добавляет заголовок текущего span'а,
  `http.Server` делает span из заголовка родителем на время обработчика;
  `diag.Traceparent()`, `diag.ParseTraceparent(h)`.
- **CPU-профиль**: `diag.StartCPUProfile(path)` / `StartCPUProfileRate`,
  `diag.StopCPUProfile() int!` (число сэмплов). `ITIMER_PROF` + `SIGPROF`,
  обработчик обходит frame pointers в пределах стека потока (границы
  запоминаются при старте воркеров) и пишет в предвыделенный буфер без
  блокировок; при остановке адреса символизируются по `.symtab`
  исполняемого файла (ELF, Linux; иначе `dladdr`) и записываются в
  `profile.proto` (ручное protobuf-кодирование, без сжатия). Код Zell
  собирается с `"frame-pointer"="all"`, рантайм — с
  `-fno-omit-frame-pointer`. Проверено `go tool pprof -top`:
  `demo.p1.fib` flat, `spin`/`main` — cum. Сборки `--release` стрипаются,
  поэтому в их профилях только адреса.
- **Heap**: `diag.WriteHeapProfile(path)` (pprof, `inuse_objects` /
  `inuse_space` по типам) и `diag.HeapProfile()` (текст) — из списка живых
  объектов детектора утечек, поэтому только под `zell run --leak-check`
  без `--release`; места аллокаций и сэмплирование `allocs` — нет.
- **Живые объекты по типам в продакшене**: с `ZELL_DIAG_HEAP=1` в
  окружении (читается при старте; в любой сборке, в том числе
  `--release`) каждая аллокация и освобождение обновляют счётчик своего
  `TypeInfo` в таблице потока (без атомарных операций на общем пути,
  суммируются по запросу; без переменной — одна предсказуемая проверка).
  `diag.HeapByType() []diag.HeapType` (`Type`, `Live`, `Allocated`,
  `Bytes` = живые × размер типа), самые многочисленные первыми; под
  `--leak-check` — из точного списка живых объектов (`Allocated` = -1);
  `diag.HeapCounting()` — есть ли данные. `diag.HeapReport(limit)` —
  текстовая таблица топ-`limit` типов с приростом каждого с прошлого
  отчёта (состояние общее для процесса); `diag.ServeHeap` — обработчик
  для `Router` (`r.Get("/debug/heap", diag.ServeHeap)`, топ-50),
  `diag.HeapHandler()` — `http.Handler`. Растущий от запроса к запросу
  тип — кандидат в утечку (обычно цикл, который не освободил ни `weak`,
  ни сборщик циклов).
- **Сборщик циклов**: `diag.EnableRuntimeMetrics()` добавляет в
  `PrometheusText` `zell_cycles_collected_total` (объекты, освобождённые
  сборщиком), `zell_cycle_collections_total`, `zell_cycle_candidates`
  (возможные корни, ждущие сборки) и `zell_cycle_candidates_total`;
  подробности — `runtime.CycleStats()` (DESIGN.md «Циклы»).
- **CLI**: `zell tool pprof [флаги] файл` вызывает `go tool pprof` (или
  `pprof`).
- Нет: эндпоинтов `/debug/pprof/*`, профилей задач/блокировок, `diag.Trace`
  (Perfetto), `zell tool trace/counters/dump/stack`, сокета диагностики,
  `@bench`/`zell bench`, `traceparent` в SQL, сэмплера на Windows.

## 6. Прочие «плюшки» уровня .NET

- `zell.config` — конфигурация с провайдерами (файлы, env, args, Vault,
  Consul), горячая перезагрузка.
- `zell.cache` — in-memory и distributed (Redis) cache с единым API.
- `zell.jobs` — фоновые задачи, cron, очереди (аналог Hangfire).
- `zell.validation` — `@validate` плюс FluentValidation-подобный API.
- `zell.mapper` — маппинг DTO через compile-time рефлексию, без
  рефлексии в рантайме (аналог Mapster/AutoMapper, но проверяется при
  компиляции).
- `zell.mediator` — CQRS, пайплайны обработчиков.
- `zell.test` — `@test`, `@fact`/`@theory` с данными, моки через
  интерфейсы (`mock.Of<UserRepo>()` генерируется при компиляции),
  snapshot-тесты, тестовый HTTP-сервер.
- `zell.cli` — парсер команд и аргументов с генерацией help из атрибутов.
- `zell.auth` — JWT, OIDC, API keys, политики.
- `zell.realtime` — SignalR-подобные хабы поверх WebSocket.
- `zell.messaging` — Kafka, RabbitMQ, NATS клиенты и абстракция шины.
- Шаблоны проектов: `zell new service`, `zell new worker`, `zell new lib`.

## Что для этого нужно от языка (требования к компилятору)

| Возможность | Кто использует |
|---|---|
| Generics с мономорфизацией и ограничениями через интерфейсы | collections, `data.Set<T>`, `json.Parse<T>`, DI |
| Compile-time рефлексия `reflect.Fields<T>()`, `Methods<T>()`, атрибуты как значения | json/xml/yaml/toml, маршруты, ORM, логгер, mapper, mock |
| Compile-time вычисление константных строк (разбор шаблона лога, маршрута, SQL) | log, data |
| Лямбда как дерево выражений при параметре типа `data.Expr<func(T) bool>` | ORM-запросы |
| Async-контекст (аналог AsyncLocal) в рантайме | log.Context, трассировка, DI scope |
| Статические таблицы типов и урезание неиспользуемых | рефлексия в рантайме для `json.ParseValue`, OpenAPI |
| Frame pointers и DWARF по умолчанию, таймер-сэмплирование в рантайме | pprof |
| Async-сокеты и таймеры в планировщике | http, sql, messaging |

## Порядок реализации

1. Generics, compile-time рефлексия, атрибуты как значения (компилятор).
2. `encoding.json` (+ Value, Reader/Writer), `net` async-сокеты,
   `net.http` 1.1, `zell.log` с Console/File/JSON.
3. `zell.web`: маршруты, биндинг, middleware, DI, конфигурация, OpenAPI
   (было в stdlib, вынесено до `TypesWith`, тег `web-in-tree`).
4. `database.sql` + PostgreSQL/SQLite, `zell.data` базовый (✓ как `data`)
   (запросы, трекинг, миграции).
5. `zell.diag`: CPU/heap/tasks профили в формате pprof, метрики,
   OpenTelemetry; `zell tool pprof/trace/counters`.
6. xml/yaml/toml, HTTP/2, TLS, auth, cache, jobs, messaging, остальное.

## План: тонкое ядро, фреймворк в пакетах

Решение (2026-10-22): в стандартной библиотеке остаётся тонкий слой, как
у Go; фреймворк живёт в отдельных репозиториях организации `zell-lang` и
подключается пакетным менеджером (`import github.com/zell-lang/web`).

**Остаётся в std:** `net`, `net.http` (клиент и сервер HTTP/1.1 и HTTP/2,
TLS, WebSocket, роутер по пути и методу без атрибутов), `encoding.*`,
`crypto`, `database.sql` (интерфейс драйверов), базовый `log`, `testing`.

**Выносится:**

| Пакет | Что | Что мешает вынести сейчас |
|---|---|---|
| `zell-lang/web` | атрибутная маршрутизация, DI, middleware, binding, OpenAPI, auth/JWT | нужен `reflect.TypesWith<A>()`; до него вынесен из дерева (2026-10-24, тег `web-in-tree`; было: таблица маршрутов и DI в компиляторе, `sema/web.rs`) |
| `zell-lang/data` | ORM: модель, change tracking, миграции, лямбда → SQL | ничего в компиляторе (с 2026-10-26 обычный код на деревьях выражений и статической рефлексии); нативный слой SQLite и `diag.rt*` — см. `docs/EXTRACTION.md` |
| `zell-lang/log`, `zell-lang/diag` | Serilog-подобный логгер; Prometheus, OpenTelemetry, pprof | ничего: обычный код |
| `zell-lang/redis`, `kafka`, `s3` | клиенты | ничего: обычный код |

**Две возможности языка, без которых `web` и `data` не станут обычными
пакетами** (полезны сами по себе, не костыли под фреймворк):

1. **Статическая рефлексия по программе**: `reflect.TypesWith<Controller>()`
   возвращает все типы программы с атрибутом, вычисляется компилятором при
   сборке, как `reflect.Fields<T>()`. На ней пакет `web` строит таблицу
   маршрутов и DI-контейнер без хуков компилятора. **Готово (2026-10-24)**:
   `reflect.TypesWith<A>()` по всей программе (включая зависимости;
   библиотека видит типы приложения), `TypeInfo.Package`,
   `MethodInfo.Params []*ParamInfo` с атрибутами параметров (для
   `@fromQuery`/`@fromBody`), `reflect.Error(msg)` — ошибка компиляции из
   библиотеки (DESIGN.md «Типы с атрибутом и ошибки из библиотек»). Вызов
   действий — `m.Invoke(recv, args)` (аргументы `[]any` после биндинга) или
   `m.Call<F>()` при фиксированной сигнатуре; статическое развёртывание
   `for t in TypesWith<A>()` — следующий шаг (`docs/REFLECTION_PLAN.md`).
2. **Деревья выражений**: параметр типа `expr<func(User) bool>` получает
   лямбду как синтаксическое дерево-значение (аналог `Expression<Func<>>` в
   C#); на нём ORM транслирует `Where(u => u.Age > 18)` в SQL уже в пакете.
   Сделано 2026-10-26: тип `expr<F>` и пакет `expr` (DESIGN.md «Деревья
   выражений»), запросы `data` на деревьях (`sema/orm*.rs` удалены).

**Порядок:**

1. Две возможности выше: спека (`DESIGN.md`, грамматика), компилятор,
   спек-тесты, примеры.
2. `web`: уже удалён из дерева (исходники — тег `web-in-tree`, корень
   `web` снят со `STD_ROOTS`); пишется заново на `TypesWith` сразу в
   `zell-lang/web` (манифест, тег `v0.1.0`, CI), пример
   `example-web-service` переходит на `import github.com/zell-lang/web`.
   `data` переписан на деревья выражений внутри основного репозитория,
   `sema/orm*.rs` удалены (2026-10-26).
3. Перенос `data` в `zell-lang/data`; корень `data` снимается с
   зарезервированных корней stdlib (`compiler/src/sema/stdlib.rs`,
   `STD_ROOTS`).
4. `log`/`diag`, `redis`/`kafka`/`s3` переносятся следом тем же путём.

Открытые вопросы: держать ли `data` в одном репозитории с `web`; объединять
ли `redis`/`kafka`/`s3` в один `zell-lang/cloud`.
