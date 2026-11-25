# Web search

Alyosha searches the web through [open-webSearch](https://github.com/Aas-ee/open-webSearch), an MCP server that fetches ordinary search engines' result pages and returns the results as text. It needs no account or API key. Like the browser, this needs no Alyosha code: one package in the container and one MCP server entry. [MCP Client](MCP.md) · [Browser](Browser.md) · [What Alyosha still needs](todo.md) · [Architecture overview](../README.md)

```text
containers.alyosha
└─ alyosha ── MCP ──► open-websearch ──► Bing, DuckDuckGo, Brave, Startpage, Baidu
                  └─► playwright-mcp ──► the pages the model chooses to open
```

## How the model uses it

The model calls `search` with a query and gets back titles, links, and short descriptions. To read a result, it opens the link with the Chromium tools. The browser skill tells it to search first when it does not already know the page. [The web-browsing skill](Browser.md#the-web-browsing-skill)

Only the `search` tool is enabled. open-webSearch can also read pages itself, more cheaply than Chromium, but for now the browser stays the one way to read a page. Enabling the page reader later is a configuration change.

## Why open-webSearch

It is the simplest option: one server that Alyosha starts and stops like the browser, with no service to keep running and no secret to store. Paid search APIs such as Tavily, Brave, and Exa need an account and an API key and see every query. A self-hosted SearXNG needs its own always-on service.

Searching through a site in Chromium, the earlier fallback, is slow, costs many tokens per search, and often meets bot checks in a headless browser.

## Limits

open-webSearch reads the engines' result pages, as a person's browser would. Engines change those pages without notice, which can break parsing until the project updates, and an engine may block the container's IP if it sees too many searches. Light personal use rarely triggers that. When one engine fails, the model can name another in its next call.

The search engines see each query and the container's IP, but no account.

## When to switch to SearXNG

Switch to a self-hosted [SearXNG](https://github.com/searxng/searxng) if searches often come back empty or blocked, or if open-webSearch stops being updated. SearXNG queries many engines at once and merges the results, so one blocked engine does not stop a search, and a large community keeps its engine parsers current. The switch changes only the `search` server entry and adds a SearXNG service. The model still sees one search tool, so the browser skill, scheduled tasks, and email need no change.

## Trust

Search results are untrusted text, like web pages. The server opens no network port: it runs in standard input and output mode only. Its other tools, which fetch arbitrary URLs, are turned off. [Browser trust](Browser.md#trust)

## Deferred

- Enabling open-webSearch's page reader as a cheaper alternative to Chromium for plain articles.
- Switching to SearXNG, if the conditions above are met.

[Web search setup guide](../develop/WebSearch.md)
