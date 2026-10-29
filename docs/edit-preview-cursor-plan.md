# 编辑/预览光标位置与焦点保持修复

## 背景与问题

笔记正文在编辑态移动光标后：

1. 切换到预览时，预览没有停在编辑时光标所在行附近。
2. 预览没有立即获得焦点，`j`/`k` 需要先点击正文才能滚动。
3. 再次切回编辑态时，光标和编辑器滚动位置回到开头，而不是上次的位置。

## 目标

1. 编辑态移动光标后，切换到预览时，预览滚动到编辑时光标所在行附近，并保持光标在视口中的相对位置。
2. 切到预览后焦点立即落在正文区域，`j`/`k` 无需点击即可滚动。
3. 再次切回编辑态时，恢复上次记录的光标位置和编辑器滚动位置。

## 决策

1. 预览定位采用“光标行保持相同屏幕相对位置”，而不是简单把光标行滚到顶部。
2. 光标位置按笔记 id 记忆，切换笔记时清空。
3. Markdown 预览无法按源码行精确锚定块，继续按行比例近似；软换行、图片高度差异属于已知限制。

## 改动范围

仅 UI 层，无数据模型、存储或同步改动：

- `crates/app/src/view.rs`
  - `NoteBrowser` 新增字段：编辑位置记忆、预览锚点、待恢复焦点标记。
  - `finish_edit`：捕获光标/滚动和预览锚点，退出后聚焦正文。
  - 150ms tick：应用预览滚动、恢复编辑光标、兜底修正焦点。
  - `body_key`：用户滚动时取消待应用的预览锚点。
  - 预览/编辑切换、工具栏按钮、`⌘E`、`:q`、`:wq`、`ZZ` 共用 `finish_edit` 的行为保持一致。
- 纯函数位置换算与单元测试位于 `view.rs` 末尾测试模块。

## 实施步骤

### 1. 记录并恢复编辑光标

新增：

```rust
struct EditorMemory {
    note_id: String,
    cursor: usize,
    scroll_y: f32,
}

last_editor_memory: Option<EditorMemory>,
pending_edit_restore: Option<EditorMemory>,
```

- `finish_edit` 在 `end_edit()` 前读取 `note_edit.cursor()` 和 `note_edit.scroll_offset().y`，连同当前笔记 id 存入 `pending_edit_restore`。
- tick 的 `edit_epoch != edit_epoch_seen` 分支中，在 `set_value(body)` 之后：
  - 若 `pending_edit_restore.note_id == selected_note`，按字节偏移计算 `Position`，调用 `set_cursor_position(pos, window, cx)`，再调用 `set_scroll_offset(point(px(0.), px(saved_y)), cx)`。顺序保证滚动位置覆盖 `set_value` 和 `scroll_to` 的默认值。
  - 恢复后清空 `pending_edit_restore`，并把焦点放回编辑器。
  - 偏移越界时按当前文本长度裁剪，避免旧位置大于新正文。
- 现有 tick 的“切换笔记”分支中，同时清空编辑位置记忆，防止把上一篇光标应用到新笔记。

### 2. 预览滚动改为按光标行定位

用结构化锚点替换 `pending_preview_ratio: Option<f32>`：

```rust
struct PreviewAnchor {
    note_id: String,
    cursor_line: f32,
    total_lines: f32,
    screen_fraction: f32,
}

pending_preview_anchor: Option<PreviewAnchor>,
```

- `finish_edit` 捕获：
  - `cursor_line = cursor_position().line`
  - `total_lines = text().lines().count().max(1)`
  - `screen_fraction = clamp(caret_top / line_height / viewport_rows, 0..1)`，即光标在编辑视口中的相对垂直位置。
- 新增纯函数：

```rust
fn preview_offset_for_cursor(
    cursor_line: f32,
    total_lines: f32,
    screen_fraction: f32,
    max_offset: f32,
    viewport: f32,
) -> f32
```

按 `content = max_offset + viewport`、`offset = -(content * line_ratio - viewport * screen_fraction)` 计算并夹到 `[-max_offset, 0]`，使光标所在行大致保留在预览视口的相同相对位置。

- tick 拿到预览布局后调用该函数并 `set_offset`；`max_offset == 0` 时直接清空锚点，不再重试。保留现有重试上限，处理预览首帧尚未布局的情况。
- `body_key` 的 `j`/`k` 一旦滚动，立即清空 `pending_preview_anchor`，避免 tick 迟到的定位覆盖用户操作。

### 3. 预览焦点与 j/k

- `finish_edit` 结束编辑后改为调用 `focus_body_focus()`，此时 `editing == false`，焦点落在 `body_focus`，`j`/`k` 立即生效。
- 新增 `pending_body_focus: bool` 兜底：
  - `finish_edit` 设置该标记并立即尝试聚焦。
  - tick 中若标记仍存在、当前不是编辑态且 `body_focus.is_focused(window) == false`，再次聚焦正文；然后无条件清除标记，避免用户快速切回编辑时被抢焦点。
- 保留预览容器的 `.track_focus(&self.body_focus)` 和 `on_key_down(body_key)`，不改变按键逻辑。

### 4. 入口统一

- 预览到编辑的所有入口（`⌘E`、工具栏“编辑”、预览态 `⌘F` 触发的编辑）都经过 `begin_edit` 和 tick 播种。
- 把“恢复编辑器位置”抽成 `restore_editor_position(window, cx)`，tick 播种分支和 `find_in_note` 的手动播种分支都调用，保证预览态 `⌘F` 也不会丢失光标记忆。

## 验证

- 单元测试：
  - `preview_offset_for_cursor`：首行、末行、中间行、无滚动、视口为 0、目标越界夹紧。
  - 编辑记忆按 note id 匹配与越界偏移裁剪的纯逻辑测试。
- `cargo test -p joplin-rs-app`
- `cargo build -p joplin-rs-app`
- 手工 GUI 回归：
  1. 打开长笔记，编辑态移动到中部某行，`⌘E`，确认预览停在该行附近，直接按 `j`/`k` 立即滚动。
  2. 再按 `⌘E`，确认光标回到离开时的行，编辑器视口也回到原位置。
  3. 用工具栏“预览/编辑”按钮重复，覆盖非快捷键路径。
  4. 在正文末尾、顶部、含标题/图片/长段落的笔记各验证一次，确认无跳顶或跳到文档末尾。
  5. 预览态 `⌘F` 进入编辑查找后，再退出/进入，确认光标恢复与查找面板行为不冲突。
  6. 切换笔记后确认旧位置不会串到新笔记。

## 实测结果（2026-10-29，隔离数据目录）

- 用 `target/debug/joplin-rs-app` 启动，`JOPLIN_RS_DATA_DIR` 与 `HOME` 指向临时副本，未触碰真实数据。
- 工具栏“编辑”进入 NORMAL 模式，`10j` 移动光标后点“预览”，预览停在该行附近。
- 预览切换后直接按 `j`/`k`，页面立即按半屏滚动，无需先点击正文。
- 再切回“编辑”，光标和编辑器滚动位置恢复到离开时的行。
- 在文档末尾用 `G` 定位后重复上述流程：预览落到文档末尾附近，按 `k` 滚动后再切回编辑，仍回到末尾光标和原滚动位置。
- 单测 `cargo test -p joplin-rs-app` 85 项通过（含光标行定位换算、编辑记忆按笔记匹配），`cargo check`、`cargo build -p joplin-rs-app` 通过。
