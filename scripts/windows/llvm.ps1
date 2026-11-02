# scripts/windows/llvm.ps1 - LLVM 23 for building the compiler with MSVC
# (ci.yml, release.yml): llvm-sys needs llvm-config and static LLVM
# libraries with every target the compiler initialises (X86, AArch64,
# WebAssembly). The official clang+llvm tarball for MSVC has both, but its
# `llvm-config --system-libs` names zs.lib (zlib), zstd_static.lib (by the
# build machine's absolute path) and xml2s.lib, none of which it ships:
#   - zlib and zstd are built here from source with its clang-cl, /MT: the
#     LLVM 23 release libraries use the static CRT (and rpmalloc), so the
#     Rust code does too (.cargo/config.toml: +crt-static), and llvm-sys then
#     links the system libraries statically, which needs their import
#     libraries in a search directory: they are copied from the newest
#     Windows SDK into the LLVM lib directory;
#   - libxml2 is needed only by LLVMWindowsManifest, which nothing links: a
#     stub library stands in (both names, libxml2s.lib and xml2s.lib);
#   - llvm-config-shim.c, compiled into <shim>\bin\llvm-config.exe, drops the
#     directory of absolute *.lib paths.
# Writes LLVM_SYS_231_PREFIX (the shim) and ZELL_WIN_LLVM (the LLVM prefix,
# for bin\lld.exe) to $env:GITHUB_ENV when it is set, and prints them.
#   pwsh scripts/windows/llvm.ps1 [-Version 23.1.4] [-Dest dir]
param(
    [string]$Version = "23.1.4",
    [string]$Dest = $(if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { $env:TEMP })
)
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"
$name = "clang+llvm-$Version-x86_64-pc-windows-msvc"
# Windows' own bsdtar: a GNU tar earlier on PATH (Git's) takes "C:" for a host
$tarExe = Join-Path $env:SystemRoot "System32\tar.exe"
$prefix = Join-Path $Dest $name
$shim = Join-Path $Dest "llvm-$Version-shim"

function Run($exe, [string[]]$a) {
    & $exe @a
    if ($LASTEXITCODE -ne 0) { throw "$exe failed ($LASTEXITCODE): $a" }
}

if (-not (Test-Path "$prefix\bin\llvm-config.exe")) {
    $tar = Join-Path $Dest "llvm-$Version.tar.xz"
    if (-not (Test-Path $tar)) {
        Invoke-WebRequest "https://github.com/llvm/llvm-project/releases/download/llvmorg-$Version/$name.tar.xz" -OutFile $tar
    }
    Run $tarExe @("-xf", $tar, "-C", $Dest, "$name/bin/llvm-config.exe", "$name/bin/clang-cl.exe", "$name/bin/llvm-lib.exe",
        "$name/bin/lld.exe", "$name/bin/lld-link.exe", "$name/lib", "$name/include")
    Remove-Item $tar
}
$cc = "$prefix\bin\clang-cl.exe"
$ar = "$prefix\bin\llvm-lib.exe"
$work = Join-Path $Dest "llvm-$Version-deps"
New-Item -ItemType Directory -Force -Path $work | Out-Null

# libxml2 stub
Set-Content -Path "$work\xml2stub.c" -Value "int zell_libxml2_stub;"
Run $cc @("/nologo", "/c", "$work\xml2stub.c", "/Fo$work\xml2stub.obj")
Run $ar @("/nologo", "/out:$prefix\lib\libxml2s.lib", "$work\xml2stub.obj")
Copy-Item "$prefix\lib\libxml2s.lib" "$prefix\lib\xml2s.lib" -Force

# zlib (zs.lib) and zstd (zstd_static.lib), static, /MD
function Build-Lib($url, $dir, [string[]]$globs, [string[]]$defs, $out) {
    if (Test-Path $out) { return }
    $arc = Join-Path $work (Split-Path $url -Leaf)
    Invoke-WebRequest $url -OutFile $arc
    Run $tarExe @("-xzf", $arc, "-C", $work)
    $obj = Join-Path $work "$dir-obj"
    New-Item -ItemType Directory -Force -Path $obj | Out-Null
    $i = 0
    foreach ($g in $globs) {
        foreach ($f in Get-ChildItem (Join-Path $work $g)) {
            $i++
            Run $cc (@("/nologo", "/O2", "/MT", "/w", "/c", $f.FullName, "/Fo$obj\$i.obj") + $defs)
        }
    }
    Run $ar (@("/nologo", "/out:$out") + (Get-ChildItem "$obj\*.obj" | ForEach-Object { $_.FullName }))
}
Build-Lib "https://github.com/madler/zlib/releases/download/v1.3.1/zlib-1.3.1.tar.gz" "zlib" `
    @("zlib-1.3.1\*.c") @() "$prefix\lib\zs.lib"
Build-Lib "https://github.com/facebook/zstd/releases/download/v1.5.7/zstd-1.5.7.tar.gz" "zstd" `
    @("zstd-1.5.7\lib\common\*.c", "zstd-1.5.7\lib\compress\*.c", "zstd-1.5.7\lib\decompress\*.c") `
    @("/DZSTD_DISABLE_ASM", "/DZSTD_MULTITHREAD=0") "$prefix\lib\zstd_static.lib"

# import libraries of the system libraries llvm-config names
$kits = Join-Path ${env:ProgramFiles(x86)} "Windows Kits\10\Lib"
$um = Get-ChildItem $kits -Directory | Sort-Object { [version]$_.Name } -Descending |
    ForEach-Object { Join-Path $_.FullName "um\x64" } | Where-Object { Test-Path (Join-Path $_ "psapi.lib") } |
    Select-Object -First 1
if (-not $um) { throw "no Windows SDK with um\x64\psapi.lib under $kits" }
foreach ($l in @("psapi", "shell32", "ole32", "uuid", "advapi32", "ws2_32", "ntdll")) {
    Copy-Item (Join-Path $um "$l.lib") "$prefix\lib\$l.lib" -Force
}

# llvm-config shim
New-Item -ItemType Directory -Force -Path "$shim\bin" | Out-Null
$real = ("$prefix\bin\llvm-config.exe") -replace '\\', '/'
Set-Content -Path "$work\real.h" -Value "#define REAL_LLVM_CONFIG `"$real`""
$src = Join-Path $PSScriptRoot "llvm-config-shim.c"
# linked by the tarball's lld-link: a link.exe on PATH may be Git's, not MSVC's
Run $cc @("/nologo", "/O2", "/w", "-fuse-ld=lld", "/FI$work\real.h", $src, "/Fe$shim\bin\llvm-config.exe", "/Fo$work\shim.obj")

& "$shim\bin\llvm-config.exe" --version --targets-built --system-libs
if ($LASTEXITCODE -ne 0) { throw "llvm-config shim failed" }
if ($env:GITHUB_ENV) {
    "LLVM_SYS_231_PREFIX=$shim" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8
    "ZELL_WIN_LLVM=$prefix" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8
}
Write-Output "LLVM_SYS_231_PREFIX=$shim"
Write-Output "ZELL_WIN_LLVM=$prefix"
