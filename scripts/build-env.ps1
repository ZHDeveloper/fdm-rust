# 在当前 PowerShell 进程中设置 FDM-Rust 的 Windows 构建环境。
# 用法： . .\scripts\build-env.ps1
$llvm = "D:\Programs\llvm-mingw\llvm-mingw-20261020-ucrt-x86_64\bin"
$xwin = "D:\Programs\xwin\sdk"

$env:CC_x86_64_pc_windows_msvc = Join-Path $llvm "clang.exe"
$env:CXX_x86_64_pc_windows_msvc = Join-Path $llvm "clang++.exe"
$env:AR_x86_64_pc_windows_msvc = Join-Path $llvm "llvm-lib.exe"
$env:RC_x86_64_pc_windows_msvc = Join-Path $llvm "llvm-rc.exe"
$env:RC = Join-Path $llvm "llvm-rc.exe"
$env:CFLAGS_x86_64_pc_windows_msvc = "--target=x86_64-pc-windows-msvc -fms-compatibility -fms-extensions"
$env:CXXFLAGS_x86_64_pc_windows_msvc = "--target=x86_64-pc-windows-msvc -fms-compatibility -fms-extensions"
$env:INCLUDE = @(
    (Join-Path $xwin "crt/include"),
    (Join-Path $xwin "sdk/include/um"),
    (Join-Path $xwin "sdk/include/shared"),
    (Join-Path $xwin "sdk/include/ucrt"),
    (Join-Path $xwin "sdk/include/winrt")
) -join ";"
$env:LIB = @(
    (Join-Path $xwin "crt/lib/x86_64"),
    (Join-Path $xwin "sdk/lib/um/x86_64"),
    (Join-Path $xwin "sdk/lib/ucrt/x86_64")
) -join ";"
$env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER = Join-Path $env:USERPROFILE ".rustup\toolchains\stable-x86_64-pc-windows-msvc\lib\rustlib\x86_64-pc-windows-msvc\bin\rust-lld.exe"

Write-Host "FDM-Rust build environment ready (llvm-mingw + xwin)."
