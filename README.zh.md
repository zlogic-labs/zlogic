# zlogic（@zlogic.run）

> 本地优先的 AI 工程助手 —— CLI、桌面、手机三端共用同一套 agent runtime。
> 无官方云端、无遥测：请求直连你配置的模型服务商，密钥只存在系统钥匙串里，绝不落盘进配置文件。

**[English](README.md) · [简体中文](README.zh.md)**

[![License](https://img.shields.io/badge/license-Apache--2.0-green)](LICENSE)
[![Version](https://img.shields.io/badge/version-v1.0.0--beta.1-blue)](https://zlogic.run)
[![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-lightgrey)](https://zlogic.run/#download)
[![Website](https://img.shields.io/badge/website-zlogic.run-purple)](https://zlogic.run)

---

## 桌面端预览

![zlogic 桌面端（浅色主题）](screenshots/desktop.light.svg)

![zlogic 桌面端（深色主题）](screenshots/desktop.dark.svg)

---

## zlogic 是什么

一句话：**本地优先的 AI 工程助手**。

把它装在你的电脑上之后，它变成一个常驻的本地 agent runtime：消息组装、上下文构建、工具执行、文件读写、会话持久化全部在本地完成，请求只发往你自己配置的模型服务商。你可以在终端里用 TUI 操作，也可以在跨平台桌面端中可视化地管理项目、会话和配置。

CLI（Rust TUI）、桌面端（Tauri 2）、移动端（规划中）共用同一内核 —— 会话、历史、审批换端不换。三端不是三个产品，是**同一套本地 runtime 的三块皮肤**。

| 端 | 形态 | 适合谁 |
|---|---|---|
| **CLI（TUI）** | 终端里原生全屏界面，Rust 编写 | 已经在终端里写代码、查日志、跑构建的人 |
| **桌面端** | Tauri 2 原生应用（Windows / macOS / Linux） | 想可视化管多个项目、看用量和成本的人 |
| **移动端（规划中）** | Flutter App，通过 daemon 配对连接你电脑上的 runtime | 想手机远程审批、随时看会话/任务的人 |

## 核心特性

### 隐私是默认设计

- 密钥只存**系统钥匙串**（macOS Keychain / Windows 凭据管理器 / Linux Secret Service）或环境变量，**绝不落盘进配置文件**。
- 模型思考过程（reasoning）只作为独立通道展示，**永不回灌**到下一轮上下文。
- 请求直连你声明的端点：用 OpenAI 就是 OpenAI，用 DeepSeek 就是 DeepSeek，不存在中间代理层。
- 无遥测：不收集会话、代码、prompt、key、用量数据；无埋点、无崩溃上报、无 cookie 跟踪。

### 多模型支持，国内模型开箱即用

内置 provider 目录，探测到 key 即自动出现，不用写配置文件：

- **完整测试**：DeepSeek、OpenAI
- **beta 可用**：Anthropic、Gemini、智谱 GLM、DashScope（通义千问）、Qwen（`qwen_local`）、OpenRouter、Fireworks、Bedrock、任意 OpenAI 兼容端点（企业网关、自建 vLLM / SGLang）
- **本地模型**：Ollama / vLLM / SGLang 等 OpenAI 兼容端点直接配置

DeepSeek、智谱、通义千问使用各自的官方 SDK，不是走通用 OpenAI 兼容层。

### 审批体系：模型可以建议，但决定权在你

每次敏感操作（写文件、跑命令、访问外部资源）之前都过「权限门」：

> **Layer 0** 确定性规则 → **Layer 1** 轻量 LLM 审查 → **Layer 2** 更强模型复审（需配置双模型层级）→ **用户确认兜底**

- 授权范围支持 `once` / `session` / `project`，批准一次这个项目就不反复弹窗；已有授权可用 `/grants` 管理。
- 内置敏感路径清单：`.env`、SSH 私钥、`~/.aws`、`~/.kube`、`.npmrc` 等一律强制询问，并可用 `policy.yaml` 自定义。
- 危险操作（不可逆删除、提权、发版、数据外泄、持久化后门）默认永远问人。
- 自动审批（Layer 0–2）无法保证 100% 准确 —— **用户确认是安全模型里最后一环，也是最重要的一环**。
- God Mode 只建议本地开发环境使用（桌面端设置或 CLI `/god` 切换）。

### 工程能力

- **本地历史（checkpoints）**：影子对象库快照，turn 内文件变更可查、可回滚、可 diff 撤销，**绝不触碰你的 git**；`node_modules` 等缓存目录无条件排除。
- **记忆（Memory）**：持久化跨会话的耐久事实，分 `global` / `workspace` 作用域，项目指令永远优先于记忆。
- **扩展市场（MCP）**：插件与技能基于 MCP，安装时逐个声明能力、逐个批准，随时禁用/卸载。
- **连接（Connections）**：数据库、对象存储、云账号一次配置、按 workspace 授权，凭据不进工作区配置。
- **用量统计**：调用次数、tokens、费用，按日期 / provider / 模型 / workspace / 会话多维筛选。

## 快速开始

### 一键安装 CLI

```sh
# bash / zsh
curl -fsSL https://install.zlogic.run | sh

# PowerShell
irm https://install.zlogic.run | iex
```

安装脚本自动检测操作系统与架构，把二进制装到 `/usr/local/bin/zlogic`（Linux / macOS）或 `%LOCALAPPDATA%\zlogic\bin`（Windows）。

### 桌面端

从 [zlogic.run/#download](https://zlogic.run/#download) 下载对应平台的安装包：

| 平台 | 格式 | 下载 |
|---|---|---|
| Windows 10/11 x64 | NSIS 安装包 (.exe) | [下载](https://zlogic.run/dl/zlogic_latest_x64-setup.exe) |
| macOS 12+ Apple Silicon（M 系列） | DMG | [下载](https://zlogic.run/dl/zlogic_latest_aarch64.dmg) |
| macOS 12+ Intel（x86_64） | DMG | [下载](https://zlogic.run/dl/zlogic_latest_x86_64.dmg) |
| Linux x86_64 | AppImage | [下载](https://zlogic.run/dl/zlogic_latest_amd64.AppImage) |

系统依赖：Windows 需要 WebView2 运行时（Windows 10+ 自带）；Linux 需要 WebKitGTK 4.1 + libsoup3。macOS 应用尚未签名/公证，首次打开会被 Gatekeeper 拦截，在终端运行 `xattr -dr com.apple.quarantine /Applications/zlogic.app` 一次性信任即可。

**校验下载（可选但推荐）**：每个安装包都附带 SHA-256 校验和与 minisign 签名，官方机器可读清单在 <https://update.zlogic.run/checksums.json>（公钥指纹 key id：`75AA378991532581`）。

### 配置模型

key 不进配置文件 —— 存进系统钥匙串或环境变量（环境变量默认开启，导出即识别）：

```sh
export DEEPSEEK_API_KEY=sk-...
# 或写入系统钥匙串
zlogic key add deepseek
```

常用密钥命令：`zlogic key list`、`zlogic key remove <provider>`、`zlogic key verify <provider> <model>`。也可以显式声明 provider：

```yaml
# ~/.zlogic/models.yaml
providers:
  my-provider:
    sdk: openai_chat
    base_url: https://api.openai.com/v1
    models:
      gpt-4o: {}
```

### 进入 TUI

```sh
zlogic    # 进入 TUI，Enter 新建会话，直接说人话
```

其他命令：`zlogic config`（配置参考）、`zlogic mcp`（插件与技能）、`zlogic memory`（跨会话记忆）、`zlogic daemon`（移动端配对）。

> 💡 Windows 用户请使用 **Windows Terminal / WezTerm / Alacritty / VS Code 终端**，旧版 cmd 不支持 DEC 2026，IME 组合也不稳定。

## 数据目录

数据按四个根目录划分（所有平台统一 XDG 风格），其中 **`data` 与 `state` 是本地数据的唯二存放处，删除不可恢复**，只有 `cache` 可以放心清空：

| 根 | 存放 |
|---|---|
| `~/.config/zlogic/` | 手写配置：`config.yaml`、`models.yaml`、`env.yaml`、`policy.yaml` |
| `~/.local/share/zlogic/` | 对象库、附件、每个 workspace 的历史、已安装的运行时与技能 |
| `~/.local/state/zlogic/` | `state.db`（会话 / entries / 用量）、锁、日志 |
| `~/.cache/zlogic/` | 可随时清空的缓存 |

项目级：项目根 `.zlogic/policy.yaml`（项目安全策略，只会收紧全局策略，不会放宽）、`.mcp.json`（工作区 MCP server 声明）。完整配置参考见[文档](https://zlogic.run/docs/)。

## 现状与 Roadmap

- **v1.0.0-beta.1（Beta）已发布**：CLI 与桌面端均可下载使用。
- **daemon**：`zlogic daemon` + `--pair` 二维码一次性配对（5 分钟有效），无账号、无云端中转，已在 CLI 中可用。
- **移动端 App**：规划中 —— 手机聊天、语音通话、远程审批，连接运行在你机器上的 agent runtime。
- **关于开源**：目前是专有软件，源码暂不公开；第三方开源组件保留各自许可证。有想法欢迎提 [Issue](https://github.com/zlogic-labs/zlogic/issues)。

## 文档与链接

- 官网（下载、文档、隐私说明）：**https://zlogic.run**
- 文档：**https://zlogic.run/docs/**（安装 / 快速上手 / CLI 与桌面端选择 / Provider 与模型 / 密钥管理 / 配置参考 / 数据目录 / 安全与审批 / 数据流与隐私 / FAQ / Roadmap）
- 安装站：**https://install.zlogic.run**
- 更新站：**https://update.zlogic.run**（Tauri v2 自动更新；`checksums.json` 校验清单）
- Issue / 反馈：**https://github.com/zlogic-labs/zlogic/issues**

## License

[Apache-2.0](LICENSE)
