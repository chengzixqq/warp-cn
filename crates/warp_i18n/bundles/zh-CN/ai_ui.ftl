# AI / Agent UI 包裹层字符串（非模型回复文本）。
# Keys MUST start with `ai-ui-`.

ai-ui-thinking = 思考中...
ai-ui-generating = 生成中...
ai-ui-cancelled = 已取消
ai-ui-error-occurred = 发生错误
ai-ui-retry = 重试
ai-ui-accept = 接受
ai-ui-reject = 拒绝

# 建议气泡的 Tooltip
ai-ui-tooltip-add-rule = 添加规则：{ $content }
ai-ui-tooltip-suggested-prompt =
    建议的提示词：
    { $prompt }

# Todos 弹层
ai-ui-tasks-header = 任务

# Agent 类型选择
ai-ui-choose-agent = 选择智能体
ai-ui-suggested-badge = 建议

# 云端 Agent 配置向导
ai-ui-cloud-setup-title = 开始使用 Oz 云智能体
ai-ui-cloud-setup-intro = 直接在 Warp 中通过集成（Linear、Slack）、事件（GitHub、内置计划任务），或通过 Oz SDK / CLI 以编程方式启动 Oz 云智能体。
ai-ui-cloud-setup-manual-section = 手动配置：使用 Oz CLI 创建 Slack 或 Linear 集成
ai-ui-cloud-setup-create-env = 创建环境
ai-ui-cloud-setup-env-first = 首先配置一个环境，以便创建集成。
ai-ui-cloud-setup-custom-image = 或者，提供你自己已有的 Docker 镜像。
ai-ui-cloud-setup-create-integration = 创建集成

# Agent 输入区底栏
ai-ui-using-default-model = 当前使用全终端智能体的默认模型。
ai-ui-enable-terminal-command-autodetection = 启用终端命令自动识别
ai-ui-disable-terminal-command-autodetection = 禁用终端命令自动识别
ai-ui-voice-input = 语音输入
ai-ui-attach-file = 附加文件
ai-ui-auto-approve-on = 关闭所有智能体动作的自动批准
ai-ui-auto-approve-off = 为此任务自动批准所有智能体动作
ai-ui-file-explorer = 文件浏览器
ai-ui-open-file-explorer = 打开文件浏览器
ai-ui-rich-input = 富文本输入
ai-ui-open-rich-input = 打开富文本输入
ai-ui-hide-rich-input = 隐藏富文本输入
ai-ui-open-coding-agent-settings = 打开编码智能体设置
ai-ui-enable-notifications = 启用通知
ai-ui-enable-agent-notifications = 启用 { $agent } 通知
ai-ui-install-plugin-tooltip = 安装 Warp 插件以在 Warp 内启用丰富的智能体通知
ai-ui-notifications-setup-instructions = 通知设置说明
ai-ui-plugin-install-instructions-tooltip = 查看安装 Warp 插件的说明
ai-ui-update-warp-plugin = 更新 Warp 插件
ai-ui-update-plugin-tooltip = 有新的 Warp 插件版本可用
ai-ui-plugin-update-instructions = 插件更新说明
ai-ui-plugin-update-instructions-tooltip = 查看更新 Warp 插件的说明
ai-ui-dismiss = 关闭
ai-ui-start-remote-control = 启动远程控制
ai-ui-start-remote-control-login-required = 登录后才能使用 /remote-control
ai-ui-stop-sharing = 停止分享
ai-ui-context-window-usage = 上下文窗口用量
ai-ui-choose-environment = 选择环境
ai-ui-show-version-history = 显示版本历史
ai-ui-update-agent = 更新智能体
ai-ui-update-agent-tooltip = 此计划包含智能体尚不知道的更改。按 { $shortcut } 可停止智能体当前任务并发送更新后的计划
ai-ui-restore = 恢复
ai-ui-plan-save-sync-tooltip = 保存并自动同步此计划到你的 Warp 云盘
ai-ui-show-in-warp-drive = 在 Warp 云盘中显示
ai-ui-save-markdown-file = 保存为 Markdown 文件
ai-ui-attach-active-session = 附加到当前会话
ai-ui-copy-plan-id = 复制计划 ID
ai-ui-attach-context = 附加上下文
ai-ui-slash-commands = 斜杠菜单
ai-ui-at-context-no-objects = 当前上下文中没有可用对象。
ai-ui-at-context-ssh = SSH 会话中不支持
ai-ui-at-context-subshell = 子 Shell 中不支持
ai-ui-at-context-filesystem-required = 需要文件系统
ai-ui-at-context-disabled-terminal-mode = 已在终端模式中禁用，可在设置中重新启用
ai-ui-choose-execution-profile = 选择 AI 执行配置
ai-ui-choose-agent-model = 选择智能体模型
ai-ui-manage = 管理
ai-ui-manage-api-keys = 管理 API 密钥
ai-ui-profiles = 配置文件
ai-ui-manage-profiles = 管理配置文件
ai-ui-open-github = 在 GitHub 中打开
ai-ui-open-code-review = 在代码审查中打开
ai-ui-manage-rules = 管理规则
ai-ui-review-changes = 审查更改
ai-ui-open-all-code-review = 全部在代码审查中打开
ai-ui-dont-show-again = 不再显示
ai-ui-rewind = 回退
ai-ui-rewind-before-block = 回退到此区块之前

# 零态区块
ai-ui-zero-isolated-cloud = 在隔离的云环境中运行你的智能体任务。
ai-ui-zero-recent-activity = 近期活动
ai-ui-view-changelog = 查看更新日志

# 通用 AI 徽标与状态
ai-ui-recommended = 推荐
ai-ui-queued = 排队中
ai-ui-check-now-suffix = { " · " }立即检查
ai-ui-invalid-api-key = 提供的 API 密钥无效
ai-ui-debug-output = 调试输出

# AWS Bedrock 凭据
ai-ui-aws-creds-error = AWS 凭据已过期或缺失
ai-ui-always-auto-run = 始终自动运行

# 代码 Diff 视图
ai-ui-file-renamed-no-change = 文件仅重命名，内容未改

# 命令请求
ai-ui-permission-always-ask = 你的执行档已设置为每次执行命令前都需获得授权。

# 会话详情面板
ai-ui-conversation-error = 错误
ai-ui-conversation-status = 状态
ai-ui-conversation-harness = 框架
ai-ui-conversation-artifacts = 制品
ai-ui-conversation-env-setup = 环境配置命令
ai-ui-conversation-env-details = 环境详情
ai-ui-conversation-credits-used = 已用额度

# 执行档编辑器
ai-ui-profile-name = 名称
ai-ui-plan-auto-sync = 计划自动同步
ai-ui-plan-auto-sync-desc = 该智能体创建的计划将自动添加并同步至 Warp 云盘。
ai-ui-call-web-tools = 调用网页工具
ai-ui-call-web-tools-desc = 该智能体在完成任务有需要时，可使用网页搜索。
ai-ui-context-window-label = 上下文窗口
ai-ui-context-window-desc = 基础模型的工作记忆 —— 一次能考虑多少 token 的对话、代码与文档。窗口越大，对话越长、跨更大代码库的回复越连贯，但代价是延迟与算力消耗更高。

# 云端（ambient）智能体 UI
ai-ui-ambient-failed = 失败
ai-ui-ambient-start-cloud-agent = 启动新的 Oz 云智能体
ai-ui-ambient-cloud-env-intro = 云智能体需要一个运行环境来完成任务。请先创建第一个环境。之后你可以编辑环境，或在需要时添加新环境。
ai-ui-ambient-free-credits = 免费额度
ai-ui-ambient-failed-start-env = 启动环境失败
ai-ui-ambient-github-auth-required = 需要 GitHub 授权
ai-ui-ambient-github-auth-msg = 请使用 GitHub 授权以继续
ai-ui-ambient-cancelled-title = 云智能体运行已取消
ai-ui-ambient-no-cloud-env = 未启动云环境

# 会话列表 / Codex 弹窗
ai-ui-no-conversations = 暂无会话
ai-ui-conversation-list-search = 搜索
ai-ui-conversation-list-view-all = 查看全部
ai-ui-conversation-list-show-less = 收起
ai-ui-conversation-list-empty-desc = 你与本地和云端智能体的活动会话及历史会话会显示在这里。
ai-ui-conversation-list-new-conversation = 新建会话
ai-ui-conversation-list-active-section = 活动
ai-ui-conversation-list-past-section = 历史
ai-ui-conversation-list-delete = 删除
ai-ui-conversation-list-cannot-delete-ambient = 云端智能体会话无法删除
ai-ui-conversation-list-share = 共享会话
ai-ui-conversation-list-fork-new-pane = 在新窗格中 fork
ai-ui-conversation-list-fork-new-tab = 在新标签页中 fork
ai-ui-codex-new = 新建

# 智能体管理
ai-ui-agent-owner-all = 全部
ai-ui-agent-owner-all-tooltip = 查看你的智能体任务和团队共享任务
ai-ui-agent-owner-personal = 个人
ai-ui-agent-owner-personal-tooltip = 查看你创建的智能体任务
ai-ui-agent-get-started = 开始使用
ai-ui-agent-view-agents = 查看智能体
ai-ui-agent-clear-filters = 清除筛选
ai-ui-agent-clear-all = 全部清除
ai-ui-agent-search = 搜索
ai-ui-agent-new-agent = 新建智能体
ai-ui-agent-filter-header = { $prefix }：{ $value }
ai-ui-agent-filter-all = 全部
ai-ui-agent-filter-none = 无
ai-ui-agent-filter-status = 状态
ai-ui-agent-filter-working = 运行中
ai-ui-agent-filter-done = 已完成
ai-ui-agent-filter-failed = 失败
ai-ui-agent-filter-source = 来源
ai-ui-agent-filter-created-on = 创建时间
ai-ui-agent-filter-last-24-hours = 最近 24 小时
ai-ui-agent-filter-past-3-days = 过去 3 天
ai-ui-agent-filter-last-week = 最近一周
ai-ui-agent-filter-has-artifact = 包含制品
ai-ui-agent-filter-pull-request = Pull Request
ai-ui-agent-filter-plan = 计划
ai-ui-agent-filter-screenshot = 截图
ai-ui-agent-filter-file = 文件
ai-ui-agent-filter-harness = 执行框架
ai-ui-agent-filter-environment = 环境
ai-ui-agent-filter-created-by = 创建者
ai-ui-agent-source-linear = Linear
ai-ui-agent-source-api = API
ai-ui-agent-source-slack = Slack
ai-ui-agent-source-cli = CLI
ai-ui-agent-source-scheduled = 定时任务
ai-ui-agent-source-warp-app = Warp 应用
ai-ui-agent-source-oz-web = Oz 网页端
ai-ui-agent-source-github-action = GitHub Action
ai-ui-agent-copied-branch-name = 已复制分支名
ai-ui-agent-session-expired = 会话已过期
ai-ui-agent-session-expired-tooltip = 会话会在一周后过期，过期后无法打开。
ai-ui-agent-no-session-available = 无可用会话
ai-ui-agent-unknown-creator = 未知
ai-ui-agent-metadata-source = 来源：{ $source }
ai-ui-agent-metadata-harness = 执行框架：{ $harness }
ai-ui-agent-metadata-run-time = 运行时长：{ $run_time }
ai-ui-agent-metadata-credits-used = 已用额度：{ $usage }
ai-ui-agent-runs = 运行记录
ai-ui-agent-loading-cloud-runs = 正在加载云端智能体运行记录
ai-ui-agent-loading-agents = 正在加载智能体...
ai-ui-agent-no-filter-results = 没有符合筛选条件的结果

# 智能体通知
ai-ui-notifications-close = 关闭
ai-ui-notifications-mark-all-read = 全部标为已读
ai-ui-notifications-title = 通知
ai-ui-notifications-filter-all-tabs = 全部标签页
ai-ui-notifications-filter-unread = 未读
ai-ui-notifications-filter-errors = 错误
ai-ui-notifications-filter-with-count = { $label }（{ $count }）
ai-ui-notifications-empty = 暂无通知
