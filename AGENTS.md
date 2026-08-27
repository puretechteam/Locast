# Locast project agent conventions

This project follows the cross-project conventions in the shared `AGENTS.md` that sits alongside this project in the same parent directory (e.g., `../AGENTS.md` when Locast is a sibling of the cross-project conventions file). Read that file first. The rules below are Locast-specific and are added to (never weaken) the shared rules.

## Project-specific rules

- **Architecture and roadmap are source of truth.** `docs/ARCHITECTURE.md` and `docs/ROADMAP.md` define what Locast is and how it is being built. Do not modify either file without explicit user instruction. If you believe a change is needed, surface it as a recommendation; do not edit.
- **Stay inside the project root.** The working directory is the project root (this repository). Do not read, write, or `cd` outside of it. Do not walk out into sibling projects. If you think you need something from another project, ask the user first.
- **Pick the smallest atomic task from the roadmap.** When implementing, find the smallest task in `docs/ROADMAP.md` whose completion advances the current goal. Tasks are sized for a single focused coding session; do not start a multi-day task. Sequence tasks by their prerequisite IDs.
- **Always run the build before claiming a task is done.** Before reporting any task complete, run the relevant build and test commands:
  - Rust side: `cargo check --workspace --all-targets`, then `cargo test --workspace`.
  - TypeScript side: `pnpm install`, then `pnpm typecheck` and `pnpm test`.
  - Full check: from the repo root, `pnpm install && cargo check --workspace --all-targets && pnpm typecheck`.
  If a command fails, fix it or surface the failure to the user. Do not claim a task is done while the build is red.
- **Subagents by default.** Follow the shared rule about delegating exploration to subagents. Do not grep or read many files in the main context when a subagent can do it. See the **Subagent workflow** section below for the mandatory rules that govern when, how, and how many subagents must be used for implementation work in this project.
- **Match the architecture's module layout.** New code goes into the directories described in section 26 of the architecture (`apps/client/src-tauri/`, `apps/client/src/`, `apps/server/`, `shared/`). If a new directory is needed, surface that as a recommendation rather than creating one silently.
- **No emojis in code, comments, commit messages, or docs.**

## Project-specific build commands

These are placeholders for now. They will be filled in as the scaffolding lands in Phase 0.

- Install dependencies: `pnpm install` (TypeScript side); `cargo fetch` (Rust side).
- Build everything: from the repo root, `pnpm install && cargo build --workspace`.
- Run the client in dev: `pnpm tauri dev` (from `apps/client/`).
- Run the server in dev: `cargo run -p locast-server` (from the repo root).
- Run all tests: `pnpm test && cargo test --workspace`.
- Lint: `pnpm lint && cargo clippy --workspace --all-targets -- -D warnings`.
- Typecheck: `pnpm typecheck`.

These commands are expected to be valid by the end of Phase 0 (P0-T01..P0-T07). Until then, an attempt to use a command that does not yet exist is expected; surface that to the user, do not invent a workaround.

## Git / GitHub

Follow the shared `AGENTS.md` rules. In addition, for this project:

- This project will live at `https://github.com/puretechteam/locast` (confirm before pushing).
- The shared rules forbid force-pushing and uninvited git operations; that applies here too.
- The release workflow (P9-T01) is the only thing that should produce a tag; do not tag locally without explicit user instruction.

## When in doubt

If a task feels like it crosses one of these boundaries, ask the user before doing it. The architecture and roadmap are the only source of truth; this `AGENTS.md` is the only rule book.

## Subagent workflow (mandatory)

Subagents are a first-class part of the Locast implementation workflow, not an optional accelerator. The primary agent (the one running in the main session) is the coordinator and final decision maker. Subagents provide implementation, investigation, testing, review, or research support, and their work is integrated only after the primary agent reviews it.

The rules below apply to every non-trivial implementation task in this project. They were added after a pattern was observed of the primary agent doing too much in the main context and undersuing the available subagent capacity. They are not suggestions.

1. **Delegation is the default, not the exception.** For every non-trivial implementation task, the primary agent MUST use at least one subagent for a meaningful independent subtask before declaring the task complete. A meaningful subtask is one whose output materially changes the task's code, tests, or design decisions, not a one-line lookup.

2. **The primary agent owns coordination.** The primary agent is the only one that:
   - Decides which roadmap task is in scope.
   - Defines the subtask scope given to each subagent.
   - Reviews the subagent's output and accepts, rejects, or iterates on it.
   - Runs the final acceptance checks (build, typecheck, test, runtime smoke).
   - Writes the task completion report and signs off on the task.

3. **Two-subagent preference for non-trivial domains.** For tasks involving architecture decisions, networking, security, concurrency, storage layout, protocol design, or generated artifacts, the primary agent MUST prefer at least TWO subagents with different responsibilities, when the environment supports it. The default pair is:
   - one implementation / investigation subtask
   - one independent review / test / validation subtask (the second subagent should not have written the code under review)
   Splitting these into separate subagents gives a genuine second pair of eyes and surfaces problems the implementing subagent has blind spots for. The same agent must never both write and review the same change in the same task.

4. **Subagents must have clearly bounded responsibilities.** A subagent prompt must name a specific deliverable. Examples that count as "meaningful" for this project:
   - investigate a specific technical problem (e.g. "find why the gen_bindings test binary fails to start on Windows")
   - implement an isolated module (e.g. "implement `apps/client/src/services/ipc.ts` against the bindings file")
   - review a specific implementation for bugs or security issues (e.g. "review the Rust `commands::greet` and the bindings export for input validation and unsafe conversions")
   - write or run focused tests (e.g. "extend `apps/client/src-tauri/tests/gen_bindings.rs` to assert every command argument type is `specta::Type`)
   - inspect platform compatibility (e.g. "confirm the WebView2 redist version on this host matches what wry 0.55 expects")
   - verify generated artifacts (e.g. "diff the checked-in `bindings/index.ts` against a fresh tauri-specta run and explain every difference")
   A subtask that only re-reads a single file the primary agent already has open is not a meaningful subtask.

5. **No blind delegation.** The primary agent MUST:
   - define the subtask in the prompt with enough context for the subagent to act independently
   - read and reason about the subagent's output before accepting it
   - re-run or cross-check the subagent's claims when they affect acceptance
   - decide explicitly to integrate or discard the output; never silently fold it in

6. **The completion report names the subagents.** Every task completion report MUST list, in a dedicated section, which subagents were used and what each one contributed. If no subagents were used, the report must say so and justify why the task was trivial enough to skip them. A report that omits this section is incomplete.

7. **Trivial tasks are exempt only when justified.** Reading a single small file, applying a one-line refactor, running an existing build command, or making a docs typo fix are examples of work that does not benefit from delegation. For these, the primary agent may skip subagents and note in the completion report: "no subagents used; task was trivial (one-line change / read-only inspection / build-only check)." Anything larger than that needs subagents.

8. **Consequential external actions are never delegated.** The primary agent MUST NOT delegate:
   - secret handling, credential disclosure, or rotation
   - `git push`, force-push, branch creation, tag creation, or any remote mutation
   - GitHub repository creation, PR creation, issue creation, or release publication
   - any action that changes the public state of the project or any external system
   These require explicit user authorization, and they must be performed (or directly supervised) by the primary agent, never handed to a subagent.

9. **The primary agent runs the final acceptance.** After subagents have done their work, the primary agent runs the actual `cargo` / `pnpm` / `tauri` / `curl` / `docker` / `git` commands that the roadmap's acceptance criteria require, and reports the literal outputs. A subagent's claim that a command passed is not a substitute.

10. **Cross-project rules still apply.** The shared `../AGENTS.md` (subagent use, no force-push, no version bumps, gitignore hygiene) is not weakened by anything in this section. Where a Locast rule and the shared rule conflict, the more restrictive rule wins.
