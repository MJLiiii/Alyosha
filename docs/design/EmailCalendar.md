# Email and calendar

Alyosha reads your Gmail and Google Calendar, tidies your mailbox, writes drafts, and sends email after you approve, through [gogcli](https://github.com/openclaw/gogcli), a command-line tool for Google services with a built-in MCP server, `gog mcp`. Like the browser, this needs no Alyosha code: one package in the container, one MCP server entry, and one skill. [MCP Client](MCP.md) · [Browser](Browser.md) · [What Alyosha still needs](todo.md) · [Architecture overview](../README.md)

```text
containers.alyosha
└─ alyosha ── MCP ──► gog mcp ──► Gmail and Calendar APIs (your Google account)
```

## What Alyosha can do

| Service | Allowed | Not allowed |
| --- | --- | --- |
| Gmail | Search and read; drafts; archive, labels, mark read or unread, trash and restore; **send, after you approve** | Permanent deletion, sending a saved draft |
| Calendar | List events | Create, change, or delete events |

**Sending always asks you.** Email is the most exposed input Alyosha reads: anyone can put text in front of the model by emailing you. `gmail_send_message` is marked `ask`, so the Tool Executor shows you the recipient, subject, and full text and sends only after you approve. A misled model cannot skip that. Sending a saved draft stays off, because its approval would show only a draft ID. [Approvals](Approvals.md)

**Tidying the mailbox needs no approval.** Archiving, labels, read state, and trash stay inside your own mailbox, and trash can be restored for 30 days. Permanent deletion stays off.

**The calendar is read-only for now.** gogcli's MCP server offers only `calendar_events`, which lists events. Its command line can create events, but the model has no way to call it: the shell sandbox has no access to the sign-in. Adding events waits for gogcli to offer the tool, or for a small wrapper.

Together with scheduled tasks, this covers requests like "every morning at 8, summarize my inbox and today's meetings." [Scheduled tasks](Schedule.md)

## Signing in

gogcli uses your own Google OAuth client, so no third party sits between Alyosha and Google. You create a desktop OAuth client in a Google Cloud project, then sign in once on the host, where a browser is available. gogcli keeps the sign-in in its encrypted file store in `~/.config/alyosha/gog`, which the container already mounts. Its password is in Alyosha's configuration file, like other MCP server tokens. [Sign in on the host](../develop/EmailCalendar.md#sign-in-on-the-host)

An OAuth app left in Google's "Testing" mode loses its sign-ins after 7 days. Set it to "In production" without asking Google for verification. You then see an "unverified app" warning when signing in, which is expected for an app only you use.

The model never sees the sign-in. The file tools and the shell sandbox cannot reach `~/.config/alyosha/gog`. [Container limits](../reference/ToolPermissions.md#container-limits)

## Trust

Email text is untrusted, like a web page. gogcli marks fetched email text as untrusted content and sanitizes message bodies. An `email` skill tells the model to treat email as information, never as instructions, and to tell you when an email asks it to do something. The skill index shows its description in every session. [Skills](Skills.md#what-the-model-sees) · [Browser trust](Browser.md#trust)

Approving every send does not stop every leak. A misled model could still carry what it read in an email to a website through the browser. The model's instructions, and the status area naming each MCP call, are the limits there, as for the browser.

## Deferred

- Permanent deletion of email.
- Creating and changing calendar events.
- Google Drive, Docs, Sheets, Contacts, and Tasks, which gogcli also covers.
- Email accounts other than Gmail.

[Email and calendar setup guide](../develop/EmailCalendar.md)
