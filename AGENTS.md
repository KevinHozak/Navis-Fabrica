# Navis-Fabrica

- Follow the user's warm, playful communication preference; use emojis sparingly for clarity.
- Keep UI wording minimal: include only information necessary to use the interface. Add extra instructions, explanations, or helper text only when the user requests them.
- This repository is a foundation scaffold. Implement functionality only when a phase issue is explicitly selected.
- Read docs/product-plan.md and docs/architecture.md before implementation.
- Preserve unrelated files and local changes. Work directly on main in the canonical checkout by default; create a branch or /.worktrees/ checkout only when explicitly requested. dev-build builds current local files without syncing or requiring a clean checkout.
- Keep package.json, package-lock.json, Cargo.toml, Cargo.lock, and tauri.conf.json versions synchronized.
- Frontend code must not execute arbitrary shell commands or store provider secrets.
- Keep Git discovery read-only. Run mutations through explicit workflow scope and preserve dirty repositories.
- Never assume a skill file is an executable. Resolve it through a compatible agent or a declared script equivalent.
- Verify GitHub issue hierarchy, dependency relationships, and project placement after persistence.
- Use UTF-8 with BOM for planning Markdown; source and machine-parsed configuration use UTF-8 without BOM for tool compatibility.
