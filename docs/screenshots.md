# 提交前截图视觉检查

在改动卡片/表单/路由/概览等页面后，用固定的 mock 数据把关键界面离屏渲染成 PNG，
提交前肉眼对比基线，避免“编译通过但布局坏了”的问题。图片不进版本库，基线是每
位开发者本地的 `.ox/screenshots/baseline/`。

## 运行

```bash
# 1. 重新渲染全部场景（退出 0 表示渲染成功）
cargo run --bin screenshots --features screenshot-test

# 2. 首次：把当前输出采纳为基线
scripts/screenshot-diff.sh --accept

# 3. 之后每次改动页面后：
cargo run --bin screenshots --features screenshot-test
scripts/screenshot-diff.sh          # 只打印变化的场景名；有变化退出 1
```

`screenshot-test` 是可选 feature：它把 `gpui-kit/test-support`、图片降采样依赖
和截图 bin 一起挡在默认生产构建之外。不带该 feature 的 `cargo build` 不会编译
截图工具。

输出目录 `.ox/screenshots/out/`、基线 `.ox/screenshots/baseline/` 都在 gitignore
的 `.ox/` 下，不会提交。`--accept` 会用当前 `out/` 覆盖整个 `baseline/`。

## 场景表

| 场景名（PNG 文件名） | 页面 | Tab | 额外动作 |
| --- | --- | --- | --- |
| `providers-cards` | 账号卡片网格（含额度/余额曲线） | 账号 | — |
| `providers-form-new` | 新建账号表单 | 账号 | 打开新增表单 |
| `providers-form-edit` | 编辑账号表单 | 账号 | 打开已有账号（key 10）的编辑表单 |
| `routes` | 路由表 | 路由 | — |
| `usage` | 概览（KPI + 趋势图） | 概览 | — |
| `users` | 用户页 | 用户 | — |

视口 1160×1020 逻辑像素；无头平台按 2× 设备像素渲染，工具会把截图降采样到
**1160×1020** 的真实像素尺寸，保证跨机器可比。

## 数据来源与确定性

- HTTP 流量来自进程内的 loopback mock server（固定端口 `127.0.0.1:18787`）与
  `GatewayRuntime::spawn`，GUI 走真实的 `GatewayHttpClient` 代码路径；不连生产
  gateway，不读数据库，也不需要任何凭据。
- mock 数据放在 `fixtures/screenshots/bootstrap.json` 与 `extras.json`，用 cardinal
  的 `gateway::admin_ops::Bootstrap` 等 DTO 做启动时反序列化校验：DTO 字段一改，
  工具立即报错，而不是渲染出空白页。
- fixture 只放 credential-free 的 view 数据；示例 key 字符串是明显的占位符，不是
  真凭据。时间/id 都是固定值。注意：卡片上的相对时间标签（如“7 小时前”“6 天后”）
  由生产格式化代码按当前时间计算，fixture 时间虽固定，跨小时/跨天运行时这些文字仍会
  有可解释的漂移（因此基线属于按天/按会话的本地快照，跨时段对比前先重新渲染并 `--accept`）；
  同一时段内连续运行的 PNG 字节一致。无需为此改动生产格式化代码。
- 等待数据加载用的是“mock 请求数静止 + 硬超时”的有界轮询，不是固定 sleep。

## 维护

- 新增场景：在 `src/screenshots.rs` 的 `SCENES` 里加一项，然后 `--accept` 更新基线。
- DTO 变更导致 fixture 失效：按启动报错修正 JSON 字段；故意的视觉变化按上面流程
  `--accept` 后提交代码改动（基线本身不进版本库）。
- 本工具只增加装配函数与 mock 基础设施，不改变生产界面行为；`src/main.rs` 未调用
  `run_screenshots`。
