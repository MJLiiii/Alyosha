# Alyosha

Alyosha is a general-purpose personal assistant for NixOS. You give it a task and a workspace. It can answer directly or use tools to work with your files. The v1 design calls for a Rust implementation and a simple terminal UI for personal use.

## Start contributing

This checkout contains the v1 design, its implementation guides, and the implementation: the `alyosha` crate at the root and the `config/`, `schedule/`, `bridge/`, `telegram/`, and `qq/` packages, with a flake in `nix/`. `cargo test --workspace` runs the offline checks. Start with the [architecture and implementation order](docs/README.md), then open the guide for the component you want to build. [AGENTS.md](AGENTS.md) is a short entry point for coding assistants. The same documents work with any editor or agent.

The first implementation milestone is one complete turn with a scripted model and a fake file reader. The [turn verification case](docs/develop/AgentLoop.md#verification-with-a-scripted-model-and-fake-tools) gives the model replies, expected history, and call counts. It can run without a provider account.

## Try it

```bash
nix build .#alyosha .#gproxy          # or: cargo build --workspace
alyosha gateway init                  # writes ~/.config/alyosha/gproxy.env
# start GPROXY (see docs/develop/LLMClientGateway.md), then:
alyosha gateway login
cd ~/work/some-project && alyosha     # any folder works
```

Start from [examples/config.toml](examples/config.toml) for `~/.config/alyosha/config.toml`, and copy the skills in [skills/](skills/) to `~/.config/alyosha/skills/`. The container module is `nixosModules.container` in the flake. [Deployment](docs/develop/Deployment.md)
