# EH 小数评分、严格比较与专用命令按 ID 退订

日期：2026-09-20。状态：已通过计划审查并实现；验证记录见文末。

## 目标与授权边界

- `/esub` 支持有限数值的小数阈值 `rating>=N` 和真正严格的 `rating>N`，阈值范围仍为闭区间 `2..=5`。例如 `rating>4` 接受评分 `4.5`，拒绝 `4.0`，不能转换为 `rating>=5`。
- rating 仍通过取得的 metadata 本地过滤，并沿用现有 rating 扫描窗口、游标、每轮上限和队列处理，不向 EH 搜索请求添加远端 rating 条件。
- 保留专用命令：`/bunsub <list 中代码格式 ID 的内容>`、`/eunsub <list 中代码格式 ID 的内容>`；参数不需要反引号。不统一到 `/unsub`，不改 Pixiv 退订语义。这里的 ID 是完整 `task.value`，不是数据库数字主键。
- `/list` 的 EH ID 使用代码格式，复制后的内容必须仍是原始完整 ID；保留现有 `ch=<频道ID>` 和权限检查。
- 不修复 Booru rating、逗号解析、EH 分类，不改 pages 整数比较，不重做 task-key 协议、扫描策略、下载队列、通用命令框架或历史 spec。
- 当前计划阶段只写本文件，不修改产品代码、测试、配置，不调用工作流代理，不做 Git 写入。实现完成后的新分支、提交、推送由主代理按既有用户授权执行；不另做计划专用提交，不附带 PR、tag、release、rebase 或强推。
- 不读取本地 `config.toml`，不安装工具或依赖，不为 Windows 验证障碍修改项目 Makefile。

## 已核实的起点与接口

计划时工作树 clean，分支 `master`，HEAD `2f8a87a`。用户给定 origin 为 `https://github.com/icceey/pixivbot`；主代理在推送前重新核对目标和工作树。

| 位置 | 当前行为及实现约束 |
| --- | --- |
| `src/db/types/eh_filter.rs` | `min_rating: Option<u8>`，派生 `Eq`，`matches` 实现 `>=`；`aggregate` 取最低门槛；`task_value_signature` 持久化 `rN pN PN` 的固定顺序；`format_for_display` 固定显示 `≥`。 |
| `src/bot/handlers/subscription/ehentai.rs` | `parse_esub_remaining` 把 rating 的 `>` 加一，`parse_eh_filter` 仅接受整数 `>=`；`handle_eunsub` 只把含 `|` 的参数视为 key，无 `|` 则按 query 查询。 |
| `src/db/types/eh_task_key.rs` | `eh:` / `ehq:` key，转义 query 后可含空格；`filter_sig` 是不解释内容的字符串。旧 `r4` key 和原始/编码 query 均有持久化兼容测试。 |
| `src/scheduler/eh_engine/collect.rs` | 对有剩余容量的 subscriptions 聚合过滤，拉取 metadata 后按游标、配置的 `scan_window_hours`、聚合 filter 筛选，再逐 subscription 处理。`fetch_gallery_refs` 只传 query/category/page。不能把既有窗口硬编码成新的 48h 行为。 |
| `src/bot/handlers/subscription/booru.rs` | `handle_bunsub` 先分词，只在一个 token 时调用 `parse_bunsub_internal_key`，导致带空格的完整 key 丢失；该 helper 有 `|` 门槛，解析 tag/ranking 类型。原语法可处理普通无 `|` ID，但会把其中 `scale=day` / `interval=8h` 等字面 query tag 误当控制参数，不能代替完整 ID 查找。 |
| `src/bot/handlers/subscription/list.rs` | Booru 已用代码格式，EH 目前仅对 value 做普通 Markdown 转义；页尾缺 EH 退订提示。 |
| `src/utils/args.rs`、`subscription/helpers.rs` | `parse_args` 只剥离开头参数，余文完整保留；先 `resolve_subscription_target`，再 `delete_subscription(chat_id, task_type, value)`。删除 helper 先核对该 chat 的 subscription，EH 调 `delete_eh_subscription_and_cancel_queue`，最后仅清理孤儿 task。 |
| `src/db/repo/eh_integration_tests.rs` | 已有 upsert 保持 subscription 身份、游标进度并更新 filter 的持久化测试。 |
| `src/db/repo/eh_download_queue.rs` | 已有共享 owner 保留、事务回滚、只取消目标 subscription 等高价值覆盖，不需复制这些测试。 |
| `src/bot/commands.rs` | 命令说明与动态菜单包含 `/bunsub`、`/eunsub` 用法，是需要更新的实际用户帮助入口。 |

实际整数 `min_rating` fixture 出现在 `eh_task_key.rs`、handler 的 `ehentai.rs` 和 `eh_integration_tests.rs`。检查过的 scheduler `tests/collect.rs`、`tests/reuse.rs` 当前只有以 `..Default::default()` 构造的其他 EH filter，没有整数 `min_rating`；不应按过时调查机械修改这些文件。执行时用 workspace 引用检查确认受影响的 literal、派生和 feature 路径。

## 关键实现决策

### 1. 最小评分模型与持久化兼容

推荐将 `EhFilter.min_rating` 改为 `Option<f64>`，增加 `min_rating_strict: bool`，serde 缺省为 `false`。保留 `PartialEq`，移除不再成立的 `Eq`；不引入 decimal crate、通用 comparison enum、定点单位、兼容包装层或新的构造器。

- 两个 rating 操作符均在 `parse_eh_filter` 校验：先识别 `>=`，再识别 `>`，数值必须 `is_finite()` 且落在 `2.0..=5.0`。拒绝 NaN、无穷、溢出和范围外值。使用现有数字解析风格，不新增固定小数位数或舍入规则。
- `parse_esub_remaining` 保留 rating 操作符原意，把数值验证交给上述入口；不再加一。重复 rating 条件延续当前“后出现者覆盖”行为，并同时覆盖数值和严格标志，不能留下之前的严格标志。
- `matches` 分别使用严格 `>` 与非严格 `>=` 的阈值语义；不使用 epsilon、取整、`ceil` 或加一。无 rating 时严格标志没有过滤、签名、展示或扫描效果。
- 旧 JSON 如 `{"min_rating":4}` 继续反序列化为非严格 `>=4`，旧整数阈值含义不变；新 JSON 保留小数与严格标志。沿用当前 JSON 列，无 schema migration，不改已有 task value，不批量重写历史数据。
- 历史 `rating>3` 已被旧版存为 `>=4` 的记录无法还原原意，必须保持现存含义；新严格语义只作用于新解析/新保存的请求。
- task 签名建议用 `r{number}` 表示 `>=`，用 `rgt{number}` 表示 `>`，后续 `p` / `P` 顺序不变。number 用解析后的 f64 标准显示形式，不用原始输入、不强制小数位、不截断。要求 `4` / `4.0` / `4.00` 仍为 `r4`，`4.50` / `4.5` 同为 `r4.5`，严格为 `rgt4.5`。`EhTaskKey` 现有解析不需要认识新比较符号。
- 这是授权评分语义所需的最小持久化扩展，不是整体 key 协议重设计。复用同一个规范化数值输出实现 display，显示 `rating>4.5` / `rating≥4.5`，保持 Telegram 动态文字转义。

**取舍与成本：** metadata 本身使用 f64，沿用它避免额外精度模型；不同十进制文本若超出 f64 可区分精度将视为同值，不承诺任意精度。旧整数签名稳定避免同一旧订阅被新建成另一 task。新小数和 strict JSON 不保证旧二进制能安全读取：禁止在已有新数据后直接把代码降级当作无损回滚，必须先保全数据库并安排前向修复或明确的数据恢复决策。

### 2. 聚合必须是每个订阅 filter 的上界

`aggregate` 沿用当前宽松聚合规则：任何缺失 filter 或缺失 rating 的参与者使聚合 rating 无限制；否则取最低 rating 值。最低值相同时，只要该最低值的任一订阅使用 `>=`，聚合也必须非严格；仅当所有处于最低值的订阅都严格时，聚合严格。较高阈值的非严格订阅不能把较低阈值的严格标志错误覆盖。

保持 min/max pages 和 telegraph 聚合语义；`has_rating_filter` 继续仅看 `min_rating.is_some()`。f64 的最小值比较使用直接的适当浮点比较，不新增“排序评分”抽象。合法命令已经排除非有限阈值，不为假设中的不可能值扩展业务分支。

**风险：** 聚合若在等阈值时错误取严格，会在逐订阅过滤之前永久丢掉本应交给非严格订阅的 gallery。即使新 key 将 `>` / `>=` 分开，现有 Repo 支持同 task 的 filter 更新，不能因此破坏聚合契约。不为了本次改动让不同新签名强制共享 task。

### 3. EH ID 与真实 query 的歧义裁决

所有选择限定在 `resolve_subscription_target` 得到的目标 chat，且只看 EH subscription。保留原始 `task.value` 用于删除，不把旧 value 解码后再编码。

1. 对 trim 后的完整 remaining 调 `EhTaskKey::parse`。一旦识别为合法 `eh:` / `ehq:` key，就仅在目标 chat 的 EH subscriptions 中找 `task.value == remaining`；精确匹配才选中，不存在直接返回未找到，绝不回退 query。无 `|` 的 `eh:foo`、`ehq:...` 同样适用。
2. 未识别为合法 key 且 remaining **不含 `|`** 时，才沿用原 query 匹配：用 `EhTaskKey::parse` 后的 `query == remaining` 查找。一个匹配才删除；多个匹配提示 `/list` 后使用完整 ID；零个匹配返回未找到。
3. 未识别为合法 key 且 remaining **含 `|`** 时，维持原内部 key 分支的边界，返回无效标识，不转为 query、不跨 chat 找候选。原来 raw query 含 `|` 就不能走 query 退订，不在本次顺带扩展；`/list` 输出的 `ehq:` ID 可用。
4. 实际删除仍走现有 `delete_subscription(target_chat, TaskType::Ehentai, original_value)`，不直接删除全局 task 或绕过 EH 队列取消事务。并发导致已删除时按未找到处理，不重选另一个目标。

**安全裁决与兼容成本：** 同 chat 中 A 的 ID 为 `eh:foo`，B 的字面 query 为 `eh:foo`、ID 为 `eh:eh:foo` 时，`/eunsub eh:foo` 仅可删除 A；重复执行必须返回未找到，B 始终不受影响。B 必须用自己的完整 ID `/eunsub eh:eh:foo` 退订，即使 A 从来不存在也不允许把合法 key 当 B 的 query。这是已授权 ID 优先契约所需的防误删规则，代价是字面 query 恰为合法 key 的旧简写不再支持。帮助说明“合法 eh:/ehq: ID 只精确匹配，未找到不会按搜索词重试；搜索词本身看起来像 ID 时，请复制 /list 中它自己的完整 ID”。无需引入新转义或确认机制。

仅对该 handler 修改所触及的失败路径遵守现有安全规范：内部 DB 错误记 `tracing` 的 `{:#}` 链，用户收到简短友好文本；不继续在重写后的分支透传原始 DB/anyhow 错误，也不扩成全项目错误处理重构。

### 4. Booru 目标 chat 完整 ID 优先，再保留原命令语法

在 `handle_bunsub` 经 `resolve_subscription_target` 获取目标 chat 后，以 `parsed.remaining.trim()` 为完整候选，按以下优先级选择：

1. 查询目标 chat 中受支持的 Booru subscriptions（`BooruTag` / `BooruRanking`），先按完整 `task.value` 精确匹配，不要求含 `|`，不分词或重建。匹配到后记录该记录的原 `task.type` / `task.value`，直接交现有 `delete_subscription` 删除，不从 tag 文本猜测类型。
2. 无精确匹配时，才把完整 remaining 交现有 `parse_bunsub_internal_key`；成功则沿用其类型识别并保留原文。这解决含空格和 `|` 的内部 key 被截断的问题。
3. helper 也不匹配时，才进入原分词/条件解析。保留 helper 的 `|` 门槛，不重设计 `BooruTaskKey` 编解码或原 `order=`、`scale=`、`interval=` 参数语法。当前 handler 没有 Pool 专用解析，本次不新增 Pool 功能。

**安全裁决与兼容成本：** 无 `|` ID 也可能包含普通查询 tag `scale=day` 或 `interval=8h`，例如 `yd:landscape scale=day`。已订阅的精确 ID 必须先命中原 tag task，不能误取消或寻找另一 ranking task。即使文本也符合旧命令语法，目标 chat 的精确 ID 优先；用户若要选择另一条订阅，应复制它自己的完整 ID。未命中才保持旧语法兼容，不因此把任意 `site:...` 文本都宣告为内部 ID。查询失败必须终止并给友好错误，不把 DB 故障当作“未匹配”继续解析；精确选中后并发删除导致失败也不得重选或回退。帮助说明“优先匹配当前目标聊天的完整 ID，未匹配才按原站点/标签/参数语法解析”。

## 实现波次与结果证据

### 波次 A：评分端到端语义与持久化身份一致

**结果：** 输入解析、数据库 JSON、task 身份、展示、共享聚合和逐订阅过滤对同一个评分条件达成一致，旧整数订阅可继续使用。

主要文件：`src/db/types/eh_filter.rs`、`src/bot/handlers/subscription/ehentai.rs`、`src/db/types/eh_task_key.rs` 的现有 fixture、`src/db/repo/eh_integration_tests.rs`；scheduler 生产逻辑预期无需改动，发现真实依赖时再作最小修改。

实现按上述模型连通所有调用点，查找全 workspace 的 `EhFilter` 构造和 `Eq` 依赖。整数 fixture 仅在编译需要处改浮点；保留旧 task-key 持久化测试原本的 expected key。

必要证据（优先改已有测试，避免重复小测试）：

- 调整 `subscription_options_parse_through_to_filters`，使其保护两阶段解析曾把 strict 改成 `+1` 的真实错误：通过实际两个解析函数得到 filter，再观察等阈值 gallery 与略高但不到下一整数的 gallery 是否被正确选中。不再仅把一组字段赋值断言换成另一组；同一入口的非有限数值拒绝可保护比较绕过风险，不扩展成全数值真值表。pages 原有整数转换仍保持。
- 在 `eh_subscription_upsert_updates_filters_without_replacing_subscription` 或贴近它的最小持久化覆盖中，使用真实旧 JSON fixture 验证历史 filter 从 DB 读出仍表示 `>=`，再保存小数严格条件并重新读取，证明 subscription 身份和进度未被替换。将 task 创建/查找的身份风险一并覆盖：`>=4.50` 与 `>=4.5` 不重复创建任务，而 `>4.5` 不覆盖 `>=4.5` 的订阅。需要时拆成两个关注不同持久化失败机制的测试，不能只断言新签名字符串或 derived round trip。
- 检查现有 `tests/collect.rs` 是否可自然承载真实共享扫描回归；当前没有直接覆盖同阈值 strict/inclusive 聚合。确有缺口时新增一个小型真实 `execute_eh_task`/`tick` 场景，复用现有 mock：同 task 的订阅在相同阈值分别 strict/inclusive，mock 提供等阈值与略高评分 metadata，通过持久化 queue owner/订阅状态证明聚合没有吞掉非严格订阅的边界 gallery，而 strict 不接收它。使用窗口内时间戳，让现有窗口实际参与；观察搜索请求仍不含 rating 参数。不要手工调用聚合后仅比较字段，或复制算法生成期望列表。
- 窗口、游标、队列和去重其余语义用现有测试和生产 diff 检查保障；`tests/reuse.rs` 不为追求覆盖额外改动。

### 波次 B：列表 ID 可用，退订范围与事务不变

**结果：** 用户从 `/list` 复制完整 ID 内容，不加反引号，使用对应专用命令能删除目标 subscription；EH 重复提交已删除的合法 ID 不会误删同名 query 的 subscription，Booru ID 内的字面控制参数 tag 不会被误解；其他 chat、共享 task 的其他 owner 和任务不被错误删除。

主要文件：`src/bot/handlers/subscription/{ehentai,booru,list}.rs`、`src/bot/commands.rs`。读取/复用 `helpers.rs` 和 Repo 删除事务，预期不改其接口。

- 实现上述 EH 合法 key 只精确匹配、无匹配不回退的裁决；Booru 在原 helper/旧解析前查询目标 chat 的完整 ID，沿用实际记录的类型和值。两者都保持目标授权、查询错误终止与选中后不重选。不要新增统一 `/unsub` 分发器、通用订阅 resolver 或纯转发 wrapper。若需从 handler 提取可测试的选择逻辑，只提取真正生产使用的决策，不保留旧实现为 test-only 代码。
- EH 列表为完整 `task.value` 添加代码格式。使用正确的 MarkdownV2 代码实体转义，不能用普通文本转义后导致代码内复制内容带多余反斜杠。检查 Booru 当前同样的代码格式输出，仅在复制 ID 契约确实需要时修正它的代码实体转义；不做无关 caption 重构。
- 页面包含 EH/Booru 时增加对应 `/eunsub <ID>` / `/bunsub <ID>` 提示，频道列表继续带 `ch=...`，普通列表不丢 Pixiv 提示。更新命令菜单、空参数用法及 EH `/esub` 实际 help：小数、2–5、严格与非严格、既有扫描窗口（48h 为默认，不重新定义配置）。说明 ID 是代码格式文本的内容，用户不用输入反引号；详细退订帮助同步 EH 合法 key 不回退、字面 query 看起来像 key 时使用自身 ID，以及 Booru 目标 chat 完整 ID 优先于旧参数语法的规则，不能再笼统承诺“ID 不存在则按搜索词重试”。历史 spec 无需同步。

必要证据：

- 以现有 handler/Repo 测试设施为基础，从完整 remaining 或真实 handler 入口覆盖含空格、含 `|f=` 的 Booru ID；不能只扩充已经接受空格的 `parse_bunsub_internal_key` 测试，那样无法发现 handler 仍提前分词的原故障。在同一必要退订验证中加入无 `|`、含字面 `scale=day` 或 `interval=8h` 的真实 tag subscription，并保留一个旧解析可能指向的 ranking subscription：用复制的 tag ID 退订，断言只删除原 tag、ranking 仍在，且采用持久化记录的类型和值。用未命中精确 ID 的旧参数调用验证 fallback 接线仍可用，不扩成参数组合真值表。
- EH 的必要退订验证实际建立同 chat A（ID `eh:foo`）和 B（query `eh:foo`、ID `eh:eh:foo`）：调用 `/eunsub eh:foo` 删除 A 后再次调用，断言第二次未找到且 B 及其队列 ownership 仍保留；随后用 B 自己的完整 ID 才能删除 B。保留无 `|` 与编码 query ID 的原入口证据。至少使用一个跨 chat 共享 task 的实际 Repo 状态观察：取消本 chat 后另一 chat subscription 仍在，不能只测选择真值表。若没有现成 handler mock，使用最小现有依赖构造加本地 Telegram mock；不为测试新建业务包装层。
- 沿用 `eh_download_queue.rs` 中 `cancel_one_shared_delivery_preserves_remaining_job_ownership`、`cancellation_rolls_back_owner_and_subscription_when_job_update_fails` 等已有事务证据。只有选择接线验证显示缺口才扩展 owner 场景，不复制完整事务测试套件。
- 用户界面验证要观察实际消息 code entity / 复制后的文本与删除对象，而不是源码字符串、反引号前后缀或命令描述常量断言。优先本地 bot API mock 观察消息 payload；有可用且已授权的测试聊天才做真实 `/list` → 复制 ID → 退订闭环，不使用生产订阅作破坏性探针。不具备真实 Telegram 环境时明确记录未验证的客户端渲染/复制行为。

波次 A 与 Booru 识别/列表展示可独立推进，但 EH handler 和帮助存在共享文件，执行者须避免覆盖对方更改。最终集成必须同时校对新评分签名在 `/list` 与 `/eunsub` 的完整原文路径。执行者可调整内部顺序和等价细节，但不能改变此处语义、授权或安全边界。

## 验证预算与交付门槛

### 测试必要性

每个新增或保留的改动测试须说清失败机制与独有的可观察后果：两段解析丢失 strict、持久化导致重复 task/进度丢失、聚合提前漏掉 gallery、handler 截断 key、EH 重复 ID 回退误删同名 query、Booru 把 ID 内的字面 tag 当控制参数而选错 subscription。这些才是理由，而非“新分支”“提高覆盖率”。不新增配置/布尔真值表、标签/签名字符串拼接、访问器、字段赋值、简单 enum 比较或大规模合成数据测试。必要性复核发现重复覆盖时删除其专用 fixture/helper，不影响本任务无关的既有测试。

### 有限执行与环境限制

基线说明：Rust 固定 `1.94`；用户报告已有离线 MSVC cache，但此前两次构建在 `aws-lc-sys` 等阶段超时，尚没有测试运行成功的证据。本计划阶段不重新启动构建。

执行阶段按以下有界策略取证：

1. 用 `rustup show active-toolchain` 和已安装工具清单确认实际 toolchain，不触发安装。所需 1.94/rustfmt/clippy/make/FFmpeg 不存在时直接记录环境阻塞，不切换未经同意的版本或下载依赖。已有离线环境优先通过 PowerShell `$env:CARGO_NET_OFFLINE = 'true'` 保持离线，结束后恢复调用前的环境值。
2. 格式检查后只做一轮最相关测试组，避免每个 fixture 都独立重编译。候选：`cargo test -p pixivbot subscription`、`cargo test -p pixivbot eh_task_key`、`cargo test -p pixivbot eh_integration_tests`、`cargo test -p pixivbot scheduler::eh_engine::tests::collect`，以及上述具体取消事务测试；根据实际测试名称合并/缩小调用，不以空匹配的 0 tests 当作通过。
3. 必须尝试 `make ci`。可执行 Makefile 是源事实：fmt-check → clippy → check → test → release build；最终 build 是 `cargo build --release --workspace --features ffmpeg-codec`，不要遗漏。
4. 如果唯一阻塞是 Windows 无法执行 Makefile 内 `RUSTFLAGS="..." cargo ...`，保留失败证据，使用 PowerShell 逐项执行相同 cargo 目标，不改 Makefile、不启动其他 shell。clippy 的环境变量用 `$env:RUSTFLAGS = '-Dwarnings'`，之后恢复先前值；其余目标保留 Makefile 原作用域，不随意改变整个构建的 flags 造成全量重编译。等价目标为 `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo check --workspace --all-targets`、`cargo test --workspace --all-targets`、`cargo build --release --workspace --features ffmpeg-codec`。
5. 针对 cache 未完工的编译，给一次合理的较长等待窗口，例如 15 分钟，而非重复短超时。首个编译阶段若仍超时或遇缺失离线依赖/FFmpeg，停止会重复同一阻塞的后续 cargo 调用，报告实际停在哪个 crate/阶段、哪些测试未运行；不能声称 CI 通过。后续只有输入或环境实质改变才重跑。`make ci` 的必要尝试不构成无限重试许可。
6. 常规测试不额外开启 `ffmpeg-codec` 的 H.264 测试；仅在已确认本地 encoder 可用时运行，release build 所需 feature 按原 Makefile 执行。不能因相关测试尚未执行而改为“绿色完成”。
7. 审阅最终 diff 与受影响完整路径，运行 `git diff --check`，检查错误暴露、MarkdownV2、权限、JSON/key 兼容、共享 ownership 和状态推进，以及无用抽象/重复测试。只清理本次启动且明确归属的后台进程和临时 fixture，不广泛杀进程或删除 cache。已有输入和成功证据未变，不重复跑同一检查。

本计划文件本身是 Markdown-only：仅做计划内容自审和 `git diff --check -- docs/superpowers/plans/2026-09-20-eh-rating-and-unsubscribe-ids.md`，不为写计划运行 Rust CI。

### 主代理交付与 Git 操作

主代理先完成 blocker-focused `plan-critic` 审核，再执行实现；审查 blocker 需修正、以证据反驳或上报，不通过不断全量重审制造形式门槛。

交付报告应区分实际通过、未运行和受环境阻塞的验证，说明原始代码/数据不再能安全降级的限制及 EH prefix/query 歧义裁决。实现完成后按用户授权在新分支提交并推送 origin，不在 master 上提交：提交前检查 `git status`、最终 diff、`git log --oneline -10`，仅暂存本次文件，使用 semantic title + 简短 body，不加 trailer。推送前核对分支、remote 和提交范围，不强推，不另开 PR；若验证阻塞影响“完成”判断，由主代理如实说明而非静默把未验证实现认定为完成。

## 计划自审结果

- 覆盖小数/strict、2–5、旧 JSON 与旧 key、规范化、宽松聚合、本地过滤和窗口不变。
- 明确专用退订命令、完整 remaining、合法 EH ID 不回退 query、Booru 目标 chat 精确 ID 优先于参数解释、chat 隔离、事务和队列 owner，不引入全局按 task 删除。
- 证据优先复用现有生产入口与持久化/事务测试；校正了 scheduler fixture 调查差异，避免机械修改和无价值测试。
- 已修正两项计划审查阻塞：EH 重复删除合法 ID 不得误删字面同名 query；Booru 无 `|` ID 内的控制参数形状 tag 不得绕过精确匹配。对应帮助、波次和必要退订验证已同步。f64 精度和回滚成本仍显式保留。

## 实施与验证记录

- 已实现小数/严格评分、旧 JSON 默认非严格、旧整数签名稳定、共享扫描的宽松聚合，以及列表完整 ID 的专用命令退订。无需迁移。
- 本地 Telegram API mock 从真实 `/list` payload 解码代码格式内容，再调用退订 handler；覆盖特殊字符、含空格 ID、Booru 控制参数形状标签、EH 重复 ID 和跨聊天隔离。没有连接真实 Telegram 客户端，未验证客户端渲染/复制。handler 场景没有另建队列 ownership fixture，事务取消由既有 Repo 测试覆盖。
- 最终测试必要性审查删除了用 `4.50` / `4.5` 浮点字面量再调用 Repo 比较任务 ID 的新增断言：这只是经数据库层重复简单签名断言，不能证明文本解析。保留旧格式读取后更新订阅而不丢身份/进度的既有测试，以及真实调度队列结果测试；任务签名的规范化和 strict 区分另由生产差异审查确认。
- 显式使用已安装的 `1.94-x86_64-pc-windows-msvc`（rustc/cargo 1.94.1），不依赖目录的 stable override；格式检查、全工作区 Clippy（`RUSTFLAGS=-Dwarnings`）、check 和全工作区测试通过。
- `make ci` 未启动：本机未安装 `make`。已逐项执行对应 Cargo 检查；release build（`--release --workspace --features ffmpeg-codec`）因离线缓存缺少 `ffmpeg-next v9.0.0` 受阻，不能声称完整 CI 或发布构建通过。未安装软件、未读取私有配置。
