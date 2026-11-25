{
  description = "Alyosha, a general-purpose personal assistant for NixOS";

  # Unstable, for playwright-mcp 0.0.83 (docs/develop/Browser.md).
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
    in
    {
      packages.${system} = {
        alyosha = pkgs.callPackage ./nix/alyosha.nix { };
        gproxy = pkgs.callPackage ./nix/gproxy.nix { };
        gogcli = pkgs.callPackage ./nix/gogcli.nix { };
        open-websearch = pkgs.callPackage ./nix/open-websearch.nix { };
        # From this flake's unstable nixpkgs: 26.05 lacks it.
        playwright-mcp = pkgs.playwright-mcp;
        default = self.packages.${system}.alyosha;
      };

      # The container around everything (docs/develop/Deployment.md).
      nixosModules.container = import ./nix/container.nix self;

      devShells.${system}.default = pkgs.mkShell {
        packages = with pkgs; [
          cargo
          rustc
          clippy
          rustfmt
          bubblewrap
          passt
          python3 # the fake MCP server in the tests
        ];
      };
    };
}
