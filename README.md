# Navis-Fabrica

A private Windows desktop workshop for code projects, repository activity, coding workflows, and configurable AI providers.

**Current state:** basic Tauri 2 + React/TypeScript + Vite scaffold only. The launch page is a placeholder. Project scanning, workflow buttons, provider integration, routing, and settings are planned, not implemented.

## Development

Prerequisites: Node.js 22.12+ (or supported newer LTS), Rust stable, Microsoft C++ build tools, and WebView2 on Windows.

```powershell
npm ci
npm run dev                 # Browser scaffold
npm run desktop:dev         # Tauri scaffold
npm run build               # TypeScript + frontend production build
npm run desktop:check       # Native compile check
npm run desktop:build       # Release executable; packaging is not configured yet
```

Installer bundling is deliberately disabled until the release phase adds icons, packaging, and metadata verification.

## Planning

- [Product plan](docs/product-plan.md): user experience, delivery milestones, routing behavior.
- [Architecture](docs/architecture.md): boundaries, proposed contracts, persistence, safety.
- [Roadmap](docs/roadmap.md): epics and native child phase issues.
- [GitHub repository](https://github.com/KevinHozak/Navis-Fabrica)
- [Private project board](https://github.com/users/KevinHozak/projects/9)

The proposed project root defaults to `C:\Projects` and will be configurable. No folder is scanned by this scaffold.
