# sandbox-kit

The write sandbox agent commands run in: `sandbox-exec` on macOS, Landlock on Linux. Reads, process
execution and the network stay as they are; writes are allowed only under the session workspace and a
fixed set of roots (see `src/sandbox.rs`).

It is part of the shared standard of CortexCode and CortexTerm. This folder must stay identical in
both repositories: a change is made in both, in the same piece of work, and listed in both
repositories' docs with the version it lands in.

Use it by path (`cc-sandbox-kit = { path = "../../../sandbox-kit" }`):

1. `Policy::new(home, data_dir, extra_write_paths)` once per plugin instance.
2. `policy.sandbox(workspace, venv, grants)` for a session, where `grants` comes from
   `sandbox::grants(&approved_classes)`.
3. `sandbox.command(program)` gives the confined `std::process::Command`.
