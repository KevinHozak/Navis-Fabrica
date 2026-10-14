# Navis-Fabrica

- Follow the user's warm, playful communication preference; use emojis sparingly for clarity.
- This repository is a foundation scaffold. Implement functionality only when a phase issue is explicitly selected.
- Read docs/product-plan.md and docs/architecture.md before implementation.
- Preserve unrelated files and local changes. Use /.worktrees/ for isolated checkouts.
- Keep package.json, package-lock.json, Cargo.toml, Cargo.lock, and tauri.conf.json versions synchronized.
- Frontend code must not execute arbitrary shell commands or store provider secrets.
- Keep Git discovery read-only. Run mutations through explicit workflow scope and preserve dirty repositories.
- Never assume a skill file is an executable. Resolve it through a compatible agent or a declared script equivalent.
- Verify GitHub issue hierarchy, dependency relationships, and project placement after persistence.
- Use UTF-8 with BOM for planning Markdown; source and machine-parsed configuration use UTF-8 without BOM for tool compatibility.
