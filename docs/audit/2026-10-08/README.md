# Audit, 2026-10-08

A read of the whole repository at commit `3ba01b7`. Twelve reports were
written by subagents and one by the session that ran them. Each report gives
file and line, a failure scenario, a suggested fix, and whether the path was
traced in full (CONFIRMED) or rests on behaviour that was not run (PLAUSIBLE).
Nothing in the reports was reproduced by running the program unless the
report says so.

`just lint`, `just test` and `just prose` passed before the audit started, so
none of these findings is caught by the existing gates.

| Report | Scope |
|---|---|
| [01](01-gui-linux-first-pass.md) | Linux window, first pass |
| [02](02-gui-linux-second-pass.md) | Linux window, second pass |
| [03](03-gui-visual.md) | Rendered previews and theme |
| [04](04-gui-wording.md) | Wording, glossary, docs drift |
| [05](05-gui-windows-mac.md) | Windows and Mac window, parity with Linux |
| [06](06-jobs-coordinator.md) | Linux job coordinator |
| [07](07-pack-store.md) | Maximum stores and the FUSE mount |
| [08](08-backend-sandbox.md) | btrfs backend, sandbox, probes |
| [09](09-estimate-database.md) | Estimates, classification, reports, database |
| [10](10-discovery.md) | Game discovery |
| [11](11-windows-backend.md) | Windows backend and coordinator |
| [12](12-macos-cli.md) | Mac backend and the command line |
| [13](13-ci-packaging-docs.md) | CI, releases, packaging, docs |

Fix status is recorded in [fixes.md](fixes.md).
