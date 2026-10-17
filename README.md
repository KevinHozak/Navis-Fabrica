# Navis-Fabrica

A private Windows desktop workshop for local projects, repository activity, coding workflows and configurable AI providers.

The Tauri 2 / React / TypeScript application includes project scanning, provider configuration and workflow history. This checkout also implements local Markdown issues and the shared durable runner. The local backlog and managed-batch controls have scoped acceptance evidence; read [runner readiness](docs/runner-readiness.md) before a live batch. This 0.26.0 source integration does not claim a newly packaged desktop release or completed real-issue pilot.

## Development

Use the Node/npm versions declared in `package.json`, the checked-in Rust toolchain, Microsoft C++ build tools and WebView2 on Windows.

```powershell
npm ci
npm run dev                  # Frontend development server
npm run desktop:dev          # Prepare worker and start Tauri
npm run build                # TypeScript and production frontend
npm run desktop:check        # Prepare worker and check native bridge
npm run release:verify       # Check synchronized release source metadata
npm run desktop:build        # Release app and configured NSIS installer
```

The release build prepares and bundles `fabrica-worker.exe`. Generated targets and packaged worker resources are ignored. Build from the canonical project folder for user-facing delivery, preserving local work and the existing version.

## Local issues and shared runner

Markdown is the editable backlog; generated HTML is optional. CLI and native IPC use the same versioned Rust core and explicit data root. Preview does not dispatch work. A live start requires a reviewed plan with issue snapshots, dependencies, isolated workspaces, provider/model assignments, budgets, checks and a distinct reviewer.

- [CLI and setup](docs/runner-cli.md)
- [Local issue format and ID allocation](docs/local-issues.md)
- [Fabrica controls](docs/runner-ui.md)
- [Jobs and dependencies](docs/runner-jobs.md)
- [Isolation](docs/runner-isolation.md)
- [Routing and budgets](docs/runner-routing.md)
- [Verification](docs/runner-verification.md)
- [Supervisor and recovery](docs/runner-supervisor.md)
- [Current readiness and evidence](docs/runner-readiness.md)

Detached workers require a Windows host that permits breakaway. Explicit foreground-hosted `run-batch` keeps its CLI host alive; it does not promise survival when that host closes. Unknown billing stays reserved. No scheduler or live batch is enabled merely by installing Fabrica.

## Product context

[Product plan](docs/product-plan.md), [architecture](docs/architecture.md) and [roadmap](docs/roadmap.md) include earlier design context. Current source and runner readiness distinguish implementation from verified acceptance. Local projects do not need a GitHub remote to use the issue store or runner.
