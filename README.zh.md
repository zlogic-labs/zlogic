# zlogic（@zlogic.run）

> 面向开发者的 AI Agent Harness —— 构建并运行可直接操作文件、终端、Git 仓库、数据库与云服务的
> coding agent。Rust 编写，针对 DeepSeek 优化，支持多模型、本地与远程 agent、MCP、技能、
> 策略化执行控制，推理、执行与花费端到端可观测。

**[English](README.md) · [简体中文](README.zh.md)**

[![License](https://img.shields.io/badge/license-Apache--2.0-green)](LICENSE)
[![Version](https://img.shields.io/badge/version-v1.0.0--beta.1-blue)](https://zlogic.run)
[![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-lightgrey)](https://zlogic.run/#download)
[![Website](https://img.shields.io/badge/website-zlogic.run-purple)](https://zlogic.run)

---

## CLI 预览

![zlogic CLI（TUI）](screenshots/cli.gif)

桌面端见 [zlogic.run](https://zlogic.run)。

---

## zlogic 是什么

zlogic 是一套 AI agent harness：一个用于构建并运行 coding agent 的 runtime，外加驱动它的 CLI。
它不只是生成代码，而是让 agent 直接在你的开发环境里干活 —— 能碰什么、能跑什么，由你定义的策略说了算。

三端共用同一套本地 agent runtime：消息组装、上下文构建、工具执行、文件读写、会话持久化全部在
你机器上完成，请求只发往你自己配置的模型服务商。

| 端 | 形态 | 适合谁 |
|---|---|---|
| **CLI（TUI）** | 终端里的原生全屏界面，Rust 编写 —— **开源，即本仓库** | 已经在终端里写代码、查日志、跑构建的人 |
| **桌面端** | Tauri 2 应用（Windows / macOS / Linux）—— 闭源 | 想可视化管多个项目、看用量和成本的人 |
| **移动端**（规划中） | 通过 daemon 配对连接你机器上的 runtime —— 闭源 | 想手机远程审批、随时看会话/任务的人 |

## 能做什么

* **AI 编码 agent** —— 理解任务、探索代码库、规划改动、编辑文件，并把多步开发流程跑完。
* **开发者工作台** —— 项目、会话、文件、工具与 AI 工作流集中在一个工作区里，历史与审批跨端一致。
* **终端与 Shell** —— agent 在审批门之后替你执行命令与开发工具，会话还可以切换进自己的 Git worktree。
* **Git 集成** —— agent 在你的仓库里跑真正的 `git`（查看、diff、暂存、提交），你的工作区不会被偷偷重排。
* **数据库访问** *（桌面端）* —— 一次性配置 PostgreSQL / MySQL / SQLite / Redis，之后让 agent 基于它们处理开发数据。
* **AWS 与外部服务** *（桌面端）* —— 对象存储与云账号一次配置、按 workspace 逐个授权。
* **MCP 支持** —— 通过 Model Context Protocol 连接外部工具与服务，每项能力逐个声明、逐个批准。
* **技能与工具** —— 用可复用的 skill 和专用工具扩展 agent 的能力。
* **多模型** —— 同一个工作区里连接并切换多个 provider 与模型。
* **细粒度权限** —— 用策略精确控制 agent 能碰什么：文件、命令、工具与外部资源。
* **远程 Agent Daemon** *（二进制分发）* —— agent runtime 跑在另一台机器上，桌面端作为本地界面连过去，
  让 agent 贴着代码、GPU、数据库与服务运行。
* **本地历史与记忆** —— 会话、entries、用量都存在本地 SQLite 里；耐久记忆跨会话保留，分 `global` / `workspace` 作用域。
* **透明的用量** —— 调用次数、tokens、费用按会话记录，可按日期 / provider / 模型 / workspace 筛选。

把代码、工具、服务与 AI agent 放进同一个 runtime。

### 支持的 AI provider

内置 provider 目录随程序发布：探测到 key 就自动出现；没收录的 provider 也就是几行 `models.yaml`。

* DeepSeek
* OpenAI
* Anthropic Claude
* Google Gemini
* OpenAI 兼容 API —— 企业网关、自建 vLLM / SGLang / Ollama
* 智谱 GLM、DashScope（通义千问）、OpenRouter、xAI、Groq
* AWS Bedrock —— 走标准 `AWS_*` 环境变量
* 自定义 provider —— 端点与模型自己声明

DeepSeek、GLM、DashScope、Anthropic 各有专用客户端，思考/推理与用量口径单独处理，不是被塞进通用
OpenAI 兼容层。

## 透明的 agent 执行

zlogic 在任务进行时就把 agent 的推理与执行过程展示出来。

你能看到它在做什么、用了哪些工具、采取了哪些动作、任务推进到哪一步，而不是把它当成黑盒。在 TUI 里，
思考通道和每一次工具调用都实时流进 transcript；`--print json` 把同样的事件输出成 JSON-lines，
脚本或 CI 也能跟着跑。

## 基于策略的权限控制

zlogic 用基于策略的权限体系对 agent 能力做细粒度控制。

策略可以明确规定 agent 允许使用哪些资源与动作，包括文件访问、shell 命令、工具和外部服务，因此你可以
只给 agent 某个任务或某个工作区真正需要的那部分权限。

每次敏感操作都过权限门：先确定性规则，再由**安全分类模型**审查（它看到的是工具的路径、目标与命令，
永远看不到文件正文），最后是你 —— 审查跑在便宜档模型上，把哪个模型放进那一档就决定这道门有多严。
授权范围 `once` / `session` / `project`，批准过的项目就不再反复弹窗。内置敏感路径清单（`.env`、
SSH 私钥、`~/.aws`、`~/.kube`、`.npmrc` 等）一律强制询问。全局策略在 `<config dir>/policy.yaml`，
项目级在 `.zlogic/policy.yaml`，只能收紧、不能放宽。

CLI 里的相关开关：`--plan`（只读规划）、`--permission auto|deny|approve-all`、`--god` 全权访问 ——
最后两个只建议本地开发环境使用。

## 远程 agent 执行

daemon 模式下，agent runtime 跑在另一台机器上，zlogic 桌面端仍是你本地的界面：当你的代码、开发环境、
GPU、数据库或云工具在别处时，这很有用。客户端连上远程 agent，你就能监控会话、查看执行、补充输入、
控制 agent，而不用把整个开发环境搬过来。

这对本仓库意味着什么，见[开源范围](#开源范围)。

## 安全与本地数据

zlogic 在模型通信上采用客户端直连架构。

* **provider 直连** —— AI 请求从 zlogic 客户端直接发往你配置的 provider，zlogic 没有任何代理或中转你
  模型请求的服务器。
* **会话本地存储** —— 对话历史存在你设备本地的 SQLite 数据库里，可以自己查看、备份、删除，不存在
  zlogic 的服务器上。
* **凭据受保护** —— API key、数据库密码、云凭据存放在操作系统的凭据管理器（macOS Keychain、
  Windows 凭据管理器、Linux Secret Service）或由 state 目录里的主密钥加密的保险库中，也支持环境变量。
* **凭据不进模型上下文** —— 存储的凭据永远不会作为对话内容或上下文暴露给模型，思考通道也永不回灌到
  下一轮。

使用某个 AI provider 时，你的请求会直接发往该 provider，适用它自己的隐私政策与数据处理方式。

## 开源范围

本仓库是 **zlogic 的开源部分：CLI 与 agent runtime**，Apache-2.0，并且完整到可以自己构建、运行和修改：

* **CLI** —— `apps/cli`，Ratatui 终端前端；
* **agent runtime** —— `crates/` 下的每个 crate：round 循环、各 provider 客户端、工具注册表与内置工具、
  权限引擎、SQLite 存储、MCP、hooks，以及承载它们的 engine。

有三部分**闭源**，在另一个私有仓库里开发：

* **桌面端** —— Tauri + React 客户端，只以二进制分发；
* **daemon** —— 远程与移动端访问，以 `zlogic-daemon` 分发；
* **闭源工具集** —— 数据库驱动、数据分析、HTML widget，以及内置的 Python 运行时；上面标的
  *（桌面端）* 就是指这些。

它们都构建在本仓库发布的 crate 之上：你在这里读到的 runtime，就是那些客户端实际跑的 runtime，
runtime 与 CLI 的 bug 可以在本仓库修。从源码构建出来的 `zlogic` 是一个完整的 CLI；官方发布的二进制
额外把 daemon 焊了进去，这也是发布版里 `zlogic daemon …` 能用的原因。

## 快速开始

### 安装 CLI

```sh
# bash / zsh
curl -fsSL https://install.zlogic.run | sh

# PowerShell
irm https://install.zlogic.run | iex
```

安装脚本自动检测操作系统与架构，把二进制装到 `/usr/local/bin/zlogic`（Linux / macOS）或
`%LOCALAPPDATA%\zlogic\bin`（Windows）。

### 桌面端

从 [zlogic.run/#download](https://zlogic.run/#download) 下载 —— 只提供二进制，客户端闭源（见上文
[开源范围](#开源范围)）：

| 平台 | 格式 | 下载 |
|---|---|---|
| Windows 10/11 x64 | NSIS 安装包 (.exe) | [下载](https://zlogic.run/dl/zlogic_latest_x64-setup.exe) |
| macOS 12+ Apple Silicon（M 系列） | DMG | [下载](https://zlogic.run/dl/zlogic_latest_aarch64.dmg) |
| macOS 12+ Intel（x86_64） | DMG | [下载](https://zlogic.run/dl/zlogic_latest_x86_64.dmg) |
| Linux x86_64 | AppImage | [下载](https://zlogic.run/dl/zlogic_latest_amd64.AppImage) |

系统依赖：Windows 需要 WebView2 运行时（Windows 10+ 自带）；Linux 需要 WebKitGTK 4.1 + libsoup3。
macOS 应用尚未签名/公证，首次打开会被 Gatekeeper 拦截，在终端执行
`xattr -dr com.apple.quarantine /Applications/zlogic.app` 一次性信任即可。

每个安装包都附带 SHA-256 校验和与 minisign 签名，官方机器可读清单在
<https://update.zlogic.run/checksums.json>（公钥 key id：`75AA378991532581`）。

### 配置模型

key 不进配置文件 —— 存进系统钥匙串（或加密保险库），或者用环境变量：

```sh
export DEEPSEEK_API_KEY=sk-...   # 环境变量导出即识别
zlogic key set deepseek          # 或写入系统钥匙串
zlogic key list                  # 查看哪些 provider 已配 key
```

`zlogic key set`、`zlogic key delete <provider>`、`zlogic key list` 在没有配置模型时也能用 —— 那正是
TUI 起不来的场景。目录里没有的 provider 手写即可：

```yaml
# ~/.config/zlogic/models.yaml
providers:
  my-provider:
    sdk: openai_chat
    base_url: https://api.openai.com/v1
    models:
      gpt-4o: {}
```

### 跑起来

```sh
zlogic                                     # 进入 TUI：Enter 新建会话
zlogic --prompt "解释一下这个仓库"          # 无界面单轮执行
zlogic --prompt "hi" --print json           # 同一轮以事件 JSON-lines 输出
zlogic --session <id>                       # 继续某个会话；--resume 重开最近一次
```

常用参数：`--model`、`--theme`、`--locale`、`--cwd`、`--plan`、`--quiet`、
`--permission auto|deny|approve-all`。TUI 里打 `/` 打开命令面板：`/help`、`/model`、`/theme`、
`/session`、`/replay`、`/workspace`、`/new`、`/compact`、`/stats`、`/info`、`/plan`、`/approval`、
`/lang`、`/view`。完整参数说明见
[`apps/cli/docs/cli-arguments.md`](apps/cli/docs/cli-arguments.md)。

> 💡 Windows 用户请使用 **Windows Terminal / WezTerm / Alacritty / VS Code 终端**，旧版控制台不支持
> DEC 2026，IME 组合也不稳定。

### 从源码构建

```sh
cargo build --release -p zlogic-cli    # target/release/zlogic
cargo test --workspace                 # runtime 与 CLI 的测试
```

需要 Rust 1.85 或更新版本（workspace 使用 edition 2024）。首次构建会从 crates.io 拉取依赖树
（rustls、rusqlite、git2 等）。这里的一切都不需要桌面端、GPU 或任何网络服务。

## 仓库结构

| 路径 | 是什么 |
|---|---|
| `apps/cli` | Ratatui TUI，也就是 `zlogic` 二进制 |
| `crates/protocol` | 各边界的共享协议类型（core↔llm、UI→engine、engine→UI） |
| `crates/core` | 一次 agent 运行：构建上下文、驱动 round、执行工具、落库 |
| `crates/engine` | 进程内单例派发层：宿主 API、服务装配、事件扇出 |
| `crates/llm` | 每个厂商一个客户端：请求序列化、part 生命周期、用量归一 |
| `crates/tools` | 工具定义、注册表与内置工具 |
| `crates/policy` | 权限审查用的 shell 命令分解与路径分区 |
| `crates/store` | SQLite 持久层：会话 / entries / 用量 |
| `crates/objects` | 大载荷的内容寻址对象库 |
| `crates/credential` | 凭据引用、环境变量解析、加密保险库 |
| `crates/config` | 目录布局、YAML 配置、provider 探测、模型目录与价格 |
| `crates/mcp` | MCP server 定义、连接池与工具目录 |
| `crates/plugins` | 插件：目录加一份清单，贡献 MCP server |
| `crates/hooks` | 生命周期 hooks 及其命令执行器 |
| `crates/task` | 持久化的 job / run 领域模型 |
| `crates/code-sitter` | 带语法信息的 grep：每条命中标注所属符号 |
| `crates/logging`、`crates/paths` | tracing 装配，以及全 workspace 共用的路径处理 |

## 数据目录

数据按四个根目录划分（所有平台统一 XDG 风格）。**`data` 与 `state` 是本地数据的唯二存放处，删除不可
恢复**，只有 `cache` 可以放心清空：

| 根 | 存放 |
|---|---|
| `~/.config/zlogic/` | 手写配置：`config.yaml`、`models.yaml`、`env.yaml`、`policy.yaml` |
| `~/.local/share/zlogic/` | 对象库、附件、每个 workspace 的历史 |
| `~/.local/state/zlogic/` | `state.db`（会话 / entries / 用量）、锁、日志、凭据保险库 |
| `~/.cache/zlogic/` | 可随时清空的缓存 |

项目级：项目根 `.zlogic/policy.yaml`（只会收紧全局策略，不会放宽）、`.mcp.json`（工作区 MCP server
声明）。完整配置参考见 [zlogic.run/docs](https://zlogic.run/docs/)。

## 现状

* **v1.0.0-beta.1（Beta）** —— CLI 与桌面端均可下载使用。
* **开源** —— CLI 与 agent runtime 在本仓库以 Apache-2.0 开源；桌面端、daemon 与闭源工具集不开源。
  想法与问题欢迎提 [Issue](https://github.com/zlogic-labs/zlogic/issues)。
* **daemon** —— `zlogic daemon …` 把 engine 暴露为 HTTP JSON + SSE 服务，供远程桌面端与移动端使用，
  用配对代替账号，没有云端中转。
* **移动端 App** —— 规划中：手机聊天、语音、远程审批，连接你机器上的 runtime。

## 文档与链接

* 官网（下载、文档、隐私说明）：**https://zlogic.run**
* 文档：**https://zlogic.run/docs/**（安装 / 快速上手 / CLI 与桌面端选择 / Provider 与模型 / 密钥管理 /
  配置参考 / 数据目录 / 安全与审批 / 数据流与隐私 / FAQ / Roadmap）
* 安装站：**https://install.zlogic.run**
* 更新站：**https://update.zlogic.run**（Tauri v2 自动更新；公钥 key id `75AA378991532581`）
* Issue / 反馈：**https://github.com/zlogic-labs/zlogic/issues**

## License

[Apache-2.0](LICENSE)。第三方开源组件保留各自许可证。
