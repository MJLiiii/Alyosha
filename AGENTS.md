# Working on Alyosha

Start with the [architecture and implementation order](docs/README.md). For the component you are changing, read its page in `docs/`: the first part for expected behavior and its **Implementation** section for interfaces and verification cases. When a change crosses components, follow the links to the caller and the callee. The [built-in tool contracts](docs/reference/BuiltInTools.md) and [tool permissions](docs/reference/ToolPermissions.md) apply wherever those tools are called.

Use the component’s verification cases to check your work. Once the Rust crate exists, run `cargo test` from the repository root for the scripted tests. The gateway, environment, and deployment pages identify checks that need GPROXY, a provider account, a NixOS sandbox, the NixOS container, or PostgreSQL. When handing work to another contributor, state which checks you ran and which still need those dependencies.
