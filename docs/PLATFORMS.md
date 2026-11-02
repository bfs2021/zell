# Платформы и кроссплатформенность

Zell должен работать одинаково на Linux, macOS и Windows, на amd64 и
arm64. Это требование к компилятору, рантайму, стандартной библиотеке и
инструментам, а не задача «портировать потом».

## Целевые платформы

| Tier | Платформы | Гарантии |
|---|---|---|
| 1 | linux-amd64, linux-arm64, macos-arm64, windows-amd64 | CI на каждый коммит, релизные бинарники, все тесты зелёные |
| 2 | macos-amd64, windows-arm64, linux-musl (amd64/arm64), freebsd-amd64 | CI на релиз, бинарники |
| 3 | wasm32-wasi, android-arm64, ios-arm64 | компилируется, тесты по возможности |

Имена целей — LLVM target triples: `x86_64-unknown-linux-gnu`,
`aarch64-apple-darwin`, `x86_64-pc-windows-gnu` и т. д., с короткими
псевдонимами `linux-amd64`, `macos-arm64`, `windows-amd64`. Сейчас
`windows-amd64` — это ABI MinGW (`x86_64-pc-windows-gnu`: рантайм собран с
заголовками mingw-w64, линковка с его CRT и импорт-библиотеками); ABI MSVC
(`x86_64-pc-windows-msvc`, CodeView/PDB, библиотеки Windows SDK) — позже,
пока только `check` и `--emit=obj`.

## Кросс-компиляция

`zell build --target windows-amd64` работает с любой машины:

- LLVM генерирует код под любую цель из одного бинарника компилятора.
- Линковщик `lld` (`bin/lld` тулчейна, один бинарник на ELF, Mach-O и
  PE/COFF/MinGW) запускается самим `zell`, без clang.
- Рантайм и всё для линковки поставляется для всех платформ tier 1-2 в
  архиве тулчейна (`<prefix>/targets/<triple>/`: `libzell_rt.a`,
  compiler-rt, crt-объекты, заглушки glibc 2.31, musl `libc.a`, CRT и
  импорт-библиотеки mingw-w64, `libSystem.tbd`) — см. «Дистрибутив».
  Дополнительные цели скачиваются `zell target add` в `~/.zell/targets`.
- Условная компиляция: `@cfg(os: "windows")`, `@cfg(arch: "arm64")`,
  `@cfg(not(os: "linux"))`, `@cfg(any(...))`, `@cfg(all(...))`,
  `@cfg(feature: "tls")` на объявлениях (и их полях, методах, вариантах) и
  перед `namespace` (весь файл); суффиксы файлов `foo_windows.zl`,
  `foo_linux.zl`, `foo_macos.zl`, `foo_amd64.zl`, `foo_arm64.zl`,
  `foo_linux_arm64.zl` как в Go. Правила — `DESIGN.md` «Платформы».
- Константы `os.Name` (`"linux"|"macos"|"windows"|...`), `os.Arch`
  (`"amd64"|"arm64"|...`), `os.PathSeparator`, `os.LineSeparator`.

## Рантайм: слой абстракции

Весь платформозависимый код рантайма живёт в `runtime/src/platform/` за
одним внутренним интерфейсом `platform/platform.h`. Остальной рантайм
(строки, срезы, map, ошибки, планировщик) платформонезависим.

| Область | Linux | macOS | Windows |
|---|---|---|---|
| Потоки, мьютексы, условные переменные | pthreads | pthreads | Win32 threads, SRWLock, CONDITION_VARIABLE |
| Атомики | C11 `<stdatomic.h>` | C11 | C11 (clang) |
| Асинхронный ввод-вывод | epoll, edge-triggered (EPOLLET, регистрация один раз на fd; опрашивается воркерами планировщика, ABI.md 10.9; io_uring по `ZELL_IO_BACKEND`, one-shot, со своими потоками) | kqueue (one-shot) | IOCP (one-shot) |
| Таймеры | timerfd / планировщик | kqueue EVFILT_TIMER | waitable timers |
| Файлы и каталоги | POSIX | POSIX | Win32 (`CreateFileW`, UTF-16 пути) |
| Процессы | fork/exec, posix_spawn | posix_spawn | `CreateProcessW` |
| Сеть | BSD sockets | BSD sockets | Winsock2 |
| Backtrace при панике | `execinfo` / libunwind | `execinfo` | `CaptureStackBackTrace` + DbgHelp |
| `файл:строка` в backtrace | таблица `zell_lines` (одна для всех) | та же | та же |
| Раскрутка паники задачи | `__builtin_setjmp` (x86-64), `_setjmp` (arm64) | `_setjmp` | `_setjmp(buf, NULL)` (без SEH-раскрутки) |
| Случайные байты | `getrandom(2)` | `getentropy` | `BCryptGenRandom` |
| Время | `clock_gettime` | `clock_gettime` | `QueryPerformanceCounter`, `GetSystemTimePreciseAsFileTime` |
| Динамическая загрузка (FFI) | `dlopen` | `dlopen` | `LoadLibraryW` |
| Консоль | UTF-8 | UTF-8 | UTF-8 через `WriteConsoleW` конверсию, ANSI-цвета включаются |

**Паника и backtrace одинаковы на всех платформах** (с 2026-10-24):
паника в задаче завершает только задачу (runtime/ABI.md 8.1), трасса
печатает `  <файл>:<строка> <функция>` для кадров Zell-кода из таблицы
`zell_lines` (ABI.md 8.2), которую компилятор строит из DWARF объектных
файлов (ELF, Mach-O, MinGW COFF) и линкует отдельным объектом — адреса
в ней 32-битные смещения от самой таблицы, их разрешает статический
линковщик, поэтому ни поправки на адрес загрузки, ни чтения файла
программы. Различается только остальное: кадры рантайма и libc
(`модуль(символ+0xсмещение)`, на Windows `модуль(символ+0xсмещение) [0xадрес]`
с именами из DbgHelp/COFF), `testing.Panics` (перехват на своём потоке
есть только на POSIX: на Windows тест перезапускается в дочернем
процессе), сборки ThinLTO (`ZELL_LTO=1`, только не Windows) — без таблицы.
MSVC ABI (CodeView вместо DWARF) таблицы не получает. На Windows и macOS
проверено: `tests/spec/panics/*` под wine (`scripts/spec-wine.sh`),
macOS — только линковка (`ld64.lld`) и разбор таблицы в собранном
Mach-O; запуск на Mac — шаг `smoke` в `.github/workflows/release.yml`.

Правила:

- Пути и строки внутри языка всегда UTF-8; на границе Win32 конвертация в
  UTF-16. `fs` принимает и `/`, и `\` на Windows, отдаёт платформенный
  разделитель.
- Переводы строк: `println` пишет `\n` везде; текстовые функции чтения
  понимают `\r\n`.
- Сигналы: `os.OnInterrupt(handler)` абстрагирует SIGINT/SIGTERM и
  `SetConsoleCtrlHandler`.
- `process`, `os`, `fs`, `net`, `time` реализуются
  поверх этого слоя, без `#ifdef` вне `platform/`.

## Компилятор и инструменты

- Сам компилятор собирается под все платформы tier 1 (Rust + статический
  LLVM), распространяется как один бинарник + архив рантаймов.
- Установка: `curl ... | sh` (Linux/macOS, `scripts/install.sh`), `irm ...
  | iex` (PowerShell, `scripts/install.ps1`) — сделано, см. «Установка и
  цели» ниже; Homebrew, winget, apt/rpm-репозитории, `zell self update` —
  позже.
- LSP, `fmt`, пакетный менеджер — платформонезависимы по построению
  (Rust), хранилище пакетов использует жёсткие ссылки на NTFS/APFS/ext4
  и копирование там, где их нет.
- Отладка: DWARF на Linux/macOS, CodeView/PDB на Windows (LLVM умеет
  оба), чтобы работали lldb, gdb и Visual Studio/WinDbg.

## CI

Матрица GitHub Actions: `ubuntu-latest` (amd64), `ubuntu-24.04-arm`,
`macos-latest` (arm64), `macos-13` (amd64), `windows-latest`. На каждой:
сборка компилятора, `make test` рантайма, `cargo test`, прогон
`examples/` и `tests/spec/`, кросс-сборка hello world под все tier-1 цели
с запуском под QEMU/Wine там, где возможно. Размер бинарника hello world
и бенчмарки отслеживаются по платформам.

## Текущее состояние

Что работает на каждой платформе и **как это проверено** (здесь — Linux
x86-64 с clang-18, mingw-w64, Wine 9.0, qemu-user 8, musl-tools; macOS и
настоящего Windows нет):

| Платформа | Рантайм (C-тесты) | Примеры (`.out` байт-в-байт) | Как проверено |
|---|---|---|---|
| linux-amd64 | 19/19 `make -C runtime test` | 49/49 (`cargo test`) | нативно |
| linux-arm64 | 19/19 `make -C runtime CROSS=aarch64-linux-gnu test-cross` | 47/49 (`--leak-check`) | кросс-сборка + `qemu-aarch64-static -L /usr/aarch64-linux-gnu`; CI — ещё и нативно на `ubuntu-24.04-arm` |
| linux-amd64-musl | 19/19 `make -C runtime CROSS=x86_64-linux-musl test-cross` | 47/49, полностью статические | нативно (статический ELF без `PT_INTERP`) |
| windows-amd64 (MinGW) | 19/19 `make -C runtime CROSS=x86_64-w64-mingw32 test-wine` (IOCP и `ZELL_POLLER=wsapoll`) | 38 + 5 сетевых (IOCP и WSAPoll) + sql с `sqlite3.dll` | Wine 9.0 |
| macos-arm64 / amd64 | все 94 файла и C-тесты компилируются (`-Werror`) против `runtime/stub-sdk/macos` | 49/49 → Mach-O объекты | без SDK: ничего не слинковано и не запущено; на Mac — `scripts/macos-verify.sh` |

Не прошедшие 2 из 49 на arm64/musl — `sql` и `web_app` (последний удалён вместе с `web`, тег `web-in-tree`): им нужен
`libsqlite3` цели (в кросс-sysroot arm64 его нет, статического для musl
тоже; рантайм musl собран без `<sqlite3.h>`, `database.sqlite`
отвечает «not available»). Всё перечисленное — `compiler/tests/cross.rs`
(тесты пропускаются с объяснением, если инструмента нет) и кросс-джоб CI.

Размер hello world (`scripts/bench-size.sh`, `--release`): linux-amd64 —
34 КБ (glibc динамически; `--static` — 843 КБ), **linux-amd64-musl — 70 КБ
полностью статический** (C hello через musl-gcc — 17 КБ), linux-arm64 —
23 КБ, windows-amd64 — 81 КБ (msvcrt.dll динамически, рантайм и CRT
mingw статически; C hello — 14 КБ).

- **Слой `runtime/src/platform/` завершён**: вне `platform/` в рантайме нет
  ни одного `#ifdef` ОС и ни одного прямого вызова POSIX/Win32
  (`runtime/ABI.md` §15). Через слой идут потоки, мьютексы/условные
  переменные/once/TLS (`zell__plat_mutex` и т. д.: pthreads inline в
  `platform.h`; SRWLOCK/CONDITION_VARIABLE/INIT_ONCE/FLS в `win32.c`; имени
  `zell_mutex` нет — оно занято ABI-типом `Mutex`), часы (`mono_ns`,
  `wall_ns`, `sleep_ns`), календарь (`utc_tm`/`timegm` на чистом C),
  `strftime`/`strptime` (свои в `timefmt.c` там, где libc неполна:
  Windows и musl — `ZELL_PLAT_LIBC_TIMEFMT`), файлы и каталоги,
  процессы, сигналы, пользователи, окружение, консоль, aligned-память,
  сокеты, поллер, backtrace с именами функций, сэмплер CPU, часовые пояса,
  хранилище корневых сертификатов, `dlopen`.

### windows-amd64 (MinGW ABI): кросс-сборка с Linux, запуск под Wine

- `make -C runtime CROSS=x86_64-w64-mingw32` собирает
  `runtime/build/x86_64-pc-windows-gnu/libzell_rt.a` через `clang -target
  x86_64-w64-mingw32` с `-Wall -Wextra -Werror` (без clang —
  `x86_64-w64-mingw32-gcc`; clang предпочтителен: тот же native TLS, что у
  кода LLVM, gcc для mingw использует emutls).
- `zell build --target windows-amd64 x.zl` → `x.exe` (PE32+, COFF от LLVM,
  соглашение Win64; на границе ABI только скаляры и указатели). Линковка:
  `clang -target x86_64-w64-mingw32 -fuse-ld=lld` + рантайм + ws2_32,
  iphlpapi, bcrypt, shell32, advapi32; ntdll, crypt32, dbghelp и
  sqlite3/winsqlite3 загружаются во время выполнения (`LoadLibraryW`/
  `GetProcAddress`), импорт-библиотеки им не нужны. `zell run --target
  windows-amd64` запускает через Wine (`$ZELL_WINE`).
- Под Wine 9.0 байт-в-байт с Linux-ными `.out` (без нормализации CRLF:
  `println` пишет `\n`, stdout/stderr в двоичном режиме): 38 примеров в
  отладочной сборке с `--leak-check` (hello, arith, control, functions,
  structs, switch, enums, slices_maps, strings, interfaces, errors,
  errors_trace, defer, bank, async, cancel, channels, drop, generics, weak,
  json, reflection, regex, sealed_shapes, try_block, task_context, tracing,
  log, metrics, cli_tool, stdlib_* ×6, multi, **cpuprofile**), 7 в
  `--release`, сетевые tcp_echo, http_server, tls_echo, redis,
  http_realtime — с IOCP-поллером и ещё раз с `ZELL_POLLER=wsapoll`;
  `sql` — с `sqlite3.dll` рядом с программой (`$ZELL_TEST_SQLITE3_DLL`,
  собран из amalgamation через mingw); процессы, часовые пояса, корневые
  сертификаты и backtrace — отдельным тестом (`cross.rs`
  `windows_processes_zones_roots_backtraces`, вывод совпадает с Linux).
  Не применимы: cfg (Windows-вариант ссылается на несуществующий `winapi`),
  stdlib_time_os_fs (пути `\` и буквы дисков против Unix-путей в `.out`).
- C-тесты рантайма под Wine: **19 из 19** (в т. ч. `test_net`: он переписан
  на сокеты платформенного слоя и собирается для Windows; AF_UNIX в Wine 9
  нет — sendfile там проверяется по loopback TCP; Windows 10 1803+ AF_UNIX
  умеет). Тестовый код (не рантайм) использует winpthreads и POSIX-шимы из
  `tests/test.h`; `run_child` вместо `fork` перезапускает тест с
  `ZELL_TEST_CHILD`. Wine нужна UTF-8-локаль (`LC_ALL=C.UTF-8`).
- Что есть в Windows-слое: потоки `CreateThread`; часы
  `QueryPerformanceCounter`/`GetSystemTimePreciseAsFileTime`; энтропия
  `BCryptGenRandom`; `LoadLibraryW`; argv в UTF-8 из `CommandLineToArgvW`;
  консоль — `WriteConsoleW` с переносом неполных UTF-8-последовательностей,
  `CP_UTF8` и ANSI-цвета; файлы через `CreateFileW` + `_open_osfhandle`,
  UTF-16 и оба разделителя, `pread` без сдвига позиции; `fs` разбирает
  `/` и `\`, буквы дисков и отдаёт `\`; сигналы — `SetConsoleCtrlHandler`;
  пользователи — только текущий (`GetUserNameW`, `USERPROFILE`), групп нет.
  Новое:
  - **Поллер IOCP** (`poll_iocp.c`, ниже) вместо WSAPoll; WSAPoll остался
    запасным (`poll_wsapoll.c`: если `\Device\Afd` не открывается, или
    `ZELL_POLLER=wsapoll`). Исправлено в нём: `remove` теперь будит
    ожидающий `WSAPoll` и ждёт его возврата, прежде чем вызывающий закроет
    сокет — под Wine закрытие сокета внутри идущего `WSAPoll` вешало
    стресс-тест `test_net` (раньше `test_net` под Windows не собирался, и
    этого не было видно).
  - **`SendFile` — `TransmitFile`** (указатель через
    `SIO_GET_EXTENSION_FUNCTION_POINTER`, без mswsock.lib): OVERLAPPED с
    событием, по куску до 64 КБ за вызов (вызывающий уже дождался
    готовности к записи, ядро берёт кусок сразу; если пир не читает 30 с —
    `CancelIoEx`, отдаётся отправленное). Запасной путь — `pread`+`send`.
  - **Процессы без потоков-ретрансляторов**: с IOCP-поллером каналы
    ребёнка — именованные каналы (`\\.\pipe\zell-<pid>-<n>`, один
    экземпляр, `PIPE_REJECT_REMOTE_CLIENTS`), родительский конец
    OVERLAPPED. Дескриптор — `0x400000 + слот` таблицы каналов (не путается
    с сокетами, CRT-дескрипторами и HANDLE). Готовность к чтению —
    нулевое OVERLAPPED-чтение `ReadFile` (как «zero read» в libuv), его
    завершение приходит в порт шарда; чтение неблокирующее
    (`PeekNamedPipe` + `ReadFile` того, что есть, EOF по
    `ERROR_BROKEN_PIPE`); запись отдаёт до 64 КБ OVERLAPPED-`WriteFile` и
    сразу возвращается, следующая ждёт (EAGAIN) завершения предыдущей, а
    готовность к записи — это завершение записи «в полёте». С WSAPoll
    остаются ретрансляторы (WSAPoll видит только сокеты). Проверено под
    Wine (нулевое чтение ждёт данных и завершается в порт, как в Windows):
    3 МБ в stdin ребёнка, stdout и stderr — в обоих режимах.
  - **Сэмплер CPU** (`prof_win32.c`): поток-сэмплер с
    высокоточным waitable-таймером `hz` раз в секунду; по
    `GetThreadTimes` считает, сколько CPU потратил каждый поток процесса
    (список — снимок Toolhelp раз в 10 тиков) и берёт по сэмплу на каждые
    1/hz CPU-секунды: `SuspendThread` → `GetThreadContext` → раскрутка
    стека `RtlLookupFunctionEntry`/`RtlVirtualUnwind` по `.pdata` (цепочка
    rbp кода Win64 ненадёжна: clang ставит rbp со смещением) →
    `ResumeThread`. Пока поток заморожен, сэмплер не выделяет память и не
    берёт локов (раскрутчик может взять лок таблиц функций загрузчика — это
    важно только для JIT с динамическими таблицами). Профиль — тот же
    pprof, что на Linux (`examples/cpuprofile.zl` проходит;
    `go tool pprof -top` показывает функции Zell).
  - **Имена в backtrace и профиле**: своя COFF-таблица символов exe (lld и
    ld пишут её для mingw, если не `-s`: все функции, локальные тоже — как
    ELF `.symtab` на Linux), затем DbgHelp `SymFromAddr` (загружается по
    требованию: экспорт и PDB системных DLL, под Wine — и DWARF). Формат
    как у glibc: `app.exe(zell_main__inner+0x101) [0x14002f7f1]`.
    `--release` (без символов) — модуль+смещение. Тот же формат и та же
    таблица символов теперь и на Linux/macOS/musl (`posix.c`: ELF
    `.symtab`, Mach-O `LC_SYMTAB` из `__LINKEDIT`; без `<execinfo.h>`, как
    в musl, — проход по frame pointer).
  - **Часовые пояса** (`tz_win32.c`): правила из реестра
    (`HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Time Zones\<ключ>`:
    `TZI` и `Dynamic DST` по годам) превращаются в TZif v2 (переходы за
    годы из `Dynamic DST` + POSIX-строка текущего правила для будущих лет),
    который `time.zl` разбирает как файл tzdata. Локальная зона —
    `GetDynamicTimeZoneInformation` (`TimeZoneKeyName`; выключенный
    переход на летнее время — круглый год стандартное). IANA-имена
    (`time.LoadLocation("Europe/Berlin")`) — по встроенной таблице из 182
    имён для 139 ключей Windows (CLDR `windowsZones.xml`, территория 001,
    и частые синонимы) с аббревиатурами (`CET`/`CEST`; у Windows только
    длинные имена; без известных букв — числовые `+03`, как в tzdata). `$TZ` с именем и
    `$ZONEINFO` с каталогом tzdata имеют приоритет. Под Wine 9 вывод
    совпадает с Linux-tzdata для 9 зон × 6 моментов (в т. ч. южное
    полушарие и St_Johns −3:30), локальная зона — по `TZ` хоста.
    Ограничение: история до первого года `Dynamic DST` неизвестна (раньше —
    стандартное смещение этого года).
  - **Корневые сертификаты**: хранилище `ROOT` (`CertOpenSystemStoreW`,
    crypt32 по требованию) выгружается раз за процесс в PEM-файл
    `%LOCALAPPDATA%\zell\roots.pem` (атомарная замена), путь которого и
    отдаётся `x509.SystemRoots` (кэш по размеру/mtime работает как с любым
    бандлом). `$SSL_CERT_FILE` — в приоритете. Оговорка: Windows
    докачивает редкие корни по требованию (Automatic Root Update), корня,
    который ни разу не использовался на машине, в хранилище может не быть.
  - **SQLite**: `database.sqlite` на Windows связывается во время
    выполнения (`platform/sqlite_dyn.h`, `sqlite_win32.c`): сначала
    `sqlite3.dll` приложения (рядом с exe или в `PATH`), затем системный
    `winsqlite3.dll` (Windows 10+, System32); без них `rtAvailable()` —
    false и `Open` возвращает «not available». `-lsqlite3` для Windows не
    передаётся. `sqlite3_changes64` (3.37+) — с откатом на
    `sqlite3_changes`.
- Сеть (`net_win32.c`): `WSASocketW` (overlapped, не наследуется),
  `FIONBIO`, `getaddrinfo`/`getnameinfo`, `GetAdaptersAddresses`, опции
  (SO_REUSEADDR в смысле POSIX — поведение Windows по умолчанию, плюс
  `SO_EXCLUSIVEADDRUSE`; REUSEPORT и CORK — ENOPROTOOPT), AF_UNIX
  (Windows 10 1803+, без абстрактных имён), сообщения об ошибках как на
  Linux.

### Асинхронный ввод-вывод на Windows: IOCP за интерфейсом готовности

Интерфейс (`platform.h`) — **готовность** (readiness): `poller_arm(fd,
READ|WRITE, token)` просит одно уведомление, `poller_wait` отдаёт
`{token, events}`, после чего вызывающий сам делает неблокирующий
`recv`/`send`, а при `EAGAIN` снова ждёт. epoll и kqueue ложатся на него
напрямую; IOCP — модель **завершения**. Выбран адаптер готовности (а не
перевод сокетов на настоящий overlapped-ввод-вывод с байтами в
завершении): он меньше, сохраняет ABI и весь код `task_io.c`/`std_net.c`,
а Zell-код и так видит только «результат или ждать». `poll_iocp.c`:

1. **Порт на шард.** Каждый поллер (= шард `task_io.c`, как шарды epoll)
   создаёт свой порт завершения и открывает свой хэндл драйвера AFD
   (`\Device\Afd` — ядро Winsock; `NtCreateFile` из ntdll по
   `GetProcAddress`), привязанный к порту.
2. **Готовность — `IOCTL_AFD_POLL`** (как wepoll, mio/tokio, `uv_poll` в
   libuv): взвод сокета — одна операция опроса по его базовому хэндлу
   (`SIO_BASE_HANDLE`) с событиями AFD интереса (receive/accept/disconnect
   для READ, send для WRITE, abort/connect-fail/local-close для обоих).
   Драйвер завершает её, как только сокет в этом состоянии
   (level-triggered: уже имеющаяся готовность — сразу), завершение
   приходит в порт с записью сокета в качестве контекста — это и есть
   one-shot-уведомление. Нулевые `WSARecv`/`WSASend` отвергнуты: нулевой
   `WSASend` завершается сразу, есть место в буфере или нет (холостой цикл
   на полном сокете), а нулевой `WSARecv` не сообщает о `accept`.
3. **Смена интереса и снятие.** Если взвод добавляет направление, пока
   опрос в полёте (писатель к читателю), опрос отменяется (`CancelIoEx`), и
   его отменённое завершение перевзводит с текущим интересом. `remove`
   отменяет и освобождает запись по последнему завершению (до него
   `IO_STATUS_BLOCK` принадлежит ядру); токены с поколением отбрасывают
   устаревшие отчёты, как раньше.
4. **Пробуждение** — `PostQueuedCompletionStatus` со служебным ключом (как
   eventfd), схлопывается флагом; ожидание — `GetQueuedCompletionStatusEx`
   пачкой до 256. **Таймеры** — прежние кучи дедлайнов `task_io.c`
   (таймаут ожидания), как на Linux.
5. **Каналы процессов** ждутся тем же портом (см. выше): нулевое
   OVERLAPPED-чтение для READ, завершение записи «в полёте» для WRITE.
6. `AcceptEx`/`ConnectEx` не нужны: `accept`/`connect` неблокирующие, а
   их готовность сообщает AFD (`AFD_POLL_ACCEPT`, `AFD_POLL_SEND` /
   `AFD_POLL_CONNECT_FAIL`).

Под Wine 9.0 `IOCTL_AFD_POLL` работает (Wine сам реализует на нём
`WSAPoll`): `test_net` (one-shot, пробуждения, шарды, гонки закрытия,
2400 сокетных пар в 8 потоках, sendfile) и все сетевые примеры проходят с
обоими бэкендами. Бенчмарки под Wine (только ориентир: Wine, общая машина
с другими процессами, разброс между прогонами до 2×), `--release`:

| | IOCP (AFD) | WSAPoll |
|---|---|---|
| `tcp_echo` (1000 соединений × 1 МБ) | 118–219 МБ/с | 67–90 МБ/с |
| `http_hello` (64 соединения, 200 000 запросов) | 1670–2810 запросов/с | 2210–2510 запросов/с |

`tcp_echo` выигрывает за счёт того, что ожидание не перебирает 1000+
взведённых сокетов; `http_hello` на 64 соединениях упирается в другое
(эмуляция Wine), разница в пределах шума. Настоящие числа — на
Windows (CI-джоб windows-amd64).

### linux-arm64 и musl

- `make -C runtime CROSS=aarch64-linux-gnu` (clang `-target
  aarch64-linux-gnu` находит sysroot `gcc-aarch64-linux-gnu`) →
  `runtime/build/aarch64-unknown-linux-gnu/`; `zell build --target
  linux-arm64` линкует `clang --target=aarch64-unknown-linux-gnu
  -fuse-ld=lld` (с `$ZELL_SYSROOT`, если задан), `zell run --target
  linux-arm64` запускает через `qemu-aarch64[-static] -L
  /usr/aarch64-linux-gnu` (`$ZELL_QEMU`, `$ZELL_SYSROOT`). CI: нативный
  джоб `ubuntu-24.04-arm` (полный `cargo test`) и кросс-джоб (C-тесты и
  примеры под QEMU).
- `--target linux-amd64-musl` (`x86_64-unknown-linux-musl`; также
  `linux-arm64-musl` при наличии musl-sysroot): всегда `-static`.
  Рантайм — `make -C runtime CROSS=x86_64-linux-musl` (clang с
  заголовками musl-tools), линковка — `musl-gcc` (`$ZELL_MUSL_CC`), если
  он для того же CPU, иначе clang с `--sysroot=$ZELL_SYSROOT`. musl нет
  `<execinfo.h>` (backtrace — по frame pointer), его `strftime` без
  `%k`/`%l`, `strptime` без `%z` (используются свои из `timefmt.c`);
  `-lsqlite3` не передаётся.

### macOS: что сделано без Mac и что проверить на настоящем

Сделано: `make -C runtime CROSS=aarch64-apple-darwin` (и
`x86_64-apple-darwin`) без SDK компилирует **все 94 файла рантайма и все
C-тесты** (`-Wall -Wextra -Werror`, `clang -target arm64-apple-macos12
-nostdlibinc`) против стаб-заголовков `runtime/stub-sdk/macos/include/`
(75 заголовков, только используемые объявления, по публичным заголовкам
SDK: `sys/event.h`, `mach-o/*`, `malloc/malloc.h`, `pthread.h`,
`sys/socket.h`, `sys/_mcontext.h`…). Объекты кладутся в
`build/<triple>-stubcheck/` и никогда не линкуются (архива в
`build/<triple>/` нет, `zell build` его не найдёт). С `SDKROOT` или на Mac
та же команда собирает настоящий рантайм. Найдено и исправлено:
`SOCK_NONBLOCK` в тестах (его нет на macOS), неиспользуемая переменная в
ветке символов; macOS-ветки `posix.c`/`fs_posix.c`/`net_posix.c` и
`poll_kqueue.c` компилируются. Добавлено: символы из `LC_SYMTAB` главного
образа (`_dyld_get_image_header` + `__LINKEDIT`) для профиля и backtrace,
`exe_range` для pprof; корни — `/etc/ssl/cert.pem` (поддерживается macOS;
пользовательские корни Keychain не входят). `zell build --target
macos-arm64|macos-amd64 --emit=obj` даёт Mach-O для всех 49 примеров.

Не проверено (нужен настоящий Mac; `scripts/macos-verify.sh` делает всё
это и печатает PASS/FAIL по пунктам, macOS-джоб CI его запускает):

1. Сборка рантайма с настоящим SDK и `make test` (19 тестов): поведение
   kqueue (one-shot через `EV_DISPATCH`, `EVFILT_USER`-пробуждение,
   EV_EOF/ошибки), `pthread_cond_timedwait` с `CLOCK_REALTIME`,
   `malloc_size`, `st_mtimespec`, поиск в `PATH` вместо `execvpe`,
   `sendfile(2)` BSD-сигнатуры, `SO_NOSIGPIPE`, `TCP_KEEPALIVE`;
   `make tsan`.
2. Расположение структур и констант стабов против SDK (проверка — тот же
   стаб-check на Mac с `SDKROOT=`: должен проходить и дальше).
3. Линковка `zell build` (`-isysroot`, `ld64`/`ld64.lld`,
   `-dead_strip`) и `cargo test --workspace` (все примеры байт-в-байт).
4. Сэмплер `SIGPROF` (`ITIMER_PROF`, регистры из `__darwin_mcontext64`)
   и символы `LC_SYMTAB` — `examples/cpuprofile.zl` и backtrace с именами.
   Паника: `tests/spec/panics/*` (в т.ч. `backtrace_lines*`: `файл:строка`
   из `zell_lines`, раскрутка задач через `_setjmp`/`_longjmp` на arm64).
5. Часовые пояса из `/usr/share/zoneinfo`/`/etc/localtime`, корни из
   `/etc/ssl/cert.pem`, `process` (`posix_spawn`), сетевые примеры.
6. `scripts/package-toolchain.sh` на Mac (архив `zell-<ver>-macos-<arch>`).

### Установка и цели (дистрибуция тулчейна)

- `scripts/package-toolchain.sh [--no-build] [--out dist] [--targets dir]
  [--lld file]` → самодостаточный `zell-<версия>-<хост>.tar.xz` (состав —
  «Дистрибутив» ниже) и по `zell-runtime-<версия>-<triple>.tar.gz`
  (распаковывается в `<triple>/` — тот же каталог входов линковки) на
  каждую цель, плюс `.sha256` и `VERSION`. Без `build/targets` (не
  запускали `scripts/bundle-targets.sh`) пакуется только рантайм хоста, и
  линковка требует clang.
- Компилятор ищет рантайм хоста в `$ZELL_RUNTIME`, затем
  `<exe>/../targets/<triple>/` установленного тулчейна,
  `$ZELL_TARGETS/<triple>/`, затем в репозитории (для других целей — тот
  же порядок без `$ZELL_RUNTIME`); `std/` — в
  `$ZELL_STD`, `<exe>/../std`, репозитории (иначе встроенные копии).
- `zell target list` — известные цели (tier 1–2) и где лежит их рантайм;
  `zell target add <цель>... [--url шаблон]` скачивает
  `zell-runtime-<версия>-<triple>.tar.gz` в `$ZELL_TARGETS` (по
  умолчанию `$ZELL_HOME/targets` = `~/.zell/targets`), сверяет с
  `<url>.sha256`, если он есть, и распаковывает системным `tar`. Шаблон —
  `$ZELL_TARGETS_URL`, по умолчанию
  `https://github.com/zell-lang/zell/releases/download/v{version}/zell-runtime-{version}-{triple}.tar.gz`
  (`{version}`, `{triple}`; `https://` — через curl, `http://` и `file://`
  — встроенно, путь без схемы — локальный архив).
- `scripts/install.sh` (Linux/macOS: `curl … | sh`, `--version`,
  `--dir`, `--bin`, `--from <архив>`, `$ZELL_DIST_URL`) распаковывает в
  `~/.zell/toolchain/zell-<ver>-<host>/`, ставит ссылку `current` и
  ссылки `zell`/`zell-lsp` в `~/.local/bin`; `scripts/install.ps1`
  (Windows, `irm … | iex`; здесь не запускался) — в
  `%USERPROFILE%\.zell\toolchain`, `bin\` в пользовательский `PATH`,
  распаковка встроенным `tar.exe`.
- Проверено: архив распакован в чистый каталог, `env -i … ZELL_HOME=…
  bin/zell build hello.zl` берёт рантайм и `std/` из тулчейна (видно в
  `-v`), `zell target add windows-amd64 linux-amd64-musl` с
  `file://`-шаблоном ставит рантаймы в `$ZELL_HOME/targets`, после чего
  `hello.exe` работает под Wine, а musl-бинарник — статически;
  `install.sh --from` и `ZELL_DIST_URL=file://…` ставят и запускают.
  С октября 2026 архив самодостаточен (см. «Дистрибутив»): системный
  `clang` больше не нужен.

### Релиз-кандидаты (pre-release)

Кандидат выпускается тем же `release.yml`, тег не нужно создавать заранее:

```sh
gh workflow run release.yml -f tag=v0.1.0-rc.1 -f dry_run=false -f hosts=all
```

Тег — `v<версия>` или `v<версия>-rc.<n>` (также `-beta.<n>`, `-alpha.<n>`);
с `Cargo.toml` сверяется только часть `v<версия>`. Архивы, архивы рантаймов
и `VERSION` называются по тегу без `v` (`zell-0.1.0-rc.1-macos-arm64.tar.xz`;
`package-toolchain.sh --version`), а GitHub Release создаётся с
`--prerelease` на собранном коммите (`gh release create --target <sha>`:
тег появляется на сервере, workflow ничего не пушит). Pre-release никогда
не становится `releases/latest`, поэтому `install.sh` без `--version` его
не видит. Коллеги ставят кандидат так:

```sh
curl -fsSL https://raw.githubusercontent.com/zell-lang/zell/master/scripts/install.sh | sh -s -- --version 0.1.0-rc.1
```

или скачивают архив со страницы релиза. Пока бинарники не подписаны, на
macOS нужно снять карантин: `xattr -dr com.apple.quarantine
~/.zell/toolchain/zell-0.1.0-rc.1-macos-arm64`. `zell target add` по
умолчанию ищет `…/download/v<CARGO_PKG_VERSION>/zell-runtime-<CARGO_PKG_VERSION>-…`,
для кандидата задайте `ZELL_TARGETS_URL` (в самодостаточном архиве все
рантаймы уже есть). Повторный запуск с тем же тегом докладывает активы
(`gh release upload --clobber`); `dry_run=true` (по умолчанию) только собирает
артефакты.

### Общее

- Условная компиляция (`compiler/src/cfg.rs`, `target.rs`): `@cfg` с
  `os`/`arch`/`feature`/`not`/`any`/`all` на объявлениях, членах и файлах,
  суффиксы имён файлов (в `driver::collect_files`, то есть и в пакетах, и
  в LSP), константы `os.Name`/`Arch`/`PathSeparator`/`LineSeparator` —
  все следуют `--target` и в `check`, и в `build`/`run`.
- CI (`.github/workflows/ci.yml`): Linux amd64/arm64 (нативно, артефакт
  тулчейна), macOS arm64/amd64 (`cargo test`, `scripts/macos-verify.sh
  --quick`, артефакт), Windows (MSYS2 MINGW64: рантайм и C-тесты нативно с
  IOCP и с WSAPoll, компилятор с LLVM 23), кросс-джоб на Linux (Windows +
  Wine, linux-arm64 + QEMU, musl, стаб-check macOS, `cross.rs` целиком,
  размеры, архивы рантаймов). Здесь он не запускался.

**Пробелы**: ABI MSVC целиком (рантайм под MSVC, Windows SDK, `lld-link`,
PDB/CodeView-символы); запуск на настоящих macOS и Windows (всё выше —
Wine и стабы); Keychain macOS (только `/etc/ssl/cert.pem`); история
часовых поясов Windows до `Dynamic DST`; `libsqlite3` для кросс-целей
linux-arm64/musl; `AcceptEx`/`ConnectEx` и настоящий
completion-путь передачи данных (`ReadInto`/`Write` прямо в буфер) — не
нужны для корректности, могут дать производительность; FreeBSD и
wasm32-wasi.

## Дистрибутив

**Решение** (владелец проекта): архив тулчейна самодостаточен, как у Zig.
Пользователь распаковывает один архив и собирает, запускает и
кросс-компилирует под все цели tier 1-2, ничего больше не устанавливая
(ни clang, ни libc6-dev, ни mingw-w64, ни macOS SDK). Для Linux
поставляются **и** заглушки glibc (как abilists у Zig: динамические
исполняемые файлы для обычных систем), **и** musl (полностью статические).

### Состав архива

```text
zell-<версия>-<хост>/
  bin/zell, zell-lsp, zell-mcp
  bin/lld (+ ссылки ld.lld, ld64.lld, lld-link; на Windows lld.exe и DLL MSVC-рантайма)
  std/  tools/  THIRD_PARTY.md  README.md  VERSION
  targets/SOURCES.sha256                      URL и sha256 всех скачанных входов
  targets/<triple>/BUNDLE                     что это за каталог (читает `zell doctor`)
  targets/<triple>/libzell_rt.a               рантайм, собранный под цель, без отладочной информации
  targets/<triple>/libclang_rt.builtins.a     compiler-rt 18.1.8 (__udivti3, LSE-атомики arm64, ___chkstk_ms…)
  targets/{x86_64,aarch64}-unknown-linux-gnu/ Scrt1.o crti.o crtn.o crtbeginS.o crtendS.o libc_nonshared.a,
                                              заглушки libc.so libm.so libpthread.so libdl.so librt.so libutil.so
                                              ld-linux-*.so.*, libsqlite3.so
  targets/{x86_64,aarch64}-unknown-linux-musl/ crt1.o crti.o crtn.o crtbegin.o crtend.o libc.a (musl 1.2.5)
  targets/x86_64-pc-windows-gnu/              crt2.o dllcrt2.o libmingw32.a libmingwex.a libmoldname.a libmsvcrt.a,
                                              импорт-библиотеки kernel32, ws2_32, iphlpapi, bcrypt, … (mingw-w64 11.0.1)
  targets/{aarch64,x86_64}-apple-darwin/      libSystem.tbd
```

Размеры (linux-amd64, `xz -9`): архив 28.8 МБ (gzip — 71 МБ, поэтому
`.tar.xz`; для Windows дополнительно `.tar.gz` для старых `tar.exe`);
`targets/` — 33 МБ распакованными (Windows-цель — 16 МБ из-за
импорт-библиотек), `bin/lld` — 56 МБ, `bin/zell` — 67 МБ. Цель — не
больше 80 МБ на архив хоста.

### Готовые входы (релиз `toolchain-inputs`)

Сборка входов медленная и почти не меняется: скачивание ~10 пакетов
(Ubuntu, mingw-w64, исходники musl и compiler-rt), сборка musl и
compiler-rt, сборка lld из LLVM 18. Поэтому ручной workflow
`.github/workflows/inputs.yml` (`gh workflow run inputs.yml`) собирает их
один раз и кладёт в GitHub Release `toolchain-inputs` (тег
`toolchain-inputs`, pre-release, чтобы не стать `releases/latest`).

Входы линковки делятся на два слоя с разными ключами: рантайм
(`runtime/`) меняется часто, а заглушки glibc, musl, mingw CRT и
compiler-rt — почти никогда и собираются долго.

- `targets-external-<ekey>.tar.xz` — внешний слой для Linux- и
  Windows-целей: заглушки glibc, crt-объекты, musl, mingw CRT и
  импорт-библиотеки, compiler-rt, `BUNDLE`, `SOURCES.sha256`;
  `targets-external-macos-<ekey>.tar.xz` — compiler-rt обеих
  apple-darwin целей (собран на Mac против настоящего SDK);
- `targets-runtime-<rkey>.tar.xz` — `libzell_rt.a` Linux- и Windows-целей;
  `targets-runtime-macos-<rkey>.tar.xz` — `libzell_rt.a`, `libSystem.tbd`,
  `libsqlite3.tbd` и `BUNDLE` apple-darwin целей (`libSystem.tbd` выведен
  из символов рантайма и проверен против SDK);
- `lld-18-<хост>-<lldkey>.tar.xz` — `bin/lld` для linux-amd64,
  linux-arm64, macos-arm64, macos-amd64, windows-amd64;
- `.sha256` рядом с каждым архивом.

Два слоя, распакованные друг поверх друга, дают полный каталог
`build/targets/<triple>/` (`BUNDLE`, `libzell_rt.a`, `libSystem.tbd` и
остальное — то, что проверяет `zell doctor`); `BUNDLE` лежит во внешнем
слое, а у apple-darwin — в слое рантайма, где известно число символов
`.tbd`.

Ключи — первые 16 hex-цифр sha256. `scripts/bundle-targets.sh --print-key`
печатает `external=<ekey> runtime=<rkey>` (`--print-key external|runtime` —
один ключ), `scripts/build-lld.sh --print-key` — `<lldkey>`:

- `<ekey>` — сам `scripts/bundle-targets.sh`;
- `<rkey>` — `runtime/Makefile`, все файлы `runtime/{include,src,stub-sdk}`,
  `scripts/lld/darwin-symbols.txt` и те части скрипта, что определяют
  рантайм: закреплённые входы (`PINS`, версии), функции `runtime()` и
  `darwin()` (смена флагов сборки или версии заголовков — новый слой
  рантайма, остальные правки скрипта его не трогают);
- `<lldkey>` — `scripts/build-lld.sh` и `scripts/lld/lld-main.cpp`.

Это не значение `hashFiles` из ключей `actions/cache`.

`bundle-targets.sh --prebuilt [URL]` и `build-lld.sh --prebuilt [URL]`
(URL по умолчанию — релиз `zell-lang/zell`, или `$ZELL_INPUTS_URL`, в том
числе `file://`) скачивают архивы своих ключей, сверяют `.sha256`
(несовпадение — ошибка, а не молчаливая пересборка) и распаковывают в
`--out`. Слои независимы: если актива одного слоя нет (404), скрипт
сообщает об этом и собирает только этот слой, второй берёт из релиза.
`--layer all|external|runtime` ограничивает работу одним слоем;
сборка только рантайма по-прежнему скачивает заголовки (glibc,
mingw-w64) и распаковывает исходники musl, но не собирает заглушки, musl
и compiler-rt. `--offline` не меняется. `release.yml` и `bundle.yml`
вызывают скрипты с `--prebuilt` (`actions/cache` остаётся вторым слоем),
а сухой прогон релиза занимает около 10 минут вместо 30.

`inputs.yml` принимает вход `layer: all|external|runtime`. После правки
`runtime/` или `scripts/lld/darwin-symbols.txt` (частый случай) достаточно
обновить только рантайм, около 1-2 минут на ОС:

```sh
gh workflow run inputs.yml -f layer=runtime
```

После правки `scripts/bundle-targets.sh` вне `runtime()`/`darwin()`/`PINS`
или `scripts/build-lld.sh` нужен `-f layer=external` (или `all`); правка
`PINS`, `runtime()` и `darwin()` меняет оба ключа. Старые активы
остаются. Пока слой не обновлён, релиз работает, но собирает этот слой на
раннере.

Сгенерированные `libSystem.tbd` и заглушки glibc в репозиторий не
коммитятся: `libSystem.tbd` строится из неопределённых символов
собранного `libzell_rt.a` и compiler-rt (а не только из
`darwin-symbols.txt`), поэтому меняется вместе с рантаймом, а заглушки
glibc — бинарные `.so`, порождаемые из таблиц символов `.deb`.

### Линковка (`compiler/src/crosslink.rs`)

Если рядом с найденным рантаймом цели лежит `BUNDLE`, `zell` сам строит
командную строку `lld` целиком; иначе (рантайм из `runtime/` репозитория,
старый `zell target add`) — прежний драйвер clang. `ZELL_LINKER=clang`
принудительно выбирает clang, `ZELL_LLD` — другой lld. Рецепты:

- **linux-gnu**: `lld -flavor gnu -m elf_x86_64|aarch64linux -pie
  -dynamic-linker /lib64/ld-linux-x86-64.so.2|/lib/ld-linux-aarch64.so.1
  --hash-style=gnu --build-id --eh-frame-hdr -z relro Scrt1.o crti.o
  crtbeginS.o <объекты> libzell_rt.a -L<dir> --as-needed -lsqlite3?
  -lpthread -ldl -lrt -lm builtins -lc libc_nonshared.a ld-linux…
  crtendS.o crtn.o` (+ `--gc-sections`/`-s` в `--release`,
  `--export-dynamic` в отладочной сборке). `--static` с glibc нужен
  `libc.a`, его нет в архиве: при наличии clang линкуется им, иначе
  ошибка с советом `--target linux-amd64-musl`.
- **linux-musl**: `-static crt1.o crti.o crtbegin.o … --start-group libc.a
  builtins --end-group crtend.o crtn.o`, без `PT_INTERP`.
- **windows-amd64** (MinGW): `lld -flavor gnu -m i386pep` (MinGW-драйвер
  lld) `crt2.o … -lws2_32 -liphlpapi -lbcrypt -lshell32 -ladvapi32
  -lmingw32 builtins -lmoldname -lmingwex -lmsvcrt -luser32 -lkernel32`;
  crtbegin/crtend gcc не нужны (SEH x86-64), libgcc заменён compiler-rt.
- **macOS**: `lld -flavor darwin -arch arm64|x86_64 -platform_version macos
  12.3.0 14.0 … builtins libSystem.tbd libsqlite3.tbd?` (macOS ≥ 12.3: первый SDK с `sqlite3_changes64`) (+ `-dead_strip`, `-x -S`); lld
  подписывает arm64-бинарники ad hoc сам.

Параллельный кодогенератор сливает части объекта `ld.lld -r` из архива,
когда системного clang нет. `zell doctor` проверяет линковщик, `std/` и
наличие всех файлов каждой цели; `zell target list` пишет `bundled`;
`zell env ZELL_LINKER ZELL_TARGETS` — линковщик и каталоги целей.

### LTO программы и рантайма (`ZELL_LTO=1`, опционально)

Каждый вызов помощника рантайма из кода Zell (`zell_slice_new`, строковые
функции, `rtGet` в `encoding.json`) — внешний вызов в `libzell_rt.a`.
ThinLTO между программой и рантаймом включается явно:

```sh
make -C runtime LTO=1 CC=clang      # build/lto/libzell_rt.a: объекты-биткод (-flto=thin)
ZELL_LTO=1 zell build --release main.zl
```

С `ZELL_LTO=1` компилятор в `--release` сборке для хоста пишет модуль
программы биткодом (`.bc`, уже оптимизированным) и линкует системным
`clang -flto=thin -fuse-ld=lld`; рантайм берётся из `build/lto/` рядом с
найденным `libzell_rt.a` (или `lto/libzell_rt.a` рядом с установленным),
без него — ThinLTO только над программой (с `-v` об этом пишется).
Нужны clang и lld одной версии (биткод рантайма читает `llvm-ar` той же
версии, что clang). Модуль оптимизируется конвейером
`thinlto-pre-link<O2>` (остальное делает LTO-линковка; −22% на оптимизации
`sort_collect`), линковке передаётся процессор обычной сборки
(`-march=native` или `ZELL_TARGET_CPU`: без этого код генерировался под
базовый x86-64 без AVX/FMA) и кэш ThinLTO (`$ZELL_CACHE_DIR/lto`, иначе
`$XDG_CACHE_HOME/zell/lto` или `~/.cache/zell/lto`): повторная сборка
берёт неизменившиеся модули рантайма из кэша и выходит быстрее сборки без
LTO (`select_many` 0.27 с против 0.57, `sort_collect` 2.1 против 3.2 на
EPYC 9554); первая сборка примерно вдвое дольше обычной.
По умолчанию выключено: без системного clang/lld не работает, линковка
дольше (~1 с на `benchmarks/json_codec`). Эффект: json_codec «value
lookups» 67 → 47 мс (Go 43), `strings` 35 → 32 мс, `map` без изменений.
Не действует на кросс-сборки, Windows и сборки с bundled lld.

### Как собираются входы (`scripts/bundle-targets.sh`)

Все загрузки закреплены URL и sha256 в скрипте (Ubuntu-архив хранит
пакеты релизных карманов вечно; compiler-rt — релиз LLVM на GitHub),
кэш — `build/bundle-cache` (`--cache`, `--offline` — сборка без сети из
кэша). Запускается на Linux (CI), результат одинаков для всех хостов.

- **glibc, пол 2.31.** crt-объекты и `libc_nonshared.a` — из `libc6-dev
  2.31-0ubuntu9` (Ubuntu 20.04). Заглушки: из таблиц динамических символов
  настоящих `.so` 2.31 (`libc6 2.31-0ubuntu9`) берутся все символы версии
  по умолчанию (`@@GLIBC_2.x`, без `GLIBC_PRIVATE`) с типом, размером и
  выравниванием (для copy-релокаций) и собираются ассемблером и
  `ld.lld -shared --version-script` в `.so` без кода с тем же `SONAME`.
  Исполняемый файл получает ровно те `NEEDED` и требования версий, что дал
  бы настоящий glibc 2.31, поэтому работает на любом glibc ≥ 2.31 (Ubuntu
  20.04+, Debian 11+, RHEL 9+). Рантайм собирается против заголовков 2.31
  (`--sysroot`, `-nostdinc`): в нём нет `__isoc23_*` (2.38) и других новых
  символов. SQLite: заглушка `libsqlite3.so.0` из 3.45.1 и `sqlite3.h`;
  программа с `database.sqlite` требует `libsqlite3.so.0` в системе, как и
  раньше.
- **musl 1.2.5** собирается из исходников (`musl_1.2.5.orig.tar.gz` = релиз
  musl) clang-ом под x86_64 и aarch64.
- **mingw-w64 11.0.1**: CRT и импорт-библиотеки из `mingw-w64-x86-64-dev`
  Ubuntu 24.04; заголовки — только для сборки рантайма. CRT — `msvcrt.dll`,
  как раньше.
- **macOS**: `libSystem.tbd` (TAPI v4) генерируется: символы, которые
  импортируют рантайм и builtins, плюс `scripts/lld/darwin-symbols.txt`
  (libcalls LLVM и общий C/POSIX API для FFI). Символы живут в
  реэкспортах libSystem (libsystem_c, libsystem_kernel…), dyld находит их
  через реэкспорты; `--check-darwin-tbd` на Mac сверяет список с `.tbd`
  SDK. Рантайм на Linux собирается против `runtime/stub-sdk/macos`
  (`STUBSDK=yes`; на x86_64 — с `$INODE64`-вариантами `stat`/`dirent`), в
  релизе — на Mac против настоящего SDK (джоб `targets-macos`).
- **compiler-rt 18.1.8 builtins** собираются из исходников под каждую
  цель (список файлов как у CMake: generic, TF, x86 80-bit, `lse.S` по
  операциям/размерам/моделям для outline-атомиков arm64 Linux, `chkstk.S`
  для Windows), плюс `crtbegin`/`crtend` для ELF.

`bin/lld` (`scripts/build-lld.sh`): Linux и macOS — драйвер lld
(`scripts/lld/lld-main.cpp`, четыре flavour) статически слинкованный со
статическими библиотеками LLVM 18 и lld дистрибутива (`liblld-18-dev`,
Homebrew `llvm@18`), из бэкендов — только X86 и AArch64, без libxml2
(слияние манифестов Windows не поддерживается), libstdc++/zlib/zstd/
terminfo внутри: зависит только от libc. Если статических библиотек нет —
сборка lld из `llvm-project-18.1.8.src.tar.xz` (кэшируется в CI).
Windows — `lld.exe` официального `clang+llvm-18.1.8-x86_64-pc-windows-msvc`
рядом с `vcruntime140.dll`/`msvcp140.dll`. Сами `zell`/`zell-lsp` хоста
собраны на Ubuntu 24.04 и требуют glibc ≥ 2.39 на машине разработчика
(это пол тулчейна, не программ, которые он собирает).

Лицензии всех частей — `THIRD_PARTY.md` (lld и compiler-rt — Apache-2.0
WITH LLVM-exception, musl — MIT, mingw-w64 — ZPL/public domain, crt glibc
— LGPL с исключением для startup-файлов, заглушки — только списки имён).

### Проверка

- Локально (Linux x86-64, без clang в `PATH` у `zell`): примеры собраны и
  запущены для linux-amd64 (и под загрузчиком glibc 2.31 из `libc6
  2.31-0ubuntu9`: `ld-2.31.so --library-path …`), linux-amd64-musl
  (статический), linux-arm64 (QEMU с glibc 2.31 и 2.39), linux-arm64-musl
  (QEMU), windows-amd64 (Wine 9); macOS — слинкованы Mach-O (arm64
  подписан ad hoc). `install.sh --from` + `zell doctor` — «toolchain
  complete».
- `compiler/tests/cross.rs`, модуль `bundle` (`compiler/tests/cross/
  bundle.rs`): то же самое из распакованного архива (`$ZELL_BUNDLE`) или
  собранного из `build/`, с `PATH` только из `bin/` тулчейна; glibc-бинарники
  не должны требовать версий выше `GLIBC_2.31`.
- CI `.github/workflows/bundle.yml`: сборка архива на linux-amd64/arm64,
  `cross.rs bundle::*`, `scripts/bundle-smoke.sh` в голом контейнере
  `ubuntu:24.04` (wine/qemu ставятся только для чужих целей), glibc-бинарники
  в `ubuntu:20.04`, кросс-собранные на Linux бинарники macOS и Windows —
  на настоящих macos-14, macos-15-intel и windows-latest.

## Этапы

1. ✅ Выделить `runtime/src/platform/` (posix.c, fs_posix.c), убрать прямые
   вызовы из остального рантайма; ✅ `@cfg` и файловые суффиксы в
   компиляторе; ✅ `os.Name/Arch`.
2. macOS arm64/amd64: kqueue-бэкенд планировщика ✅ (компилируется против
   стабов), символы `LC_SYMTAB` ✅; проверка на Mac и CI на macOS —
   `scripts/macos-verify.sh`.
3. Windows amd64: ✅ win32.c (потоки, файлы, консоль UTF-8, Winsock,
   процессы, сигналы), ✅ PE/COFF через lld (MinGW ABI), ✅ поллер IOCP
   (AFD poll) с запасным WSAPoll, ✅ `TransmitFile`, ✅ каналы процессов
   через IOCP, ✅ сэмплер CPU, ✅ символы, ✅ часовые пояса, ✅ хранилище
   сертификатов, ✅ sqlite через DLL; CodeView/MSVC ABI — дальше.
4. Кросс-компиляция: ✅ `--target` в `build`/`run` (Windows через Wine,
   linux-arm64 через QEMU, macOS — объектники), ✅ поиск рантайма в
   `~/.zell/targets/<triple>/` и в тулчейне, ✅ `zell target add/list`,
   ✅ `package-toolchain.sh`, `install.sh`/`install.ps1`; релизные сборки
   под все tier-1 — по артефактам CI.
5. ✅ linux-arm64 (QEMU + нативный CI), ✅ linux-musl (статический); FreeBSD,
   позже wasm32-wasi.
