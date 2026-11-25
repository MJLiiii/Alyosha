# Alyosha

Alyosha is a general-purpose personal assistant for NixOS. You give it a task and a workspace. It can answer directly or use tools to work with your files. The v1 design calls for a Rust implementation and a simple terminal UI for personal use.

## Start contributing

This checkout currently contains the v1 design and implementation guides. Start with the [architecture and implementation order](docs/README.md), then open the guide for the component you want to build. [AGENTS.md](AGENTS.md) is a short entry point for coding assistants. The same documents work with any editor or agent.

The first implementation milestone is one complete turn with a scripted model and a fake file reader. The [turn verification case](docs/develop/AgentLoop.md#verification-with-a-scripted-model-and-fake-tools) gives the model replies, expected history, and call counts. It can run without a provider account.
