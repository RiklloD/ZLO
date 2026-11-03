## Subagents (T3 orchestrator)

In this app every subagent is a T3 orchestrator thread: the person sees it,
can open it and steer it, and its result comes back here. Start subagents
only with the T3 tools. Never use a built-in subagent, agent, task or workflow
tool of your own harness (they are switched off), and never shell out to
another agent CLI.

The tools: `t3_capabilities`, `t3_delegate`, `t3_task`, `t3_thread`,
`t3_queue`, `t3_schedule`. On Claude Code they are `mcp__ironclaw__t3_*`;
load their schemas with ToolSearch. Call `t3_capabilities` first and use the
exact provider, model and profile names it returns.

There are two kinds, both made with `t3_delegate`:

1. **Subagent on this task** (ask another model, a reviewer, a parallel
   slice of the same work): `context: "inherit"`. The child gets this
   conversation so far, keeps this thread's profile, and folds under this
   thread. Pick its engine with `target.provider` / `target.model`.
2. **Side thread** (a task less tied to this conversation, possibly for
   another profile): `context: "brief"` (the default), optionally
   `target.profile`. The child sees only `task`, so make it self-contained:
   every fact, path, constraint, and the output you expect back.

Either way:
- `mode: "async"` (default) returns a `taskId` at once. The result arrives in
  this thread as a new turn when the child finishes: end your turn, do not
  poll or sleep. `mode: "wait"` blocks up to `timeout_s`.
- `t3_task` does `status`, `list` and `cancel` (cancel stops the child and
  everything it started).
- To talk to a running or finished child, `t3_thread` `action: "send"` with
  `thread` = its `childThreadId`. For another review round, call
  `t3_delegate` again with the full brief, the prior findings and your
  objections, and a new `client_request_id`.
- Check `status`, `provider`, `model` and `profile` in the result before you
  rely on which engine or profile ran.
