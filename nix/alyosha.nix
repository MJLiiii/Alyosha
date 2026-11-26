# Every Alyosha program in one package: alyosha (`alyosha acp` and the
# gateway commands), alyosha-schedule, alyosha-telegram, and alyosha-qq.
#
# `alyosha` is wrapped so the shell sandbox finds Bubblewrap, pasta, Bash,
# and the default tools as Nix store paths (docs/EnvironmentInterface.md#implementation).
{
  lib,
  rustPlatform,
  makeWrapper,
  pkg-config,
  aws-lc,
  bubblewrap,
  passt,
  bashInteractive,
  coreutils,
  findutils,
  git,
  ripgrep,
}:
rustPlatform.buildRustPackage {
  pname = "alyosha";
  version = "0.1.0";
  src = lib.cleanSourceWith {
    src = ../.;
    filter =
      path: type:
      let
        name = baseNameOf path;
      in
      !(builtins.elem name [ "target" "docs" "result" ".git" ]);
  };
  cargoLock.lockFile = ../Cargo.lock;
  cargoBuildFlags = [ "--workspace" ];
  # The scripted tests run with `cargo test` in the dev shell; checks that
  # need the sandbox or a provider run on the host.
  doCheck = false;
  # reqwest's rustls uses AWS-LC; nixpkgs builds it against the system one.
  nativeBuildInputs = [
    makeWrapper
    pkg-config
  ];
  buildInputs = [ aws-lc ];
  postInstall = ''
    wrapProgram $out/bin/alyosha --prefix PATH : ${
      lib.makeBinPath [
        bubblewrap
        passt
        bashInteractive
        coreutils
        findutils
        git
        ripgrep
      ]
    }
  '';
  meta = {
    description = "A general-purpose personal assistant for NixOS";
    mainProgram = "alyosha";
    platforms = lib.platforms.linux;
  };
}
