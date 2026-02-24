# Multi-agent quickstart

Ralph can dispatch to different agent backends based on the model name
in your config. Each role (planner, implementer, tester, reviewer,
triager) can use a different model from a different vendor.

## Supported backends

| Backend  | Binary     | Models                                    | Auth                |
|----------|------------|-------------------------------------------|---------------------|
| Claude   | `claude`   | `opus`, `sonnet`, `haiku`, `claude-*`     | `ANTHROPIC_API_KEY` |
| Codex    | `codex`    | `o3`, `o3-pro`, `o3-mini`, `o4-mini`, `gpt-*`, `codex-*` | `OPENAI_API_KEY` |
| Gemini   | `gemini`   | `gemini-*`                                | OAuth or `GEMINI_API_KEY` |
| OpenCode | `opencode` | Any `provider/model` from `opencode models` | Managed by OpenCode |

The mapping is determined by model name. Exact aliases (`opus`,
`sonnet`, `o3`, etc.) and vendor prefixes (`claude-*`, `gpt-*`,
`gemini-*`) are recognized. Any model string containing a `/` routes
to OpenCode, using the exact `provider/model` format that
`opencode models` lists (e.g. `deepseek/deepseek-reasoner`,
`opencode/kimi-k2`, `opencode/glm-5`). Unrecognized model names
are a hard error.

## Install the backend CLIs

You only need to install backends you intend to use.

    # Claude (likely already installed)
    npm i -g @anthropic-ai/claude-code

    # Codex
    npm i -g @openai/codex

    # Gemini
    npm i -g @anthropic-ai/gemini-cli   # or: brew install gemini-cli

    # OpenCode (for DeepSeek and other providers)
    curl -fsSL https://opencode.ai/install | bash

## Configuration

Edit `.ralph/config.toml` in your project root. Set model names per
role and forward the required API keys.

### Example: Gemini for implementation, Claude for review

```toml
[models]
planner = "opus"
implementer = "gemini-2.5-pro"
tester = "gemini-2.5-flash"
reviewer = "opus"
triager = "opus"
```

Gemini CLI uses OAuth by default — run `gemini` interactively once
to authenticate. Alternatively, set `GEMINI_API_KEY` and add it to
`[env] passthrough`.

### Example: Kimi via OpenCode

```toml
[models]
planner = "opus"
implementer = "opencode/kimi-k2"
tester = "opencode/kimi-k2"
reviewer = "opus"
triager = "opus"
```

### Example: Codex for implementation

```toml
[models]
planner = "opus"
implementer = "o3"
tester = "o3"
reviewer = "opus"
triager = "opus"

[env]
passthrough = ["OPENAI_API_KEY"]
```

### Example: all different vendors

```toml
[models]
planner = "opus"
implementer = "opencode/kimi-k2"
tester = "gemini-2.5-flash"
reviewer = "o3"
triager = "sonnet"

[env]
passthrough = ["OPENAI_API_KEY"]
```

## Escalation across backends

The `escalation_model` setting works across backends. A failing
implementer will switch to the escalation model after
`escalation_after` attempts, even if that means switching backends:

```toml
escalation_after = 2
escalation_model = "opus"

[models]
implementer = "opencode/kimi-k2"
# ...
```

Here, the first two attempts use Kimi via OpenCode. If both fail,
attempt 3 escalates to Claude Opus.

## Pinning specific model versions

Use full model IDs to pin versions:

```toml
[models]
implementer = "claude-sonnet-4-6-20250217"
reviewer = "gemini-2.5-pro-preview-05-06"
```

The vendor prefix (`claude-*`, `gemini-*`, etc.) determines the backend.

## Troubleshooting

**"unrecognized model" error**: The model name doesn't match any known
alias or vendor prefix. Check spelling and see the table above.

**Backend CLI not found**: Ralph spawns the backend binary directly.
Make sure it's on your `PATH`. Run `which codex` (or `gemini`,
`opencode`) to verify.

**API key errors**: Non-Claude backends need their API keys forwarded
explicitly via `[env] passthrough`. Ralph clears the environment before
spawning agents for isolation, so keys must be listed here.
