# Cue Shell — TUI 设计决策

## 一、架构

| 决策 | 结果 |
|------|------|
| **架构模式** | TEA + Component 混合：全局 `AppState` + `Message` 枚举 + 纯函数 `update`，面板用独立 Widget trait 渲染 |
| **渲染框架** | ratatui 0.30 + crossterm 0.29 |

## 二、布局

### 普通模式

```
┌──────────┬──────────────────────────┐
│ Sidebar  │  Main REPL Area          │
│ (模式    │  (卡片式命令历史流)       │
│  相关    │                          │
│  列表)   │  ┌─ > cargo build ─────┐ │
│          │  │ J1                   │ │
│ ──────── │  └─────────────────────┘ │
│ 全局概览 │  ┌─ > :out J1 ────────┐ │
│ J:3 A:1  │  │ Compiling cue-core  │ │
│ C:2      │  │ Finished dev 0.3s   │ │
│          │  └─────────────────────┘ │
├──────────┴──────────────────────────┤
│  [JOB ⚡] > _                       │ ← Input Line
├─────────────────────────────────────┤
│ J:3(1🔄) A:1 C:2    cued:ok  14:30 │ ← Status Bar
└─────────────────────────────────────┘
```

### :fg 全屏模式

```
┌─────────────────────────────────────┐
│                                     │
│   (Job/Agent 的完整 pty 输出)       │
│                                     │
├─────────────────────────────────────┤
│  [FG J2] Ctrl+Z to detach          │
└─────────────────────────────────────┘
```

### 布局规则

| 规则 | 值 |
|------|---|
| 侧边栏显示 | 终端 ≥100 列时自动显示，<100 隐藏 |
| 侧边栏切换 | Ctrl+B 手动覆盖 |
| 侧边栏宽度 | 比例 20%~30%，min 20 列 max 40 列 |
| :fg 退出键 | Ctrl+Z |

## 三、侧边栏

### 内容跟随模式

| 模式 | 上半：主列表 | 下半：全局概览 |
|------|-------------|---------------|
| JOB ⚡ | Jobs 列表 | J:N A:N C:N |
| AGENT 🤖 | Agents 列表 | J:N A:N C:N |
| CRON ⏰ | Crons 列表 | J:N A:N C:N |

### 列表项格式（双行）

```
J1 ✅ cargo build
  0.3s exit:0
J2 🔄 cargo test
  running 12s
```

### 点击行为

- 点击列表项 → 选中高亮
- 选中后智能路由：
  - 需要 tty 的 Job/Agent → 自动 `:fg`
  - 不需要 tty 的 → 自动 `:out`

## 四、Main REPL Area

### 卡片式渲染

- 每条命令 + 响应 = 一个带边框的卡片（Warp Terminal 风格）
- 无前缀标记，**用颜色区分**：绿色=成功，红色=错误，默认=普通输出
- 只有最新卡片（且程序未结束）实时更新
- 历史卡片标记暂停/完成状态

### 长输出处理

- 默认显示部分（最后 N 行）
- 可展开查看全部（有上限）
- 卡片内鼠标滚轮滚动
- 长时间不活跃的旧卡片自动折叠/隐藏内容

### 命令输出语义

- `:run cargo build` → 卡片内容为 `J1`（Job ID，绿色）
- `:out J1 --tail 5` → 卡片内容为 Job 的 stdout
- `:kill J2` → 卡片内容为 `killed J2`（绿色）
- 错误 → 红色文字

## 五、Input Line

| 特性 | 行为 |
|------|------|
| 默认 | 单行输入 |
| 换行 | Shift+Enter 或 Ctrl+Enter |
| 提交 | Enter |
| 历史 | ↑↓ zsh 风格（单行/首行 ↑ 翻历史，多行非首行 ↑ 移光标） |
| 补全 | Tab |
| 模式切换 | Shift+Tab 循环 JOB→AGENT→CRON |
| Prompt 格式 | `[JOB ⚡] > _`（不显示 scope） |

## 六、Status Bar

```
左对齐（可变）：J:3(1🔄) A:1 C:2
右对齐（常驻）：cued:connected  14:30
```

## 七、鼠标支持（第一版）

| 操作 | 支持 |
|------|------|
| 点击切换焦点区域 | ✅ |
| 滚轮滚动 | ✅（REPL区 + 卡片内 + 侧边栏） |
| 点击侧边栏条目选中 | ✅ |
| 拖拽调整面板 | ❌（后续） |
| 双击操作 | ❌（后续） |

## 八、Popup 系统

| 特性 | 设计 |
|------|------|
| 视觉形式 | 居中弹窗 + 半透明背景遮罩（lazygit 风格） |
| :help | ✅ overlay 弹窗 |
| 错误详情 | ✅ overlay 弹窗 |
| Ctrl+R | ✅ fuzzy finder 搜索历史命令 |
| Ctrl+C（空输入时） | ✅ Job 选择器 popup |

## 九、Agent 设计（TUI 层面）

| 决策 | 结果 |
|------|------|
| Agent 编号 | A1, A2, ...（与 Job J1, J2 平级） |
| Agent 与 Job 关系 | **独立原语**（有自己的状态机、生命周期、存储） |
| CLI Agent :fg | 完全移交 pty（和 Job :fg 一样） |
| API Agent :fg | 报错：`Agent A1 has no pty, use :out A1` |
| :confirm 交互 | 在 AGENT 模式 REPL 区作为对话消息显示，用户 input line 回复 |

## 十、Scope 模型（重新定义）

### 核心概念

- **Scope = 不可变的环境快照**（env + cwd + ...），内容寻址
- **ID = blake3(content) 的 hash**，显示为 `S0@a3f1`（标签+短hash）
- 相同环境 = 相同 hash = 自动去重

### Job 与 Scope 的关系

```
Job 持有两个引用：
  start_scope: ScopeHash  — 执行前的环境
  end_scope: ScopeHash    — 执行后的环境（可能与 start 相同）

规则：
  - start == end → Job 无副作用，合并
  - 只能从已完成的 scope fork（Job 未完成时 end_scope 不存在）
  - 默认 scope 通过 :env set / :cd 永久修改（持久化）
  - 未来：多个保存的默认 scope 可切换
```

### 类比

```
Scope ≈ git commit（不可变快照，内容寻址）
Job ≈ git diff（从一个快照到另一个快照的变换）
fork ≈ git branch（从某个快照开始新分支）
默认 scope ≈ HEAD 指针（可移动）
```
