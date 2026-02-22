# Per-role environment variable overrides

## Problem

Ralph applies one `[env]` configuration to all agent roles. When
`isolate_env` injects variables like `CARGO_TARGET_DIR`, every agent
(implementer, tester, reviewer) receives the same value. This caused a
production failure where testers inherited `CARGO_TARGET_DIR`, which
leaked into integration tests that spawn their own cargo subprocesses.

The immediate fix (computing `isolate_env` from the agent's actual
working_dir inside `invoke_agent()`) addresses the common case. But
there will be situations where an operator knows something Ralph
doesn't — e.g. "my test harness breaks when `FOO` is set" or "reviewers
need `LINT_STRICT=1`". Today there's no way to express this.

## Possible design

Add `[env.roles.<name>]` sections that layer on top of the global
`[env]`:

```toml
[env]
passthrough = ["MY_TOKEN"]

[env.set]
FOO = "bar"

[env.roles.tester]
remove = ["CARGO_TARGET_DIR"]

[env.roles.tester.set]
TEST_TIMEOUT = "120"

[env.roles.reviewer]
remove = ["CARGO_TARGET_DIR"]
passthrough = ["LINT_CONFIG"]
```

Resolution order: global passthrough/set -> isolate_env from working_dir
-> role set (overrides) -> role remove (strips) -> role passthrough
(additional forwarding).

Rust types:

```rust
pub struct EnvConfig {
    pub passthrough: Vec<String>,
    pub set: HashMap<String, String>,
    pub roles: HashMap<String, RoleEnvOverride>,  // new
}

pub struct RoleEnvOverride {
    pub set: HashMap<String, String>,
    pub remove: Vec<String>,
    pub passthrough: Vec<String>,
}
```

## Questions to resolve

**Is the complexity warranted?** The working_dir fix handles the
`isolate_env` case without any config changes. Per-role overrides add a
new concept to the config surface. How often would operators actually
need to differentiate env vars by role? If the answer is "rarely, and
only for exotic setups," the escape hatch might not justify the config
complexity.

**Could a simpler mechanism suffice?** For example, a global `env.remove`
list (not per-role) that strips variables from all agents. That's a
one-field addition to `EnvConfig` and covers the "my tool is confused by
this env var" case without role-specific layering. You lose the ability
to set something only for implementers, but that might be acceptable.

**Interaction with isolate_env.** If `env.roles.tester.remove` strips
`CARGO_TARGET_DIR`, but `isolate_env` re-injects it (since it runs
inside `invoke_agent()`), which wins? The resolution order above says
`remove` runs after `isolate_env`, so the operator's intent wins. But
this needs to be clearly documented and tested, since the interaction
is non-obvious.

**TOML ergonomics.** Nested tables like `[env.roles.tester.set]` are
verbose. Is there a flatter shape that reads better? The `[models]`
precedent is a flat map of role -> string, which is simpler than what's
proposed here.
