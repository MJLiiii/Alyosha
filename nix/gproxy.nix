# GPROXY Headless at a pinned release: the official static musl build,
# installed unchanged. To upgrade, change `version` and `hash`, taking the
# hash from the release's manifest.json (docs/LLMClientGateway.md#implementation).
{ stdenvNoCC, fetchurl, unzip }:
stdenvNoCC.mkDerivation rec {
  pname = "gproxy";
  version = "4.2.5";
  src = fetchurl {
    url = "https://github.com/LeenHawk/gproxy/releases/download/v${version}/gproxy-headless-linux-x86_64-musl.zip";
    # manifest.json: 96746f386125387587b54f4e39cb2460c85eaa6103cb012c13ddf9381b243621
    hash = "sha256-lnRvOGElOHWHtU9OOcskYMheqmEDywEsE935OBskNiE=";
  };
  nativeBuildInputs = [ unzip ];
  sourceRoot = ".";
  dontConfigure = true;
  dontBuild = true;
  dontFixup = true; # keep the upstream executable byte for byte
  installPhase = "install -Dm755 gproxy $out/bin/gproxy";
  meta.platforms = [ "x86_64-linux" ];
}
