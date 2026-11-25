# Set up web search

Web search needs no Alyosha code. Package open-webSearch, add one MCP server, and include the search step in the browser skill. Do this after the browser works. [Web search design](../design/WebSearch.md) · [Browser setup](Browser.md) · [Architecture overview](../README.md)

Facts below come from open-webSearch's README for v2.2.0, read in November 2026. [README](https://github.com/Aas-ee/open-webSearch) · [Releases](https://github.com/Aas-ee/open-webSearch/releases)

## Package

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

## Configuration

Add the server to `~/.config/alyosha/config.toml`. The tool appears as `mcp__search__search`. [MCP configuration](MCP.md#v1-configuration)

```toml
[mcp.servers.search]
command = "open-websearch"
tools = ["search"]

[mcp.servers.search.env]
MODE = "stdio"
SEARCH_MODE = "request"
DEFAULT_SEARCH_ENGINE = "bing"
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
| `ALLOWED_SEARCH_ENGINES` | The engines the model may name. Bing is the default. Remove `baidu` if you never search Chinese content. |
| `MCP_TOOL_FETCH_*_NAME = "<disabled>"` | Turns off the page readers in the server itself. The `tools` allowlist filters them again in Alyosha. |

Leave `USE_PROXY` unset, because the container reaches the internet directly.

The `search` tool takes `query`, an optional `limit` (default 10), and an optional `engines` list. Check the names against `tools/list` after each upgrade.

## The browser skill

The `web-browsing` skill's first step tells the model to search before opening a page it does not know, and to retry with another engine when a search fails. Include that step once this server is configured. [The skill](Browser.md#the-skill)

## Verification

These need the configured model. Run them on the host at step 4 and again in the container at step 5.

- Start a session. Check that the `search` server registers only `search` and the startup shows no MCP warnings. [MCP checks](MCP.md#verification)
- While the session runs, list listening ports with `ss -ltn`, on the host or in the container. Check that nothing listens on port 3000.
- Ask "what is the latest NixOS release?" Check that the model searches, opens a result with the chromium tools, and answers from the page.
- Ask for a search with `engines` set to an engine not in `ALLOWED_SEARCH_ENGINES`. Record whether the server returns an error or falls back to an allowed engine, and check that the model still gets results.
- Ask a question in Chinese. Check that Baidu results come back.
- Run ten searches in a row on Bing. Note whether any come back empty or blocked. Repeat after a week of normal use. Repeated failures are the signal to switch to SearXNG. [When to switch](../design/WebSearch.md#when-to-switch-to-searxng)

[Architecture overview](../README.md)
