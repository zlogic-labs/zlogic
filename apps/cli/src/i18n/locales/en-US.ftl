sessions-title = Session History
sessions-search-placeholder = / Search sessions / title / model
sessions-search-active = /{ $query }_
sessions-search-query = /{ $query }
sessions-count =
    { $count ->
        [one] { $count } session
       *[other] { $count } sessions
    }
sessions-hint-search = Type then Enter to search · ↑/↓ select · Enter again to resume · Esc exit search
sessions-hint-history = ← Back to sessions · ↑/↓ select history · Enter fork/rewind · Esc back
sessions-hint-list = ↑/↓ select · → history · / search · Enter resume · r rename · a archive · d delete · m multi-select · Esc close
sessions-hint-selection = ↑/↓ select session · Space mark · d delete marked · Esc exit multi-select
sessions-empty = No matching sessions
sessions-history-title = History
sessions-history-picker-title = History  ·  Pick a fork / rewind point
sessions-history-picker-hint = ↑/↓ move · Enter choose fork / rewind
sessions-history-user = user
sessions-history-assistant = assistant
sessions-history-turn = Turn #{ $n }
sessions-delete-title = Delete { $count } session(s)?
sessions-delete-message = This permanently removes { $count } selected session(s).
common-delete = Delete
common-cancel = Cancel
sessions-hint-rename = Type new title · Enter save · Esc cancel
sessions-rename-active = Rename: { $title }_
sessions-status-renamed = Renamed session to "{ $title }"
sessions-status-archived = Archived "{ $title }"
sessions-status-deleted = Deleted { $count } session(s)
sessions-status-deleted-switched = Deleted { $count } session(s) · switched to { $id }
sessions-status-forked = Forked and switched to { $id }
sessions-status-rewind = Created rewind from history { $id }
sessions-switch-title = Switch session?
sessions-switch-message = Resume "{ $title }"?
sessions-switch-confirm = Switch
sessions-history-action-title = Choose history action
sessions-history-action-message = Use turn { $turn } as the starting point.
sessions-history-action-fork = Fork
sessions-history-action-rewind = Rewind
workspace-pick-title = The current directory is not inside any registered workspace
workspace-pick-create = Create a new workspace from the current directory
workspace-pick-select = Choose from the workspace list
workspace-pick-hint = ↑/↓ select · Enter confirm · Esc cancel
workspace-pick-cancelled = Cancelled (no workspace chosen)
workspace-pick-empty = (no usable workspaces — the directory may have moved)
workspace-pick-list-title = Choose a workspace
workspace-pick-list-hint = ↑/↓ select · Enter open · Esc back
workspace-title = Workspaces
workspace-count =
    { $count ->
        [one] { $count } workspace
       *[other] { $count } workspaces
    }
workspace-empty = No workspaces yet · start zlogic from another directory to create one with the current directory
workspace-hint-left = ↑/↓ select workspace · Enter switch · → sessions · Esc close
workspace-hint-right = ← workspaces · ↑/↓ select session · Enter resume · Esc close
workspace-sessions-title = Sessions
workspace-sessions-empty = This workspace has no sessions yet
workspace-new-session = New session
workspace-new-session-title = Start a new session?
workspace-new-session-message = Start a new session in "{ $name }"?
workspace-current = current
workspace-switch-title = Switch workspace?
workspace-switch-message = Switch to "{ $name }" and resume its most recent session?
workspace-switch-confirm = Switch
workspace-session-switch-title = Resume session?
workspace-session-switch-message = In "{ $workspace }", resume "{ $title }"?
workspace-status-switched = Switched to workspace "{ $name }"
workspace-status-session-switched = Resumed session "{ $title }" in "{ $workspace }"
workspace-status-current = This is already the current workspace
workspace-status-switch-failed = Failed to switch workspace
command-workspace-desc = List and switch workspaces
command-help-desc = Show help
help-title = Help
help-subtitle = Commands, navigation, and composer controls
help-shortcuts = Essentials
help-no-commands = No commands available
help-key-send = Send prompt
help-key-newline = Insert newline
help-key-file = Attach file or directory
help-key-command = Open command completion
help-key-cancel = Cancel active response
help-key-quit = Quit CLI
help-hint = ↑/↓ browse · Enter run · Esc close
help-group-commands = Commands
help-group-skills = Skills
command-model-desc = Switch / manage models
command-key-desc = Manage provider API keys
command-theme-desc = Switch theme
command-session-desc = Open session history
command-new-desc = New session
command-stats-desc = Usage statistics
command-compact-desc = Summarise older context now
command-info-desc = Current session information
session-info-title = Session information
session-info-workspace = WORKSPACE
session-info-model = MODEL
session-info-runtime = RUNTIME
session-info-usage = USAGE
session-info-hint-model = switch model
session-info-hint-plan = toggle Plan mode
session-info-hint-approval = change approval mode
overlay-mouse-select-apple = Fn+drag select text
overlay-mouse-select-iterm = Option+drag select text
overlay-mouse-select-vscode-mac = Option+drag select text
overlay-mouse-select-vscode-other = Alt+drag select text
overlay-mouse-select-windows = Shift+drag select text
overlay-mouse-select-xterm = Shift+drag select text
command-plan-desc = Toggle Plan mode
command-approval-desc = Switch approval mode
command-lang-desc = Switch UI language
command-view-desc = Switch live view detail (minimal/normal/verbose)
command-kind-builtin = cmd
command-kind-skill = skill
command-kind-option = option
command-overlay-title = Commands
command-empty = No matching commands
command-hint = Type to filter · Enter/Tab select · Esc close
command-plan-on = Enable Plan mode
command-plan-off = Use Normal mode
command-plan-toggle = Toggle Plan mode
command-approval-auto = Ask when an operation needs approval
command-approval-deny = Deny operations requiring approval
command-approval-all = Allow all operations without prompts
command-lang-zh-cn = Switch UI language to Simplified Chinese
command-lang-en-us = Switch UI language to English
command-view-minimal = Thinking + spinner only, no tool rows
command-view-normal = Full thinking process + tool summaries
command-view-verbose = Thinking + tool parameters and results
status-plan-usage = Usage: /plan on|off|toggle
status-plan-on = Plan mode enabled
status-plan-off = Normal mode enabled
status-theme-usage = Usage: /theme <name>
status-theme-updated = Theme changed to { $theme }
status-new-session = Started a new session
status-new-failed = Failed to create a new session
status-lang-usage = Usage: /lang zh-CN|en-US
status-lang-updated = Language switched to { $locale }
status-lang-failed = Failed to switch language: { $locale }
status-approval-usage = Usage: /approval auto|deny|all
status-approval-mode = Approval mode: { $mode }
status-view-usage = View mode is { $mode } · Usage: /view minimal|normal|verbose
status-view-set = View mode changed to { $mode }
view-mode-minimal = minimal
view-mode-normal = normal
view-mode-verbose = verbose
stats-title = stats
stats-today-cost = Today ${ $cost }
stats-range-cost = Range ${ $cost }
stats-total-cost = Total ${ $cost }
stats-cache-summary = Cache { $hit }% · saved ${ $savings }
stats-recent-trend = Recent trend
stats-by-model = By model
stats-daily-usage = Daily usage
stats-range-label = Range
stats-range-month = month
stats-range-hint = ←/→ range · ↑/↓ scroll · PgUp/PgDn page · Esc close
stats-cache = Cache
stats-cache-read = read
stats-cache-write = write
stats-context = Context
models-title = models
models-subtitle = ↑/↓ select · Enter switch · Esc close
models-list-title = Model list
models-detail-title = Model details
models-empty-models = No models under this provider
models-empty-catalog = No models yet. Add providers / models in the config file, then press r to reload
models-filter-empty = No models match "{ $query }"
models-key-present = present
models-key-missing = missing
models-key-env = env
models-current = current
models-key-missing-hint = k bind key · Enter works too
models-provider-count = { $count } models under this provider
models-detail-select-model = ↑/↓ pick a model on the left to see its details
models-field-sdk = sdk
models-field-base-url = base url
models-field-provider = provider
models-field-tier = tier
models-field-context = context
models-field-vision = vision
models-field-price = price
models-field-key = key
models-field-storage = storage
models-form-set-key = Set provider key
models-config-note = Edit models — change the config file
models-scroll-above = ↑ { $count } more
models-scroll-below = ↓ { $count } more
models-form-hint = Enter next/save · Tab field · Esc cancel
models-hint = Enter switch · k bind key · t test · [ ] scroll · r reload · Esc close
models-status-key-saved = Key saved
models-status-reloaded = Reloaded models.yaml
models-error-key-save = Could not save the key (check the keychain / credential store)
models-prompt-key-title = Bind API key
models-prompt-key-message = { $model }'s provider ({ $provider }) has no API key yet. Bind one to switch.
models-prompt-key-bind = Bind key
models-test-title = Test model
models-test-running = Testing model
models-test-hint = Esc close · t retest
form-option-allow-once = Allow once
form-option-allow-session = Allow for session
form-option-deny = Deny
form-option-confirm = Confirm
form-option-cancel = Cancel
form-hint-permission = ↑/↓ choose · Enter submit · Esc deny
form-hint-confirm = ↑/↓ choose · Enter submit · Esc cancel
form-hint-submit-skip = Enter submit · Esc×2 skip
form-enter-submit = Enter submit
form-enter-next = Enter next
form-hint-text = Type text · ←/→ cursor · ↑/↓ field · { $enter } · Esc×2 skip
form-hint-checkbox = Space toggle · ↑/↓ field · { $enter } · Esc×2 skip
form-hint-select = ↑/↓ choose · { $enter } · Esc×2 skip
form-hint-multiselect = ↑/↓ move · Space check · { $enter } · Esc×2 skip
form-hint-default = { $enter } · Esc×2 skip
form-empty = Empty
form-toggle-on = ● On
form-toggle-off = ○ Off
form-none-selected = None selected
form-selected-count = { $labels } ({ $count } selected)
form-more-above =         … { $count } more above
form-more-below =         … { $count } more below
form-fields-above = ↑ { $count } more fields above
form-fields-below = ↓ { $count } more fields below
form-required-mark = *
form-warn-required = ⚠ this field is recommended
form-warn-number = ⚠ expected a number
form-warn-integer = ⚠ expected an integer
live-thinking = Thinking…
live-tool-running = { $spinner } running
live-generating = { $spinner } generating reply…
file-picker-hint = Type to filter · Enter open/select · Tab select · Esc close
file-picker-title = Files
exit-resume-label = Resume conversation
exit-resume-command-fallback = zlogic --resume <sessionId>
status-esc-cancel-again = Press Esc again to cancel the current response
status-quit-confirm = Press Ctrl+C again to quit
status-canceling = Canceling current response…
status-interaction-allowed-once = Allowed once
status-interaction-allowed-session = Allowed for this session
status-interaction-denied = Denied
status-interaction-confirmed = Confirmed
status-interaction-confirm-canceled = Confirmation canceled
status-interaction-input-sent = Input sent
status-interaction-input-skipped = Input skipped
status-form-skip-again = Press Esc again to skip the form
status-file-parent-enter = Press Enter to enter the parent directory
status-pasted-direct = Pasted { $chars } chars
status-paste-save-failed = Failed to save paste: { $error }
paste-display-saved = { $glyph } pasted { $lines } lines / { $chars } chars · saved as file
paste-display-inline = { $glyph } pasted { $lines } lines / { $chars } chars
status-large-paste-file = Large paste will be sent as a file reference
status-paste-full-render = Paste content will render fully after send
paste-default-label = paste content
permission-shortcuts = a allow · d deny · s session
mailbox-consumed = mailbox consumed
queue-title = queue
queue-current = inject current
queue-next = next question
queue-hint = Send target: { $target } · Ctrl+T switch · Ctrl+Z undo
status-send-target-mailbox = Send target: inject current turn
status-send-target-next = Send target: next question
status-mailbox-queued = Queued for current-turn injection
status-mailbox-rejected = Not queued: the engine rejected this message (see the error notification)
status-next-turn-queued = Queued as the next question
status-next-turn-started = Started the next queued question
status-compacting = Compacting the oldest context…
status-compact-rejected = Could not start compaction (see the error notification)
status-turn-not-started = Message queued: the session is busy (another process is running), so no turn could start right now
status-queue-undone = Removed the latest queued item
status-queue-empty = The selected queue is empty
status-queue-undo-draft = Clear the current draft before restoring a queued item
notification-permission-title = Permission request: { $action }
notification-confirm-title = Confirmation required
notification-input-title = Input required
notification-form-message = Please fill out the form
status-canceled = Current response canceled
notification-api-error-title = API error
notification-compacting = Compacting the oldest context…
compaction-marker = compaction · turns { $from }–{ $to }
compaction-more = ⋯⋯ { $count } more lines
round-summary = Thinking { $think } chars · tools { $tools } { $status }
round-think = Think
round-steps =
    { $count ->
        [one] { $count } step
       *[other] { $count } steps
    }

# ── /replay history view (history-model-refactor plan A) ──
replay-title = History · { $count } turns
replay-title-detail = Turn { $turn } · { $rounds } rounds
replay-no-answer = (no text reply)
replay-widget-count =
    { $count ->
        [one] { $count } card
       *[other] { $count } cards
    }
replay-loading = Loading history…
replay-empty = No history in this session
replay-failed = Could not load this turn's detail
replay-hint-list = ↑/↓ select · Enter open · Esc close
replay-hint-detail = ↑/↓ select round · Enter expand · Esc back
command-replay-desc = Review a past turn's rounds (thinking / tools)
toolbar-notifications =
    { $count ->
        [one] { $glyph } { $count } notification
       *[other] { $glyph } { $count } notifications
    }

# ── zoom viewer (fullscreen text viewer, Ctrl+O / sessions history `v`) ──
zoom-title-last-reply = Last reply
zoom-title-history = Turn { $turn } · { $role }
zoom-footer = { $percent }% · ↑/↓ scroll · PgUp/PgDn page · g/G top/bottom · Esc close
zoom-footer-copy = { $percent }% · c copy · ↑/↓ scroll · PgUp/PgDn page · g/G top/bottom · Esc close
zoom-footer-info = { $percent }% · ↑/↓ scroll · PgUp/PgDn page · Esc close
zoom-copy-success = Copied { $chars } characters
zoom-copy-failed = Copy failed: system clipboard command is unavailable
status-zoom-empty = No reply to zoom yet
status-zoom-hint = Ctrl+O zoom last reply
sessions-history-zoom-hint = v view full text

# ── sending a message · no usable model ──
error-no-usable-model =
    No usable model, so this message was not sent. Add a model / set a key and send again:
    · built-in providers appear once a key is set — nothing to configure; your own providers/models go in { $models_file };
    · set a key with: zlogic key set <provider> <value>

# ── key subcommand (zlogic key …) ──
key-usage-set = Usage: zlogic key set <provider> <value>
key-usage-delete = Usage: zlogic key delete <provider>
key-usage-list = Usage: zlogic key list (no arguments)
key-unknown-command = Unknown key subcommand: { $command }
key-status-set = Wrote key for { $provider } ({ $hint })
key-status-deleted = Deleted key for { $provider }
key-list-title = Provider keys:
key-list-empty = No provider keys configured yet
key-pick-notty = Not an interactive terminal (stdin/stdout is not a tty): pass the provider and value explicitly, e.g. zlogic key set <provider> <value>
key-cancelled = Cancelled
key-empty = Empty input; no key written
key-pick-title = Choose a provider:
key-pick-hint = ↑/↓ move · Enter confirm · Esc cancel


# ── startup (EngineSession::connect, before eprintln) ──
startup-dirs-failed = Failed to derive the data directory (logs go to stderr only): { $error }
startup-runtime-failed = Failed to start the tokio runtime: { $error }
startup-engine-failed = Failed to assemble the engine: { $error }
startup-workspace-failed = Failed to open the workspace: { $error }
startup-workspace-list-failed = Failed to list workspaces: { $error }
startup-workspace-get-failed = Failed to read the selected workspace: { $error }
startup-workspace-id-invalid = Invalid workspace id "{ $raw }": { $error }
startup-workspace-cancelled = Cancelled (no workspace chosen)
startup-deviation-note = Workspace { $root } · tools run in { $dir }
startup-session-id-invalid = Invalid session id "{ $raw }": { $error }
startup-latest-failed = Failed to look up the latest session: { $error }
startup-no-session = Nothing to resume: this workspace has no sessions yet (start once without --resume first)
startup-open-session-failed = Failed to open the session: { $error }

# ── session actions (submit / control / model switch) ──
submit-unsupported-input = This model lacks { $caps }; switch to one of: { $models }
submit-empty = Empty message
control-failed = Control command failed: { $error }
model-switch-failed = Failed to switch the model: { $error }
env-passthrough-request = skill { $skill } requests to pass through environment variables: { $vars }
form-max-length = At most { $n } characters
form-free-text-hint = { $hint } (free text allowed)
task-update-fallback = Background task notification

# ── engine wire messages (LocalizedMessage.key → Fluent id: dots/underscores become dashes) ──
error-submit-no-usable-model = This message was not sent: no usable model ({ $error }). Configure a model and set an API key, then try again.
error-submit-queued-no-model = No model is currently available, so this message is queued. Configure a model and an API key in settings ({ $error })
error-submit-queued-turn-failed = This message was queued, but a reply could not be started yet: { $error }
error-submit-rejected = Submission rejected: { $reason }
notice-mcp-tools-pending = Still fetching the tool manifest for { $servers }; they are not included this round (they will be next round)
notice-mcp-server-no-tools = MCP server `{ $server }` connected but did not provide any tools
notice-mcp-tools-expensive = MCP tool definitions total { $how_much }, and you pay that cost **every round**. The biggest consumers: { $biggest }. To tighten: restrict which tools ship in the definition (`tools`: ["only these"]), or turn off servers you do not need right now with `/mcp off <id>`.
notice-llm-interrupted-retry = The reply was interrupted mid-generation and is being continued automatically (retried { $n } times so far). What you already saw stays; the model picks up from where it stopped.
notice-agent-model-unavailable = Sub-agent `{ $name }` could not resolve a model ({ $error }); it will not run in this turn
notice-session-title-failed = The session title could not be refined: { $error }
notice-turn-start-failed = A queued message could not start a reply: { $error }. Configure a model and an API key, then try again.
notice-mcp-server-untrusted =
    { $count } MCP servers from this project will not start until you confirm them:
    { $lines }
    Confirm one with `/mcp trust <id>`; list all with `/mcp`.
notice-tasks-still-running = Background tasks started by this turn are still running; you will be notified when they finish:
    { $tasks }

error-submit-internal-part = Task updates, skill loads and skill invocations can only be produced by the runtime
error-submit-empty = The submission is empty
error-submit-unknown-model = Unknown model: { $model }
