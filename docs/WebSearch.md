# Web search

Alyosha searches the web through [open-webSearch](https://github.com/Aas-ee/open-webSearch), an MCP server that fetches ordinary search engines' result pages and returns the results as text. It needs no account or API key. Like the browser, this needs no Alyosha code: one package in the container and one MCP server entry. [MCP Client](MCP.md) · [Browser](Browser.md) · [What Alyosha still needs](todo.md) · [Architecture overview](README.md)

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

## Implementation

Web search needs no Alyosha code. Package open-webSearch, add one MCP server, and include the search step in the browser skill. Do this after the browser works. [Browser setup](Browser.md#implementation) · [Architecture overview](README.md)

Facts below come from open-webSearch's README for v2.2.0, read in November 2026. [README](https://github.com/Aas-ee/open-webSearch) · [Releases](https://github.com/Aas-ee/open-webSearch/releases)

### Package

open-webSearch is not in nixpkgs. Build v2.2.0 from source in Alyosha's flake, next to `gproxy` and `gogcli`, and install it on the host and in the container. Use v2.1.7 or later, which fixed a server-side request forgery bug in its page reader. [Packages](EmailCalendar.md#package)

```nix
{ buildNpmPackage, fetchFromGitHub }:
buildNpmPackage rec {
  pname = "open-websearch";
  version = "2.2.0";
  src = fetchFromGitHub {
    owner = "Aas-ee";
    repo = "open-webSearch";
    rev = "v${version}";
    hash = "sha256-...";
  };
  npmDepsHash = "sha256-...";
}
```

The package's `build` script compiles TypeScript to `build/index.js`, and `bin` in `package.json` provides the `open-websearch` command. Fill in both hashes from the first build's errors. Playwright is only a development dependency and is not needed.

### Configuration

Add the server to `~/alyosha/config/config.toml`. The tool appears as `mcp__search__search`. [MCP configuration](MCP.md#v1-configuration)

```toml
[mcp.servers.search]
command = "open-websearch"
tools = ["search"]

[mcp.servers.search.env]
MODE = "stdio"
SEARCH_MODE = "request"
DEFAULT_SEARCH_ENGINE = "duckduckgo"
ALLOWED_SEARCH_ENGINES = "bing,duckduckgo,brave,startpage,baidu"
MCP_TOOL_FETCH_WEB_NAME = "<disabled>"
MCP_TOOL_FETCH_GITHUB_NAME = "<disabled>"
MCP_TOOL_FETCH_CSDN_NAME = "<disabled>"
MCP_TOOL_FETCH_JUEJIN_NAME = "<disabled>"
MCP_TOOL_FETCH_LINUXDO_NAME = "<disabled>"
```

| Setting | Why |
| --- | --- |
| `MODE = "stdio"` | The default, `both`, also starts an HTTP server on port 3000. |
| `SEARCH_MODE = "request"` | Never try to drive a browser for Bing. Playwright is not installed. |
| `DEFAULT_SEARCH_ENGINE` | A search that omits `engines` uses DuckDuckGo. |
| `ALLOWED_SEARCH_ENGINES` | The engines the model may request explicitly. Remove `baidu` if you never search Chinese content. |
| `MCP_TOOL_FETCH_*_NAME = "<disabled>"` | Turns off the page readers in the server itself. The `tools` allowlist filters them again in Alyosha. |

Leave `USE_PROXY` unset, because the container reaches the internet directly.

The `search` tool takes `query`, an optional `limit` (default 10), and an optional `engines` list. Check the names against `tools/list` after each upgrade.

### The browser skill

The `web-browsing` skill's first step tells the model to search before opening a page it does not know, and to retry with another engine when a search fails. Include that step once this server is configured. [The skill](Browser.md#the-skill)

### Verification

These need the configured model. Run them on the host at step 4 and again in the container at step 5.

- Start a session. Check that the `search` server registers only `search` and the startup shows no MCP warnings. [MCP checks](MCP.md#verification)
- While the session runs, list listening ports with `ss -ltn`, on the host or in the container. Check that nothing listens on port 3000.
- Ask "what is the latest NixOS release?" Check that the model searches, opens a result with the chromium tools, and answers from the page.
- Ask for a search with `engines` set to an engine not in `ALLOWED_SEARCH_ENGINES`. Record whether the server returns an error or falls back to an allowed engine, and check that the model still gets results.
- Ask a question in Chinese. Check that Baidu results come back.
- Run ten searches using the default engine. If a search returns no results, retry with another allowed engine. Repeat after a week of normal use. [When to switch to SearXNG](#when-to-switch-to-searxng)

[Architecture overview](README.md)
