# FDM Rust

FDM 的 Rust + Tauri 2 重写版：桌面壳和下载内核使用 Rust，前端继续复用原 `app_ui`，并保留 Chromium MV3 扩展对 `127.0.0.1:25432` 的兼容。

## 架构

- `fdm-core`：任务模型、设置、跨平台数据目录、原子 JSON 写入、旧数据迁移
- `fdm-engine`：HTTP Range 探测、多线程分块下载、断点续传、暂停/恢复、限速、任务调度
- `fdm-platform`：默认下载目录、打开路径、剪贴板、系统对话框、开机启动、文件关联
- `fdm-server`：Axum HTTP API、SSE、静态 UI、浏览器扩展接管
- `src-tauri`：Tauri 2 窗口、托盘、单实例、桌面窗口钩子

HTTP 服务固定监听 `127.0.0.1:25432`，现有前端通过同源 `fetch` 和 `EventSource` 直接访问。

## 开发

### 构建依赖（Windows）

BT/ED2K 引擎依赖 rustls（ring/aws-lc）与 Tauri 资源编译，需要 C 编译器与 Windows SDK 头文件/库。
当前工作区使用 `D:\Programs\llvm-mingw\llvm-mingw-20261020-ucrt-x86_64` 与 `D:\Programs\xwin\sdk`，
可参考 `scripts/build-env.ps1` 设置 `CC/AR/RC/LIB/INCLUDE` 与 `CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER`。

### ED2K 引擎资源

ED2K 使用随包附带的 aMule 3.1.0。首次构建前执行：

```powershell
powershell -ExecutionPolicy Bypass -File scripts/package-amule.ps1
```

脚本会把 `amuled/amuleapi` 及依赖裁剪到 `src-tauri/resources/amule/win-x64`（约 33 MiB）。
`src-tauri/resources/amule/` 不纳入 Git，发行时必须随安装包提供（GPL-2.0，见 `THIRD_PARTY_NOTICES.md`）。

```powershell
. .\scripts\build-env.ps1
cargo check --workspace
cargo test --workspace
npm install
npm run dev
```

后台模式：

```powershell
cargo run -p fdm-desktop -- --background
```

## 构建

```powershell
. .\scripts\build-env.ps1
npm run build
```

Windows 产物位于 `target/release/bundle/`。macOS 需要在 macOS 主机上执行同一命令，并使用 Apple Developer 证书完成签名/公证。

## 发布与自动更新

- 版本号以 `package.json` 为基准：`npm run version:next` 只计算下一 patch，`npm run version:bump` 计算并同步全部版本文件，`npm run version:check -- 1.0.1` 用于校验。
- 推送 `release` 分支会触发 `.github/workflows/release.yml`：自动递增 patch、提交版本、打 `vX.Y.Z` 标签，构建 Windows x64 与 macOS arm64/x64，并在全部产物和 `latest.json` 校验通过后自动发布。
- 发布后工作流会把该版本同步回 `main`。若 `main` 有冲突或分支保护阻止 Actions 推送，发布仍会保留，但同步 job 会失败。
- 发布前需配置仓库 Secrets：`TAURI_SIGNING_PRIVATE_KEY`、`TAURI_SIGNING_PRIVATE_KEY_PASSWORD`。私钥禁止提交到仓库。
- 客户端通过 `https://github.com/ZHDeveloper/fdm-rust/releases/latest/download/latest.json` 检查更新；设置页“关于”提供手动检查与安装入口。调试构建不自动检查，可用 `FDM_DISABLE_UPDATE_CHECK=1` 禁用正式构建的启动检查。

## 数据目录

- Windows：`%USERPROFILE%\FDM\data`
- macOS：`~/FDM/data`

## 浏览器扩展

安装包自带 Chromium MV3 扩展，支持 Chrome、Edge 及其他 Chromium 内核浏览器（如 Brave、Vivaldi、Opera 等；Firefox / Safari 暂不支持）。全新安装后首次启动 FDM 会自动弹出安装引导：

1. 打开 Chrome 的 `chrome://extensions` 或 Edge 的 `edge://extensions`
2. 打开「开发者模式」
3. 点击「加载已解压的扩展程序」，选择 `%USERPROFILE%\FDM\data\extension`（macOS：`~/FDM/data/extension`）

安装后可在「设置 → 扩展」查看连接状态、打开扩展目录或重新打开安装向导。扩展活跃时每 5 秒访问本机 `/api/browser/status`，Service Worker 休眠后通过 `chrome.alarms` 每约 30 秒唤醒心跳，FDM 以最近 75 秒内是否收到心跳判断扩展是否已连接；已有用户在升级后不会自动弹出该引导。

## 文件关联

Windows 下 FDM 会在当前用户注册 `magnet:`、`ed2k://` 协议和 `.torrent` 文件关联，并在 Windows“默认应用”中登记 FDM。
macOS 下 `.app` 通过 `Info.plist` 声明这两个协议和 `.torrent` 文档类型，启动时用 LaunchServices 将 FDM 设为默认处理程序；关闭开关只会停止后续接管，已设置的默认应用需在“系统设置 → 默认应用”里修改。
可在“设置 → 常规 → 系统”中开关；如果系统已将关联指向其他程序，需要在系统的默认应用设置里手动改为 FDM。
调试构建启动时不会自动写入系统关联（在设置里保存仍会立即生效）；可用环境变量 `FDM_DISABLE_FILE_ASSOC=1` 临时关闭。

保留 `settings.json`、`tasks.json`、`.fdmpart`、`.fdmcfg` 的兼容格式，并会从旧安装目录旁的 `data` 目录迁移缺失数据。

## 平台验证

- Windows：已在本机完成 `cargo check --workspace`、`cargo test --workspace`、`npm run build`（MSI/NSIS 安装包）、Tauri 实机启动、主窗口/设置/新建下载/进度窗口、HTTP API、SSE、多线程下载、暂停恢复、浏览器拦截验证。
- macOS：代码路径已按 macOS 适配（`~/Downloads`、`open`、剪贴板、LaunchAgent 开机启动），但未在当前 Windows 环境实际编译或运行验证；macOS 打包、签名和公证必须在 macOS 上完成。

## 测试用环境变量

- `FDM_DATA_DIR`：覆盖数据目录，便于隔离测试。
- `FDM_DISABLE_AUTO_LAUNCH`：设置任意值后禁止写入开机启动项。
