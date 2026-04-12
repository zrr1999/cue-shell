# Cue Shell 设计文档

> 基于完整设计讨论的结构化设计文档。
> 命令与模式的详细参考见 [commands-and-modes.md](./commands-and-modes.md)。
> 调研与决策过程见 [research/](./research/) 目录。

---

## 一、项目定位

| 项目 | 说明 |
|------|------|
| **暂定名** | cue shell |
| **CLI 入口** | `cue`（推测） |
| **守护进程** | `cued`（daemon） |
| **定位** | **异步进程运行时 + TUI**，面向人机（human-agent）协作场景。**不是**终端复用器（明确区别于 tmux/zellij），核心价值在于将"命令执行"从同步阻塞模型升级为原生异步、可编排、可持久化的运行时。 |

---

## 二、架构总览

### 三层架构

| 层级 | 名称 | 职责 |
|------|------|------|
| **L1** | Execution Backend | 底层进程执行、pty 分配、I/O 捕获 |
| **L2** | Session Runtime（核心） | Scope / Job 生命周期管理、环境快照、调度、链式编排 |
| **L3** | Frontend | TUI / MCP / API 等多前端接入 |

### cued 守护进程

- 通过 **Unix socket** 提供服务，实现跨 screen/terminal 的共享状态。
- 使用 **SQLite** 存储历史记录与持久化数据。
- **State Store** 采用双层策略：
  - **热数据**：内存中维护。
  - **冷数据**：在关键事件节点（key events）checkpoint 到磁盘。

---

## 三、核心原语

### Job

| 字段 | 说明 |
|------|------|
| `JobId` | 唯一标识（如 `J<id>`） |
| `status` | 生命周期状态（pending → running → done/failed/killed） |
| `stdout` / `stderr` | **环形缓冲区（ring buffer）**，固定容量，避免无限内存增长 |

关键能力：
- `:fg J<id>`：为指定 Job 分配 pty，支持交互式程序（vim、htop 等）。
- Ctrl-C 行为：在**空输入行**按下时弹出 **Job 选择器 popup**，而非直接发送 SIGINT。

### Scope

> v2 设计中统一使用 "Scope" 替代原先的 "Session" 概念。

**数据结构**：
- 共享可变环境（shared mutable env）
- 顺序 Job 队列（sequential job queue）
- **完整环境快照**：env 变量 + cwd + umask + aliases + functions + shell options + traps
- 环境增量捕获（env delta capture）：每个 Job 执行后记录环境变化

**生命周期状态**：

```
Active → Idle (队列空) → Persisted (TTL 到期，落盘)
                                    ↓
                              Archived (LRU 淘汰 or :scope close)
                                    ↓
                              Deleted (超出保留限制)
```

| 状态 | 说明 |
|------|------|
| **Active** | 有活跃 Job 或用户正在交互 |
| **Idle** | 无活跃 Job，等待输入 |
| **Persisted** | 超过 TTL 后自动持久化到磁盘 |
| **Archived** | 长期归档，可按需恢复（auto-resume） |
| **Deleted** | 超出保留限制后删除 |

**Scope 树**：支持 fork/branch 产生子 Scope，可选 merge 合并环境。

**Scope 持久化策略**：

- **Delta 存储**：fork 出的 Scope 只存储 `parent_id` + `env_delta`，读取时沿 parent 链合并得到完整 env，大幅减少磁盘开销。
- **Scope 继承**（Executor 间数据共享）：`:spawn --inherit-scope S1` 允许 Executor B 继承 A 的 Scope（默认只读），A 完成后 Scope 冻结，B 读取 A 写入的 env 数据。Planner 只做调度，不搬运大数据。
- **LRU 淘汰 + 引用保护**：配置 `max_persisted_scopes`，超限时淘汰 `last_used` 最旧的 Scope。被 active/idle 子 Scope 依赖的 parent 不可淘汰。淘汰前先把依赖它的子 Scope 的 delta 展平为全量快照。

**Scope 类型**：讨论了 direct（人类直接操作）与 agentic（Agent 驱动）两种类型，最终结论是**仅作为 origin tag 标记来源**，不做类型级别的区分。

---

## 四、语法设计

### 核心语法：`:` 前缀

**所有内建命令以 `:` 开头**，后接命令名和参数。

```
:run cargo build          # 发射 job
:kill J1                  # 终止 job
:jobs                     # 列出所有 job
:scope list --tree        # 列出 scope
```

**设计理由**（详见 [research/syntax-decisions.md](./research/syntax-decisions.md)）：
- `:` 在所有模式下零冲突：shell 命令、自然语言、cron 表达式都不以 `:` 开头
- Vim/Helix/lazygit 用户有肌肉记忆（TUI 文化一致）
- Agent 友好：`:` 不是任何编程语言的转义字符
- 解析规则极简：**首字符 `:` → 内建命令，否则 → 模式默认包装**

### 三模式系统

> v2 设计去掉了 CMD 模式，保留三个模式。`:` 前缀在任何模式下都能直接执行内建命令，因此不再需要独立的 CMD 模式。

| 模式 | 默认包装 | 含义 | 对应核心原语 |
|------|---------|------|-------------|
| **JOB** ⚡ | → `:run <input>` | 输入即执行 | Job |
| **AGENT** 🤖 | → `:ask <input>` | 输入即对话 | Agent |
| **CRON** ⏰ | → `:cron <input>` | 输入即调度 | Cron |

- Shift+Tab 循环切换：JOB → AGENT → CRON → JOB
- 解析规则：`:` 开头 = 内建命令，否则 = 模式默认包装。零歧义。

> 详细的命令列表、解析规则、模式转换示例见 [commands-and-modes.md](./commands-and-modes.md)。

### 链式操作符

```
:run cargo fmt ~> cargo build -> cargo test || cargo clippy
```

| 操作符 | 语义 | exit code 分隔符 |
|--------|------|----------------|
| `->` | 串行，前者成功才继续 | `;` |
| `~>` | 串行，忽略前者结果继续 | `;` |
| `\|\|` | 并行，同时发射 | `,` |
| `\|\|?` | 并行，任一成功即可 | `,` |

**退出码聚合**示例：`[0; 0; 0, 1]`
- **分号 `;`**：分隔串行步骤的退出码
- **逗号 `,`**：分隔并行步骤的退出码

---

## 五、Agent 架构

### 双层模型：Planner + Executor

| 角色 | 数量 | 权限 | 职责 |
|------|------|------|------|
| **Planner** | 单实例（全局） | **只读** + `:spawn` + `:confirm` | 全局规划、任务分解、调度 Executor |
| **Executor** | 多实例 | **完整命令工具集** | 执行具体任务，拥有文件系统、进程管理等全部能力 |

### 设计原则

- **Planner 是事件驱动的决策器**，不轮询，不阻塞
- **Planner 看全景 + 可窥探细节**（通过 `:probe`，有输出上限 4KB）
- **Planner 不产生副作用**（`:spawn` 和 `:confirm` 除外）
- **写操作全部通过 Executor**

### Planner 唤醒事件

```
user_input:     用户的新请求或追问
executor_done:  Executor 完成（带结构化摘要）
executor_error: Executor 异常退出
escalate:       Executor 上报需要决策
cron_trigger:   定时任务触发
```

### Agent 运行模式

| 模式 | 说明 |
|------|------|
| **Stateless** | 无状态，每次调用独立 |
| **Stateful** | 有状态，跨调用保持上下文 |

### Executor 结构化上报

Executor 完成时向 Planner 上报结构化摘要（而非让 Planner 自己读 stdout）：

```json
{
  "status": "failed",
  "category": "test_failure",
  "failed_tests": ["test_auth_login", "test_db_connect"],
  "error_summary": "2/47 tests failed, both in auth module",
  "suggestion": "auth module regression"
}
```

> 详细的 Planner/Executor 权限边界、可用命令列表、`:probe`/`:confirm`/`:escalate` 约束见 [commands-and-modes.md](./commands-and-modes.md)。

---

## 六、配置与扩展性

### config.toml 结构

| 配置区块 | 内容 |
|----------|------|
| **Scope Profiles** | 预定义 Scope 模板（含 init 脚本、环境变量、默认选项等） |
| **Command Rules** | 命令规则：`deny`（禁止）/ `warn`（警告）/ `suggest`（建议替代） |
| **Hooks** | 生命周期钩子（见下） |
| **Extensions** | 扩展机制配置 |

### Hook 系统

| 作用域 | Hook 点 |
|--------|---------|
| **Job** | `job.before` / `job.done` / `job.fail` / `job.kill` |
| **Scope** | `scope.create` / `scope.idle` / `scope.persist` / `scope.resume` / `scope.close` |

### Init Job 机制

Scope Profile 可配置 init 脚本，Scope 创建时自动执行，并通过 **env delta capture** 捕获初始化产生的环境变化。

### 扩展机制

采用 **WASM**（WebAssembly）作为扩展运行时，实现安全沙箱化的插件机制。

---

## 七、关键设计决策

### 1. Scope 不可变性

Job 和 queue 一旦 dispatch 即**不可变（immutable）**。如需在已有 Scope 基础上继续工作，使用 **fork** 创建"延续（continuation）"。这保证了历史可追溯性和并发安全性。

### 2. Bash 兼容性："Island" 方法

借鉴 **Astro Islands** 思想：
- **简单命令**：原生执行（cue shell 内置解析）
- **复杂语法**（管道、heredoc、复杂展开等）：**委托给 bash** 执行

目标是在不重新实现完整 bash 语法的前提下，覆盖绝大多数日常使用场景。

### 3. Foreground PTY 分配

`:fg J<id>` 将指定 Job 提升为前台，分配完整 pty，从而支持 vim、htop 等需要终端控制的交互式程序。

### 4. 与 tmux/zellij 的边界

明确声明 **cue shell 不是终端复用器**。tmux/zellij 管理的是"窗口/面板"，cue shell 管理的是"异步 Job 和 Scope"。两者可以共存（cue shell 可以跑在 tmux 的一个 pane 里）。

### 5. 结构化输出与精确错误

UX 层面强调：
- **结构化输出**：非纯文本，支持机器可解析的输出格式
- **精确错误信息**：明确指出错误原因和位置
- **进度追踪**：长时间运行的 Job 提供进度反馈

### 6. `:` 前缀替代 `cmd:` 分隔符（v1→v2）

v1 设计使用 `cmd: args` 分隔符语法（如 `kill: J1`），v2 改为 `:cmd args` 前缀语法（如 `:kill J1`）。改变理由：
- `:` 前缀与 TUI 文化（Vim/Helix）更一致
- 解析规则更简单（首字符判断 vs 扫描冒号位置）
- 消除了 `echo "foo:bar"` 类的误判风险

详细决策过程见 [research/syntax-decisions.md](./research/syntax-decisions.md)。

### 7. 三模式替代四模式（v1→v2）

v1 设计有 4 个模式（CMD/JOB/AGENT/SCHED），v2 去掉 CMD 模式，改为 3 个（JOB/AGENT/CRON）。理由：
- `:` 前缀在任何模式下都能触发内建命令，CMD 模式不再必要
- SCHED 重命名为 CRON，与 `:cron` 命令一致

### 8. TUI 布局

```
┌─────────────────────────────────┐
│         Main View               │
│   (Job 输出 / 交互内容)         │
├──────────────┬──────────────────┤
│ Scope Panel  │   Job Panel      │
│ (Scope 列表) │   (Job 列表)     │
├──────────────┴──────────────────┤
│         Input Line              │
├─────────────────────────────────┤
│         Status Bar              │
└─────────────────────────────────┘
```

---

## 八、开放问题

| # | 问题 | 当前状态 |
|---|------|----------|
| 1 | **项目最终命名** | 仍为暂定名 "cue shell"，未最终确定 |
| 2 | **WASM 扩展的具体 API 边界** | 提及了 WASM 机制，但具体的宿主 API、权限模型、扩展生命周期未详细定义 |
| 3 | **Agent stateless vs stateful 的切换策略** | 提到两种模式，但何时使用哪种、状态如何持久化、跨 Scope 是否共享等细节未明确 |
| 4 | **Scope merge 的语义** | Scope 树支持 fork/branch 和"可选 merge"，但 merge 时环境冲突如何解决未定义 |
| 5 | **链式操作符的错误传播细节** | `~>` 忽略失败，但忽略到什么程度（仅退出码？还是包括 stderr？）未完全明确 |
| 6 | **退出码聚合在嵌套链中的表示** | 单层聚合已定义，深度嵌套（并行中嵌串行中嵌并行）的表示是否递归尚未讨论 |
| 7 | **cued 的多用户 / 权限模型** | Unix socket 通信已定，但多用户场景下的权限隔离未提及 |
| 8 | **Bash island 的边界判定** | "简单命令原生、复杂语法委托"的具体判定规则（哪些算简单？）未定义 |
| 9 | **Hook 的执行上下文与错误处理** | Hook 触发点已列出，但 Hook 失败是否阻断主流程、是否支持 async hook 未明确 |
| 10 | **Config 热重载** | config.toml 结构已定义，但修改后是否支持热重载（不重启 cued）未讨论 |
| 11 | **Planner 的调度策略** | Planner 负责全局规划，但具体的调度算法、优先级、资源限制等未涉及 |
| 12 | **CRON 任务的持久化** | cron 任务在 cued 重启后是否自动恢复、任务定义存储位置未明确（Scope 持久化策略已定义了通用框架） |

---

*本文档综合自设计讨论总结（v1）和命令与模式最终方案（v2），反映截至 v2 设计完成时的状态。标记为"开放问题"的条目需要后续讨论确定。*
