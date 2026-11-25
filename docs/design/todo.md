# What Alyosha still needs

This page lists the capabilities Alyosha needs beyond the browser to work like a personal assistant. Each one gets its own design page once discussed. Designed items stay here, marked, while later items depend on them. [Architecture overview](../README.md#v1-decisions-and-open-work) · [Browser](Browser.md)

The biggest gap is that Alyosha works only while you sit at the TUI. A human assistant keeps working while you are away and can reach you. The order below follows that gap, and later items depend on earlier ones.

```text
1. Chat channel ──┬─► 2. Tasks that start on their own
                  └─► 5. Approval before risky actions
3. Email and calendar     (independent)
4. Web search             (independent)
```

## 1. A way to reach you: a chat channel

Designed. Messaging apps connect through bridge programs that drive `alyosha acp` over the Agent Client Protocol. Telegram and QQ each get a small Rust bridge of Alyosha's own that runs as a service in the container. QQ's official API limits how often a bot may message you first. [Chat channels](Channels.md)

Without a channel, Alyosha cannot ask you anything while you are away. The browser skill already depends on that: it tells the model to ask before buying, posting, sending, or deleting. [The web-browsing skill](Browser.md#the-web-browsing-skill)

Recommended first, because items 2 and 5 depend on it.

## 2. Tasks that start on their own

Designed. The model creates tasks through a small schedule MCP server, and each chat bridge runs its own app's tasks in its session and delivers the result. [Scheduled tasks](Schedule.md)

## 3. Email and calendar

Designed. gogcli's MCP server reads Gmail and Google Calendar, tidies the mailbox, and writes drafts. Sending waits for the owner's approval. [Email and calendar](EmailCalendar.md)

## 4. Web search

Designed. open-webSearch's MCP server searches without an account or key, with SearXNG as the planned fallback. [Web search](WebSearch.md)

## 5. Approval before risky actions

Designed. Alyosha acts freely in its own space and asks before reaching the real world: tools marked `ask` in the configuration wait for the owner's approval, and the model asks with `ask_owner` before actions only it can recognize. [Approvals](Approvals.md)

## Later

These are already deferred on their components' pages and are less urgent:

- Recall of past sessions. [Memory deferred work](Memory.md#deferred)
- Reading images and PDFs, which needs image support in tool results and a model that reads images.
- Downloads from the browser.
- Full desktop control. [Browser deferred work](Browser.md#deferred)

[Architecture overview](../README.md)
