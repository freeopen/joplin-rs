# joplin-rs GUI 迁移到 gpui-kit

> 状态：计划（未开始实施）
> 目标版本：gpui-kit 0.7.3（gpui-pre 0.3.9 / gpui-base 0.7.3 / gpui-component 0.7.3 / gpui-kit-assets 0.7.1）
> 现状基线：`gpui = "=0.2.2"` + 手写 widgets（`ui/input.rs` / `ui/dialog.rs` / `ui/markdown.rs`），位于 `gpui-view` feature 之后

## 1. 背景与目标

当前 GPUI 视图层（`crates/app`，`gpui-view` feature）是按 longbridge/gpui-kit 的设计思路**手写复刻**的：单行输入框、模态对话框、Markdown 渲染器都是对 gpui 0.2.2 原语的自行封装。gpui-kit 现已发布到 crates.io（本机 registry 已缓存 0.7.3 全家族），其 `gpui-component` 提供了生产级的 Input / Dialog / TextView 等组件。

迁移目标：

1. 用 gpui-kit 门面替换 `gpui = "0.2.2"` 依赖与全部手写 widgets；
2. 功能与现有 egui UI 完全对等（同步、搜索、Markdown、阅读密码、E2EE 弹窗）；
3. 迁移完成后 egui 前端退役，gpui 成为默认 GUI。

非目标（本次不做）：

- 深色模式切换（组件主题已支持，后续可加）；
- `gpui-fast` 后端；
- Sidebar / List / Dock 等重组件替换现有 div 布局（列为可选后续优化）。

## 2. 现状架构

```
crates/app/src/
├── main.rs          egui/eframe UI（默认路径）+ 模块声明 + Config 等共享逻辑
├── app.rs           framework-agnostic 状态机（AppState、Dialog 枚举、JobChannel）
├── jobs.rs          后台 sync/decrypt 工作线程（framework-agnostic）
├── view.rs          GPUI 根视图 NoteBrowser + gpui_main()（gpui-view feature）
├── ui/
│   ├── input.rs     手写单行输入（EntityInputHandler、IME、光标、快捷键）
│   ├── dialog.rs    手写模态层（deferred 绘制、scrim、Esc/Enter 语义）
│   └── markdown.rs  comrak AST → GPUI 元素树（GFM 子集）
├── read_password.rs 阅读密码 PBKDF2（框架无关，不动）
└── macos_about.rs   原生 About 面板 + dock 图标（objc2，与框架无关，不动）
```

要点：

- `app.rs` / `jobs.rs` 在上一轮已抽成框架无关层，本次基本不动；
- `view.rs` 的 150ms `cx.spawn` 轮询负责消费 `JobChannel` 事件并镜像 worker 状态，另以每帧轮询实现 search-as-you-type；
- 对话框字段（`rp_current/rp_new/rp_confirm/e2ee_field`）目前是"打开对话框时重建"的临时 `Entity<InputState>`。

## 3. gpui-kit 生态速览

gpui-kit 是 Zed GPUI 家族的应用侧门面，一个依赖带齐所有层：

| 通过 `gpui_kit::` 访问 | 实际 crate | 说明 |
| --- | --- | --- |
| `gpui_kit::*` | `gpui` (= gpui-pre 0.3.9) | GPUI 本体，API 与 Zed 上游同名 |
| `gpui_kit::platform` | `gpui-pre-platform` | `application()` 引导，桌面默认带 font-kit/x11/wayland/**runtime_shaders** |
| `gpui_kit::base` | `gpui-base 0.7.3` | Root、Dialog 原语、Input 状态机、富文本（TextView/Markdown） |
| `gpui_kit::component` | `gpui-component 0.7.3` | shadcn 风格组件：Button/Dialog/Input/TextView/Theme… |
| `gpui_kit::assets` | `gpui-kit-assets` | 内置图标（IconName） |

版本族约束：GPUI 家族以 `=0.3.9` 锁定联动发布，升级需整族一起动。

### 3.1 关键 API（迁移直接相关，均已对照 0.7.3 源码核实）

**引导**

```rust
gpui_kit::application().run(|cx: &mut App| {
    gpui_kit::init(cx);                     // = gpui_component::init
    gpui_kit::open_window(WindowOptions{..}, cx, |window, cx| cx.new(|cx| NoteBrowser::new(window, cx)))
    // open_window 内部用 base::Root 包住视图 —— 对话框/浮层依赖它，必须用它开窗
});
```

- `WindowOptions.icon: Option<Arc<image::RgbaImage>>` 支持窗口图标（app 需加 `image` crate 解码 `assets/icon.png`）；`TitlebarOptions { title, .. }` 设窗口标题。
- 动作宏：`gpui_kit::actions!`（自带宏，不要求依赖名叫 `gpui`）。
- `Context::spawn` 签名：`AsyncFnOnce(WeakEntity<T>, &mut AsyncApp) -> R`；定时器 `cx.background_executor().timer(Duration)`。

**输入（替换 ui/input.rs）**

```rust
use gpui_kit::component::input::{Input, InputState, InputEvent};

// InputState::new 需要 &mut Window —— NoteBrowser::new(window, cx) 需改签名
let state = cx.new(|cx| InputState::new(window, cx).placeholder("Search…"));
// 密码类：InputState::new(window, cx).masked(true)
Input::new(&state)            // 渲染端，可 .cleanable() / .mask_toggle() / .with_size()
// 事件：InputEvent::{ Change, PressEnter{secondary,shift}, Focus, Blur }
cx.subscribe(&state, |_, ev: &InputEvent, ..| match ev { .. });
state.read(cx).value()        // SharedString；set_value(..) 写入；state.focus(window, cx)
```

- 快捷键（backspace/copy/paste/enter 等）由组件内置 `Input` key context 注册，不再手写。

**对话框（替换 ui/dialog.rs）**

```rust
use gpui_kit::component::dialog::{Dialog, DialogButtonProps, DialogFooter, AlertDialog};
use gpui_kit::base::DialogHandle;

let handle = DialogHandle::new(false);
let dialog = Dialog::new(cx)
    .title("阅读密码设置")
    .content(|content, window, cx| content.child(..))   // 表单体
    .footer(DialogFooter::new()..)                      // 按钮行
    .handle(handle.clone())
    .on_ok(|_, window, cx| { ..; true /* false = 不关窗，显示错误 */ })
    .on_cancel(|_, window, cx| { ..; true });
handle.open(window, cx);    // 命令式开关；Esc / 点击 scrim 关闭由组件自带
```

- 简单确认框可用 `AlertDialog::new(cx).title(..).description(..).ok_text(..).on_ok(..)`。

**Markdown（替换 ui/markdown.rs）**

```rust
use gpui_kit::component::text::{TextView, TextViewStyle}; // re-export gpui_base

TextView::markdown("note-body", body)
    .image_source(|uri: &SharedUri| -> ImageSource {
        // 默认只内嵌解码 data: URL，其余按 URI 交给资源加载器；
        // Joplin 资源图片已被 app.rs 重写成本地文件路径，必须在此转 Path：
        if uri.scheme().is_none_or_http() { ImageSource::Resource(Resource::Uri(uri.clone())) }
        else { ImageSource::Resource(Resource::Path(PathBuf::from(uri.as_ref()).into())) }
    })
    .on_link_click(|url, _, _, cx| cx.open_url(url))
```

- GFM 表格/任务列表/删除线/代码块、文本选择均由 gpui-base 内置；comrak 依赖从 app 移除。
- 自定义块渲染逃生口：`MarkdownExtensions::block_renderer(name, fn)`（本应用暂不需要）。

**主题与字体（CJK 关键）**

- 主题：`Theme::change(mode, window, cx)`；`cx.theme()`（`ActiveTheme` trait）取色；`theme.font_family` / `theme.mono_font_family` 控制全局字体。
- GPUI 对**缺失 family 会在首次排版时 panic**（fallbacks 只补字形不救 family）。gpui-component 自身用 `cx.text_system().all_font_names()` 探测后替换默认 mono（见其 `theme/mono_font.rs`），迁移代码要沿用该模式：

```rust
// 启动时（init 之后、开窗之前）：
if let Ok(bytes) = std::fs::read(&font_path) {          // JOPLIN_RS_FONT 指定的 CJK 字体
    cx.text_system().add_fonts(vec![bytes.into()])?;    // 注册进字体系统
    cx.global_mut::<Theme>().font_family = family_name; // family 名取自字体文件
}
// 未指定 JOPLIN_RS_FONT 时：macOS 上 ".SystemUIFont"/PingFang 已覆盖中文，
// 但仍应探测 font_family 是否安装，缺失则替换为已安装候选，避免 panic。
```

**构建配置**

- `.cargo/config.toml` 的 `[profile.dev.package]` 包名需按 gpui-pre 家族改名（gpui-pre、gpui-pre-macros、gpui-pre-platform、gpui-pre-wgpu、naga、cosmic-text、taffy、rustybuzz、ttf-parser、lyon、image 等），首次编译后按 Cargo.lock 实际包名补齐。
- `MACOSX_DEPLOYMENT_TARGET=11.0` 保留。**已核实 gpui-pre 0.3.9 支持 Big Sur**（2026-10-13，方法与证据见 §7.7）。
- runtime_shaders：gpui-kit 的 platform 依赖默认带 `runtime_shaders`，无 Xcode metal 编译器的开发机可直接 `cargo build --features gpui-view`，现 `spike-runtime-shaders` feature 可删除。正式发布需预编译 metallib 时再评估（见 §7 风险）。

## 4. 迁移步骤（每步独立提交，全程 `--features gpui-view`）

### 步骤 1：依赖切换与编译通路

- `crates/app/Cargo.toml`：
  - `gpui = "=0.2.2"` → `gpui-kit = "0.7.3"`（默认 features：component + assets）；
  - `gpui-view = ["dep:gpui-kit"]`；删除 `spike-runtime-shaders`；
  - 新增 `image = { version = "0.25", default-features = false, features = ["png"] }`（窗口图标解码用）；
  - 移除 `comrak`（步骤 5 后生效）。
- `.cargo/config.toml` dev profile 包名更新。
- 验收：`cargo build -p joplin-rs-app --features gpui-view` 通过（此步 view.rs 尚未改，先只保证依赖图成立；需要临时把 view.rs/ui 的 gpui 引用改成 `gpui_kit::` 并容忍编译错误，或在本步内一并做步骤 2 的最小引导改动 —— 建议 1+2 合并为一个提交序列）。

### 步骤 2：引导层（view.rs 的 gpui_main + register）

- `Application::new().run(...)` → `gpui_kit::application().run(...)`；
- `cx.open_window(...)` → `gpui_kit::open_window(...)`（Root 包装）；
- `NoteBrowser::new` 增加 `window: &mut Window` 参数（InputState 需要）；
- `actions!` 改用 `gpui_kit::actions!`；keybinding 注册保留（cmd-r Sync、NoteGate Enter 等）；
- 窗口图标 + 标题：`WindowOptions { icon, titlebar: Some(TitlebarOptions { title: Some("joplin-rs".into()), .. }) }`；
- 字体初始化（§3.1 主题与字体小节），CJK 渲染人工验证；
- 窗口创建后调用 `macos_about::install_native_about()`（不变）。

### 步骤 3：输入框迁移（删 ui/input.rs）

- 五组输入实体（search、gate、rp_current/rp_new/rp_confirm、e2ee）**启动时一次性创建**并常驻 `NoteBrowser`，不再按对话框开关重建；打开对话框时 `set_value("")` 清空。
  - 理由：轮询/订阅路径拿不到 `&mut Window`，而组件版 `InputState` 的创建需要 Window；常驻化后所有重建逻辑消失。
- 密码类 `.masked(true)`。
- search-as-you-type：`cx.subscribe(&search_state, InputEvent::Change)` → `s.search = ..; s.load_notes()`，删除每帧文本轮询。
- gate / E2EE 提交：订阅 `InputEvent::PressEnter`，与按钮等价。

### 步骤 4：对话框迁移（删 ui/dialog.rs）

- `AppState.dialog` 仍为唯一事实来源；`NoteBrowser` 持有 `DialogHandle` + 当前 `Dialog` 变体的渲染函数，render 时对齐 handle 开/关状态。
- 阅读密码设置：content 内嵌 3 个 Input（当前/新/确认）+ 提示行；footer 三按钮（保存 / 移除密码 / 取消，移除密码仅已设置时显示）；`on_ok` 回调里 `collect_rp_inputs → s.submit_reader_pw()`，失败返回 `false` 不关窗。
- E2EE 提示：1 个 masked Input + 开始解密 / 取消（保持加密）；空密码/错误密码沿用 `AppState` 现有报错字段。
- About（非 macOS）：`AlertDialog` 或静态 Dialog。
- Esc / scrim 点击关闭由组件提供，删除手写 key_context("Dialog")。

### 步骤 5：Markdown 迁移（删 ui/markdown.rs）

- 正文面板 `TextView::markdown("note-body", body)`（见 §3.1）；
- `.image_source` 把本地资源路径转为 `Resource::Path`，远程 URL 保持 `Resource::Uri`；
- `.on_link_click` → `cx.open_url`；
- `Cargo.toml` 移除 `comrak`；文本选择、表格、代码块为免费增益。

### 步骤 6：view.rs 主体收尾

- 轮询循环改为新 spawn 签名：`async move |this: WeakEntity<NoteBrowser>, cx: &mut AsyncApp|`，保留 150ms tick（worker status 镜像 + `handle_job_event` + `after_sync_done` + `sync_dialog_from_state`）；删除 search 文本轮询；
- 侧栏/笔记列表暂用现有 div 结构（换主题色取 `cx.theme()` 语义色，替代硬编码 zinc 常量）；
- key_context（"NoteGate"）、Focusable 对齐。

### 步骤 7：切换默认并删除 egui（最后一步，单独提交/PR）

- `main()` 直接走 gpui 路径；删除：
  - 依赖：`eframe` / `egui` / `egui_commonmark`（workspace 级）；
  - `main.rs` 内 egui `App` 实现、egui 版 `Dialog`、`configure_fonts`/`find_cjk_font`（egui 专用，GPUI 走 §3.1 方案）；
  - `--features gpui-view` 门控（`mod app/jobs/ui/view` 无条件编译），feature 定义移除；
- `main.rs` 里仍被共享的逻辑（`crate_resource_ids`、`short_id`、`is_encrypted_placeholder` 的 egui 副本）以 app.rs 版本为准，删除重复；
- icon 测试改用 `image` crate 解码断言；
- `README.md`：GUI 段落（构建命令不再带 feature）、feature 说明删除；
- 验收：`cargo build --release` 默认 feature 即产出 GPUI 版 `joplin-rs-app`，CI 矩阵无需改动（release.yml 编译默认 feature）。

## 5. 行为对等验收清单（人工，对照 egui 版逐项）

- [ ] 启动：窗口 1100x720 居中、dock/窗口图标、中文渲染正常（PingFang）、`JOPLIN_RS_FONT` 覆盖生效
- [ ] Sync：两阶段下载 → 自动解密（有存储密码）或弹 E2EE 窗；进度条文案实时更新；完成后列表刷新
- [ ] 笔记列表：锁定占位符 🔒、受保护 🔐、todo ☑/☐、tag `#xxx`、snippet 显示
- [ ] 搜索：即时过滤、多词 AND、锁定/受保护笔记不显示 snippet
- [ ] 笔记渲染：标题/加粗/斜体/删除线/行内代码/代码块/引用/列表/任务列表/表格/分隔线/链接点击/本地资源图片/远程图片降级为链接
- [ ] 阅读密码 gate：错误提示、解锁、每次重选重锁、保护/取消保护、设置/修改/移除密码、`config.toml` 密文不被降级（跑 `app_write_preserves_encrypted_password` 等现有测试）
- [ ] E2EE 弹窗：空密码报错、错误密码保持加密并重新弹窗、取消保持加密
- [ ] 对话框交互：Esc 关闭、scrim 点击关闭、Enter 提交、错误时不关窗
- [ ] `joplin-rs-app --version` 与 About 面板版本一致（版本逻辑在 core，不受影响）
- [ ] `cargo test --workspace` 全绿；`cargo clippy` 无新告警

## 6. 文件级迁移对照

| 现有 | 处置 | 替代物 |
| --- | --- | --- |
| `ui/input.rs` | 删除 | `component::input::{Input, InputState, InputEvent}` |
| `ui/dialog.rs` | 删除 | `component::dialog::{Dialog, DialogFooter, AlertDialog}` + `base::DialogHandle` |
| `ui/markdown.rs` | 删除 | `component::text::TextView::markdown`（+ `image_source` / `on_link_click`） |
| `ui/mod.rs` | 删除或保留注释索引 | — |
| `view.rs` 主体 | 重写 | 新引导 + 常驻 InputState + handle 驱动对话框 + 新 spawn 签名 |
| `view.rs::gpui_main` | 重写 | `gpui_kit::application/init/open_window` |
| `app.rs` / `jobs.rs` | 不动 | — |
| `read_password.rs` / `macos_about.rs` | 不动 | — |
| `main.rs` egui 部分 | 步骤 7 删除 | gpui 路径成为唯一实现 |
| Cargo.toml gpui 0.2.2 / spike-runtime-shaders | 删除 | `gpui-kit = "0.7.3"` |

## 7. 风险与决策点

1. **实施范围**：步骤 7（删除 egui）是否纳入本次？默认纳入；保守方案是停在步骤 6，两前端共存。
2. **runtime_shaders 与发布体积/首启延迟**：gpui-kit 默认平台依赖硬编码 `runtime_shaders`，开发机免 Xcode 编译；正式发布首次启动需运行时编译 shader（略慢）。若不可接受，需向 gpui-kit 提 feature 或自管 platform 依赖（本次不做）。
3. **CI 时长**：gpui-pre 全家族 + wgpu 首次编译显著拉长四平台 CI；`Swatinem/rust-cache` 缓解。Linux runner 需确认 wgpu/lwayland 系统依赖（如失败需在 release.yml 补 apt 包）。
4. **字体 panic 面**：任何写死的、目标机器没有的 family 都会 panic；所有 family 选择必须经过 `all_font_names()` 探测。
5. **版本锁定**：GPUI 家族 `=0.3.9` 联动发布，后续升级是整族操作。
6. **Windows 未知项**：gpui-pre 在 Windows 的路径（DirectX/wgpu、输入法）未经本机验证，靠 CI 产物回归。

### 7.7 macOS Big Sur (11.0) 支持核实（2026-10-13）

结论：**gpui-pre 0.3.9 没有不支持 macOS Big Sur**，`MACOSX_DEPLOYMENT_TARGET=11.0` 原样保留。

三方面证据：

1. **编译期探针（决定性）**：临时工程依赖 gpui-pre / gpui-pre-platform（`runtime_shaders`）/
   gpui-pre-apple / gpui-pre-macos 全家族，`MACOSX_DEPLOYMENT_TARGET=11.0 cargo check
   --target x86_64-apple-darwin` 零错误、零 availability 告警。降到 10.15 / 10.12（objc2
   默认下限）同样干净编译。
2. **源码层面**：`gpui-pre-macos` 中所有 12+/15+ API 均为运行时守卫
   （`os_version() >= Version::new(12, 0, 0)`，如 `setAllowsAutomaticKeyEquivalentLocalization`、
   `setDefaultApplicationAtURL`——后者在 <12 时返回错误而不崩溃，本项目也不使用）；
   Metal shader build script 的编译下限显式声明 `-mmacosx-version-min=10.15.7`
   （`gpui-pre-apple/build.rs:138`）；无启动时版本断言或硬性 exit。
3. **上游 Zed 官方文档**（zed.dev/docs/installation）：macOS 11.x Big Sur 为
   "Partially Supported"，唯一原因是协作屏幕共享依赖 ScreenCaptureKit（12+），
   与 gpui 本身无关；Zed 编辑器本体在 Big Sur 运行。

对本文档的影响：§3.1 构建配置中部署目标条款维持原状；步骤 7 后的发布产物在
Big Sur 上运行不受影响。本机 CLT 无 metal 工具链时非 `runtime_shaders` 构建会
在 build script 失败，属工具链问题，与 Big Sur 无关（gpui-kit 默认 platform
依赖已带 `runtime_shaders`，恰好绕过）。

## 8. 参考资料

- gpui-kit 文档：<https://gpui-kit.com> / <https://docs.rs/gpui-kit>
- 本机源码（对照用）：`~/.cargo/registry/src/*/gpui-kit-0.7.3/`、`gpui-component-0.7.3/`、`gpui-base-0.7.3/`、`gpui-pre-0.3.9/`
- 字体探测参考实现：`gpui-component-0.7.3/src/theme/mono_font.rs`
- 组件用法参考：`gpui-component-0.7.3/tests/`（controls / input / overlays 等集成测试）
