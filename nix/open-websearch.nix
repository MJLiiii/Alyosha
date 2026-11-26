# open-webSearch's MCP server, which is not in nixpkgs
# (docs/WebSearch.md#implementation). v2.1.7 or later fixes a server-side request
# forgery bug in its page reader.
{ buildNpmPackage, fetchFromGitHub }:
buildNpmPackage rec {
  pname = "open-websearch";
  version = "2.2.0";
  src = fetchFromGitHub {
    owner = "Aas-ee";
    repo = "open-webSearch";
    rev = "v${version}";
    hash = "sha256-5Hf1e3AgSC0brQqMqAbTt718ly1Ms5GoV0JuDAE1BCc=";
  };
  npmDepsHash = "sha256-fcRKZroEokDjEY1xiIu5kOqthIK0zx6ZnjpWMkzyIj0=";
  # Playwright is a development dependency only.
  env.PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD = "1";
}
