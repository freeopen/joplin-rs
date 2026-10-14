# gpui-kit 迁移收尾：验证、修复、文档对齐、落地提交

> 状态：计划（已评审，未开始实施）
> 前置文档：[docs/gpui-kit-migration.md](gpui-kit-migration.md)
> 日期：2026-10-13

## 1. 现状结论

`docs/gpui-kit-migration.md` 的步骤 1-7 已在当前工作树（**未提交**，12 个文件改动）中实质完成：

| 计划步骤 | 实际状态 |
| --- | --- |
| 步骤 1：依赖切换 | `gpui = "=0.2.2"` → `gpui-kit = 0.7.3`；`comrak` 移除；`spike-runtime-shaders` 删除；`.cargo/config.toml` 按 gpui-pre 家族改名 |
| 步骤 2：引导层 | `gpui_main()` 走 `gpui_kit::application/init/open_window`，窗口图标 + 标题 + CJK 字体初始化（`JOPLIN_RS_FONT`）已实现 |
| 步骤 3：输入框 | `ui/input.rs` 已删，6 个常驻 `InputState`（search/gate/rp×3/e2ee）+ `InputEvent` 订阅 |
| 步骤 4：对话框 | `ui/dialog.rs` 已删，`WindowExt::open_dialog` / `open_alert_dialog` + `sync_dialog_from_state` 对齐 |
| 步骤 5：Markdown | `ui/markdown.rs` 已删，`TextView::markdown` + `image_source`（本地资源→`Resource::Path`）+ `on_link_click` |
| 步骤 6：主体收尾 | 150ms `cx.spawn_in` 轮询 JobChannel；侧栏/笔记列表主题色取 `cx.theme()` |
| 步骤 7：删除 egui | `main.rs` 缩至 39 行单 GPUI 前端；`eframe`/`egui`/`egui_commonmark` 全部移除；feature 门控删除 |

基线验证（2026-10-13，本机）：

- `cargo build -p joplin-rs-app` 通过；
- `cargo test --workspace` 全绿（app 15、core 29、其余各 1-6 项全部通过）；
- `cargo clippy -p joplin-rs-app` 有 5 个告警（见 §3 步骤 1），另有上游 `block v0.1.6` future-incompat 提示（objc2 传递依赖，不处理）。

剩余工作：修复告警与一处真实 bug、人工行为对等验收、迁移文档状态对齐、落地提交、CI 四平台验证。

## 2. Scope / affected areas

- `crates/app/src/view.rs`：clippy 3 处 + `first_family_name` 的 TTC 头解析 bug；
- `crates/app/src/app.rs`：clippy 2 处 unneeded return；
- `docs/gpui-kit-migration.md`：状态行与"实际落地差异"小节；
- `.github/workflows/release.yml`：仅当 Linux CI 缺系统依赖时补 apt 包；
- 其余已实现文件（`Cargo.toml` / `.cargo/config.toml` / `main.rs` / `README.md` / `ui/` 删除）不再改动。

## 3. 实施步骤（有序）

### 步骤 1：修复 5 个 clippy 告警

验收项（迁移文档 §5）要求 `cargo clippy` 无新告警，当前 5 处：

- `app.rs:710` / `app.rs:714`：`unneeded return statement`；
- `view.rs:932`：`redundant closure`；
- `view.rs:1089`：`writing &PathBuf instead of &Path`。

### 步骤 2：修复 first_family_name 的 TTC 解析 bug

`view.rs:1179` 的 `if bytes.get(..4) == Some(b"ttcf") { 4 } else { 4 }` 两分支相同（即 clippy "identical blocks" 告警），背后是一个真实缺陷：

- 普通 sfnt（.ttf/.otf）头：scaler type 4 字节 + numTables @4，表目录从 8 开始；
- `.ttc` 头：tag 4 + version 4 + numFonts 4 共 12 字节，numTables 在偏移 8，表目录从 12 开始。

当前代码对 `.ttc` 按 sfnt 布局解析，`JOPLIN_RS_FONT` 指向 `.ttc`（如思源黑体的集合发行版）时 family 名提取会失败，字体注册被跳过。修正为：`ttcf` 时取 numTables@8 / dir@12，否则维持 sfnt 路径。

### 步骤 3：本地全量验证

```sh
cargo build -p joplin-rs-app
cargo test --workspace
cargo clippy -p joplin-rs-app
```

预期：构建通过、测试全绿、clippy 0 告警（仅剩上游 `block v0.1.6` future-incompat 提示，不动）。

### 步骤 4：人工行为对等验收（迁移文档 §5 清单，macOS）

实施进度（2026-10-14）：

- [x] 启动：窗口居中已用 CGWindowList 验证（1100x753，标题 joplin-rs；内容高 720 + 标题栏）；进程启动无崩溃
- [x] 视觉验收：解锁桌面后实测截图确认 —— 工具栏（Sync/ready、阅读密码、Reload、About）、笔记本/笔记列表、
      Markdown 正文（行内代码、链接下划线）、中英文混排渲染全部正常，应用前台运行稳定
- [x] `JOPLIN_RS_FONT` 覆盖：.ttf（Andale Mono）、.ttc（Hiragino Sans GB、ADTNumeric 12 成员集合）均正常启动无 panic
- [x] `JOPLIN_RS_FONT` 指向不存在文件：降级跳过注册，不 panic
- [x] `joplin-rs-app --version`：输出 `joplin-rs-app v1.1.0 (…)`，与 CLI/About 同源（VERSION_DISPLAY）。
      注意：egui 时代 `--version` 也会静默启动 GUI（从未实现过该 flag），本次补上
- [ ] Sync / E2EE 弹窗 / 阅读密码 gate 交互 / 搜索即时过滤 / 对话框 Esc/scrim：需真实后端与用户参与；
      桌面为用户在用环境，不再自动化点击，待用户手动验收
- [ ] `JOPLIN_RS_FONT` 覆盖后字体实际生效的目视确认（当前默认 PingFang 渲染已确认正常）

### 步骤 5：更新 docs/gpui-kit-migration.md

- 状态行 `计划（未开始实施）` → `已实施（2026-10-13）`；
- 新增"实际落地与计划差异"小节，记录三处偏差：
  1. **feature 门控未保留**：未按步骤 1-6 保留 `gpui-view` feature，步骤 7 前移，egui 与 feature 同批删除；
  2. **对话框 API**：用 `WindowExt::open_dialog` / `open_alert_dialog`（Root 栈式管理），而非 §3.1 草案的 `DialogHandle` 命令式 API；`AppState.dialog` 仍是唯一事实来源，`sync_dialog_from_state` 负责对齐开关；
  3. **轮询签名**：用 `cx.spawn_in(window, ..)`（闭包带 window 上下文）而非裸 `cx.spawn`，dialog 同步需要 window。

### 步骤 6：落地提交

迁移代码是一个原子变更（依赖、代码、删除、README 相互依赖），拆开任何中间提交都无法编译，故：

1. **单个迁移提交**：`feat(app): migrate GUI to gpui-kit 0.7.3, drop egui frontend`
   （含 Cargo.toml / Cargo.lock / .cargo/config.toml / main.rs / view.rs / app.rs / macos_about.rs / ui/ 删除 / README.md）；
2. **单独 docs 提交**：`docs: mark gpui-kit migration implemented, record deltas`
   （本收尾文档 + gpui-kit-migration.md 更新）。

### 步骤 7：CI 验证（推送后四平台）

`release.yml` 矩阵（linux/windows/macos-x64/macos-arm64）各跑一次。重点观察：

- **Linux**：runner 是否缺 wayland/x11/wgpu 系统依赖；如失败，在 build 步骤前补 apt 安装；
- **Windows**：gpui-pre 首次构建（迁移文档风险 #6，路径未经本机验证，靠 CI 回归）；
- 结果（含是否补依赖）回写迁移文档。

预分析（2026-10-14，基于 Cargo.lock 依赖图核查，待 CI 确认）：**Linux 大概率无需补 apt 包**。
构建期链接需求只有：`freetype-sys`（pkg-config freetype2，否则内置源码用 cc 编译，runner 均有 gcc）、
`libsqlite3-sys`（bundled）。其余全部运行时 dlopen 或纯 Rust：`wayland-sys`（dlib/dlopen）、
`yeslogic-fontconfig-sys`（dlib/dlopen）、`x11rb`/`xkbcommon-rs`（纯 Rust）、Vulkan（ash + libloading）。
gpui-kit 对 Linux 同时启用 x11 + wayland（见 gpui-kit-0.7.3 Cargo.toml），运行时依赖
`ubuntu-latest` 镜像自带。若 CI 仍报链接错误再按报错补包并回写此处。

## 4. Validation 汇总

| 层面 | 手段 | 通过标准 |
| --- | --- | --- |
| 自动 | `cargo build -p joplin-rs-app` | 0 error |
| 自动 | `cargo test --workspace` | 全绿（基线 55 项） |
| 自动 | `cargo clippy -p joplin-rs-app` | 无新告警 |
| 回归 | `app_write_preserves_encrypted_password` 等密文/密码测试 | 通过 |
| 人工 | §3 步骤 4 清单 | 逐项勾选 |
| CI | 四平台 release 构建 | 产物全部产出 |

## 5. Open questions / decisions

1. **提交粒度**：推荐"单原子迁移提交 + docs 提交"共 2 个；如坚持按迁移文档 7 步拆分，需重建中间态且无法保证逐步可编译，默认不拆。
2. **保守方案已失效**：步骤 7 已随迁移完成（egui 已删），迁移文档风险 #1 的"两前端共存"不再可选。
3. **Windows 人工回归**：IME/中文输入无法本机验证，是否有可用 Windows 机器做人工检查？
4. **block v0.1.6 future-incompat**：来自 objc2 传递依赖，上游升级前只能带提示编译，本次不处理。
