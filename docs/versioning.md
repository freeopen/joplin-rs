# 发布式重置的版本号生成逻辑

> 实现位置：`crates/core/build.rs`（git 编排）+ `crates/core/src/version.rs`（纯逻辑，可被 `cargo test` 覆盖）
> 发布流水线：`.github/workflows/release.yml`

本项目的版本号**不写死在 `Cargo.toml`**，而是在**编译期从 git 推导**：

```
X.Y.Z = 最近一个 vM.I.P tag（或 Cargo.toml 基线）
        + tag 之后所有 Conventional-Commits 提交的"发布式重置"bump
```

本文档说明这条逻辑的规则、边界与验证方式。

## 1. 整体流程

```
CARGO_PKG_VERSION ─┐
                   ├─→ derive_version() ─→ "M.I.P" ─┬─→ JOPLIN_RS_GIT_VERSION
latest tag ────────┤                                │
                   │                                └─→ version_display() ─→ JOPLIN_RS_VERSION_DISPLAY
tag..HEAD 的提交 ──┘                                      = "vM.I.P (<hash9>)"
```

- `build.rs` 只做三件事：跑 git、把结果交给纯逻辑、导出两个环境变量。
- 纯逻辑放在 `src/version.rs`，build script 通过 `#[path]` 复用同一份源码，以便 `cargo test` 能覆盖（build script 自己的 `#[cfg(test)]` 不会运行）。
- `cargo:rerun-if-changed` 监听 `src/version.rs`、`.git/HEAD`、`.git/refs`（锚在仓库根，因为 build script 的 cwd 是 crate 目录）。

## 2. 环境变量契约

| 变量 | 内容 | 用途 |
| --- | --- | --- |
| `JOPLIN_RS_GIT_VERSION` | 纯 `M.I.P`，无 `v` 前缀、无 hash | CI 用它建 tag |
| `JOPLIN_RS_VERSION_DISPLAY` | `vM.I.P (<hash9>)`，非 git 环境省略 `(hash)` | `--version` 显示 |

- 不在 git 仓库里：tag 与 hash 都拿不到，`--version` 直接显示 `v{CARGO_PKG_VERSION}`，无 hash。
- 解析失败：`derive_version` 返回 `None`，`build.rs` 回退成裸 `CARGO_PKG_VERSION`（**不做任何 bump**）。

## 3. 基线的选取

- **有 tag**：`git describe --tags --abbrev=0 --match 'v[0-9]*'` 取最近的 `v` 开头数字 tag 作为基线。tag 形如 `v0.1.46-g76a78f8`（CI 自动 tag）时先剥掉 `-g<hash>` 再解析；没有 `v` 前缀的 tag 被忽略。
- **无 tag**：用 `CARGO_PKG_VERSION`（workspace 版本，当前是 `1.0.0`）作为基线。
- **提交范围**：有 tag 时只统计 `git log vM.I.P..HEAD`（最近发布之后的提交）；无 tag 时统计全量历史。

> 注：真正的"重置"发生在**统计区间**上 —— 每次发版建 tag 后，后续版本的计数从零重新开始，不会把上一个发布线里的提交再次计入。

## 4. 提交分类（`classify`）

只看提交主题的**第一行**、冒号**之前**的部分：

| 主题形态 | 判定 | 级别 |
| --- | --- | --- |
| `feat: ...` / `feat(app): ...` | Minor | `feat` |
| `fix: ...` / `fix(sync): ...` | Patch | `fix` |
| `feat!: ...` / `feat(api)!: ...` / `fix!: ...` | Major | `!` |
| `chore:` `docs:` `test:` `refactor:` `Merge ...` 无冒号 | None | 不计 |

细节：

- **`!` 优先级最高**：先判 `!`，所以 `feat!: ...` 只算 Major，不会再算 Minor。
- 类型名大小写不敏感（`eq_ignore_ascii_case`）；`scope` 括号内容不参与判定。
- 没有冒号、或冒号前为空，一律 None。

## 5. 发布式重置（`bump`）——核心规则

从基线 `(M, I, P)` 与一组提交主题出发，**取最强级别、只应用一次、低位清零**：

```
highest = max(classify(每条提交))        # None < Patch < Minor < Major
match highest:
    Major → (M+1, 0, 0)
    Minor → (M, I+1, 0)
    Patch → (M, I, P+1)
    None  → (M, I, P)      # 版本不变 → release 被跳过
```

这与"逐条累加"的关键区别：

- **只应用一次**：3 个 `feat` 也只升一次 minor；5 个 `!` 也只升一次 major。
- **低位清零**：升 major 时 minor/patch 归零；升 minor 时 patch 归零。这是 semantic-release、commitizen 等主流工具的默认行为。
- **`!` 会吃掉其它提交**：同一区间里只要有一个 `!`，无论还有多少 feat/fix，结果都是 `(M+1).0.0`。

> 历史：旧逻辑是"逐条累加、不重置"，曾把 1.0.0 基线 + 6 feat + 12 fix 算成 `1.6.12`。重置规则把它修正为 `1.1.0`。

## 6. 例子

基线 tag 为 `v1.4.2`：

| tag 之后的提交 | 结果 | 说明 |
| --- | --- | --- |
| `fix: a` | 1.4.3 | 单个 fix |
| `fix: a`, `fix: b` | 1.4.3 | fix 只升一次 |
| `feat: a`, `fix: b` | 1.5.0 | minor 升位，patch 归零 |
| `feat: a`, `feat: b` | 1.5.0 | feat 只升一次 |
| `feat!: 破坏性改动` | 2.0.0 | `!` 升 major，minor/patch 归零 |
| `fix: a`, `feat!: b` | 2.0.0 | `!` 优先级最高，且只应用一次 |
| 只有 `chore: x`, `docs: y` | 1.4.2（不变） | 无 feat/fix，跳过发布 |

无 tag 路径（本仓库首个 tag 建立前的形态）：

```
无任何 tag，CARGO_PKG_VERSION = 1.0.0
全量历史中有 feat=6, fix=12, 无 `!`
→ 1.0.0 → minor 升一级 → 1.1.0
```

## 7. CI 如何消费

`.github/workflows/release.yml`：

1. 编译后从二进制 `--version` 用 `sed -E 's/.* v([0-9]+\.[0-9]+\.[0-9]+).*/\1/'` 剥出纯 `X.Y.Z`。
2. 日常 push（非 tag 触发）：`TAG_NAME=v${VERSION}`，在**当前提交**上建 tag 并发布 Release。
3. 版本不变（本次 push 无 feat/fix）：tag 已存在，跳过发布，避免把新产物挂到旧提交的 tag 上。
4. 人工 push `vX.Y.Z` tag：Release 直接绑定该 tag。

`version_logic.rs` 中的 `display_string_is_parseable_like_ci_sed` 测试与这条 sed 表达式一一对应，保证显示格式永远可被 CI 解析。

## 8. 验证

```bash
# 单元测试（含 reset 语义、§4 边界与 CI-sed 契约）
cargo test -p joplin-rs-core version

# 查看当前推导结果
cargo run -p joplin-rs-cli -- --version
# -> joplin-rs vM.I.P (<hash9>)

# 模拟 CI 解析（应输出纯 X.Y.Z，无 v 前缀）
cargo run -q -p joplin-rs-cli -- --version | sed -E 's/.* v([0-9]+\.[0-9]+\.[0-9]+).*/\1/'
```

## 9. 与 xray 上游方案的差异

- 本方案把 `bump` 从"逐条累加"改成"发布式重置"（`1.6.12` → `1.1.0`）；joplin-rs 已采用同款发布式重置语义（`crates/core/src/version.rs` 的 `bump`），并有与本文 §4/§5/§6 例子一一对应的单元测试（`classify_conventional_subjects`、`bump_applies_highest_level_once_and_resets`、`docs_examples`）。
- 变量命名沿用 joplin-rs 约定：`JOPLIN_RS_GIT_VERSION` / `JOPLIN_RS_VERSION_DISPLAY`。
- 其余契约保持一致：`#[path]` 共享纯逻辑、`-g<hash>` tag 解析、双环境变量、`rerun-if-changed` 锚定仓库根。
