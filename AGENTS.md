# Project agent memory

This file is the project's committed home for project-intrinsic agent knowledge: build, test, release, architecture, and sharp-edge notes that should travel with the code.

- Add durable project-specific notes here as they are discovered through real work.
- Layout, build/run/test commands, protocol details, and scope (what's implemented vs. planned)
  are documented in `README.md` — read that first, it's the source of truth, not this file.
- `flake.nix` pins `nixpkgs` to `nixos-25.11` deliberately: `libmpv2` requires Rust's
  `edition2024`, which needs Cargo/rustc >= 1.85; `nixos-24.11`'s toolchain is too old and fails
  with "feature `edition2024` is required".
- `nix build`/`nix flake check` fetch crate sources straight from `crates.io` (no vendored
  `Cargo.lock` hashes beyond what `cargoLock.lockFile` gives). That endpoint occasionally 403s a
  plain-`curl` fetch (no User-Agent) with no pattern tied to a specific crate; if a build fails
  with `curl: (22) ... 403` on a `crate-*.tar.gz.drv`, just retry the same `nix build` — it
  resumes from whatever already fetched successfully and has always succeeded within a few
  retries. This is a `crates.io`-side bot-mitigation quirk, not a broken lockfile.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
