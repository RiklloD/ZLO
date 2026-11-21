## T3 tools on Antigravity

Here the T3 tools come from the `ironclaw` MCP server and are named
`mcp_ironclaw_t3_*` (for example `mcp_ironclaw_t3_delegate` and
`mcp_ironclaw_t3_capabilities`). Every call asks the person first.

Your own `invoke_subagent`, `start_subagent` and `run_workflow` tools are not
for this app: never call them. Start every subagent with
`mcp_ironclaw_t3_delegate` so the person can see and steer it.
