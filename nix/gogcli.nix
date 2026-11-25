# gogcli with its MCP server, which the nixpkgs version predates
# (docs/develop/EmailCalendar.md).
{ buildGoModule, fetchFromGitHub }:
buildGoModule rec {
  pname = "gogcli";
  version = "0.48.1";
  src = fetchFromGitHub {
    owner = "openclaw";
    repo = "gogcli";
    rev = "v${version}";
    hash = "sha256-jsyXBhLTImA1pIQ/MyHrq672Sp+glj3pYbmxMMRKXd8=";
  };
  vendorHash = "sha256-18pPtme/0QoeX1PkCdlhWStX3MYJT83ULHSJI3RIOlo=";
  subPackages = [ "cmd/gog" ];
}
