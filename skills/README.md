# Skill templates

Alyosha reads skills only from `~/.agents/skills/` on the host, the user-level folder of the Agent Skills convention, which you own and edit. These are the skills the design calls for. Copy the ones you use:

```bash
cp -r skills/web-browsing skills/email ~/.agents/skills/
```

- `web-browsing`: rules for the `search` and `chromium` MCP servers. [Browser](../docs/Browser.md#the-skill) · [Web search](../docs/WebSearch.md#the-browser-skill)
- `email`: rules for the `google` MCP server. [Email and calendar](../docs/EmailCalendar.md#the-skill)
