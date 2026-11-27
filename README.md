# Alyosha

Alyosha is a general-purpose personal assistant for NixOS. You message it through Telegram or QQ. It can answer directly or use tools in its own working folder. The v1 design calls for a Rust implementation for personal use.

## Start contributing

This checkout contains the v1 design, one page per component in `docs/`, and the implementation: the `alyosha` crate at the root and the `config/`, `schedule/`, `bridge/`, `telegram/`, and `qq/` packages, with a flake in `nix/`. `cargo test --workspace` runs the offline checks. Start with the [architecture and implementation order](docs/README.md), then open the page for the component you want to build. [AGENTS.md](AGENTS.md) is a short entry point for coding assistants. The same documents work with any editor or agent.

The first implementation milestone is one complete turn with a scripted model and a fake file reader. The [turn verification case](docs/AgentLoop.md#verification-with-a-scripted-model-and-fake-tools) gives the model replies, expected history, and call counts. It can run without a provider account.

## Try it

```bash
nix build .#alyosha .#gproxy          # or: cargo build --workspace
# add nixosModules.container to your NixOS configuration, rebuild, then on the host:
alyosha gateway init                  # GPROXY's master key, as a secret in the container
alyosha gateway login                 # sign in to the provider
alyosha secret set telegram-token     # and qq-secret for the QQ bridge
```

Write your settings as `services.alyosha.settings`, in the shape of [examples/config.toml](examples/config.toml), and copy the skills in [skills/](skills) to `~/.agents/skills/`. Then message the Telegram or QQ bot. The container module is `nixosModules.container` in the flake. [Deployment](docs/Deployment.md#implementation)
