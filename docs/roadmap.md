# Navis-Fabrica roadmap

All items start in **Backlog** on [Project #9](https://github.com/users/KevinHozak/projects/9). Features are planned; the initial scaffold implements none of these phases. Native sub-issues track epic completion; native blockers match the dependency list.

Suggested first task: [Foundation-P1](https://github.com/KevinHozak/Navis-Fabrica/issues/8). Manual operation precedes automated routing.

## [Foundation: Desktop foundation and Navis UI](https://github.com/KevinHozak/Navis-Fabrica/issues/1)

Build the responsive desktop shell, reusable UI, and persisted configuration.

| Phase | Depends on | Priority |
| --- | --- | --- |
| [Foundation-P1: Desktop shell and design system](https://github.com/KevinHozak/Navis-Fabrica/issues/8) | None | High |
| [Foundation-P2: Settings and typed native boundary](https://github.com/KevinHozak/Navis-Fabrica/issues/9) | [#8](https://github.com/KevinHozak/Navis-Fabrica/issues/8) | High |
| [Foundation-P3: Responsive dashboard interactions](https://github.com/KevinHozak/Navis-Fabrica/issues/10) | [#8](https://github.com/KevinHozak/Navis-Fabrica/issues/8), [#12](https://github.com/KevinHozak/Navis-Fabrica/issues/12) | High |

## [Discovery: Local project and Git inventory](https://github.com/KevinHozak/Navis-Fabrica/issues/2)

Discover coding projects and repositories under a configurable root without changing them.

| Phase | Depends on | Priority |
| --- | --- | --- |
| [Discovery-P1: Bounded project scanner](https://github.com/KevinHozak/Navis-Fabrica/issues/11) | [#9](https://github.com/KevinHozak/Navis-Fabrica/issues/9) | High |
| [Discovery-P2: Git facts and stack detection](https://github.com/KevinHozak/Navis-Fabrica/issues/12) | [#11](https://github.com/KevinHozak/Navis-Fabrica/issues/11) | High |
| [Discovery-P3: Incremental refresh and discovery performance](https://github.com/KevinHozak/Navis-Fabrica/issues/13) | [#12](https://github.com/KevinHozak/Navis-Fabrica/issues/12) | High |

## [GitHub: Optional GitHub project context](https://github.com/KevinHozak/Navis-Fabrica/issues/3)

Enrich local projects with repository activity and issue selection while preserving offline use.

| Phase | Depends on | Priority |
| --- | --- | --- |
| [GitHub-P1: Auth and remote association](https://github.com/KevinHozak/Navis-Fabrica/issues/14) | [#12](https://github.com/KevinHozak/Navis-Fabrica/issues/12) | Medium |
| [GitHub-P2: Cached repository activity](https://github.com/KevinHozak/Navis-Fabrica/issues/15) | [#14](https://github.com/KevinHozak/Navis-Fabrica/issues/14), [#10](https://github.com/KevinHozak/Navis-Fabrica/issues/10) | Medium |
| [GitHub-P3: Issue picker and workflow context](https://github.com/KevinHozak/Navis-Fabrica/issues/16) | [#15](https://github.com/KevinHozak/Navis-Fabrica/issues/15) | Medium |

## [Workflows: Coding actions and recoverable runs](https://github.com/KevinHozak/Navis-Fabrica/issues/4)

Provide dev-work/dev-push/dev-build style actions with reliable execution and visible progress.

| Phase | Depends on | Priority |
| --- | --- | --- |
| [Workflows-P1: Project-aware action registry](https://github.com/KevinHozak/Navis-Fabrica/issues/17) | [#12](https://github.com/KevinHozak/Navis-Fabrica/issues/12), [#9](https://github.com/KevinHozak/Navis-Fabrica/issues/9) | High |
| [Workflows-P2: Managed execution and streamed runs](https://github.com/KevinHozak/Navis-Fabrica/issues/18) | [#17](https://github.com/KevinHozak/Navis-Fabrica/issues/17) | High |
| [Workflows-P3: Agent-backed work and push actions](https://github.com/KevinHozak/Navis-Fabrica/issues/19) | [#18](https://github.com/KevinHozak/Navis-Fabrica/issues/18), [#22](https://github.com/KevinHozak/Navis-Fabrica/issues/22), [#16](https://github.com/KevinHozak/Navis-Fabrica/issues/16) | High |
| [Workflows-P4: Build artifacts and interrupted-run recovery](https://github.com/KevinHozak/Navis-Fabrica/issues/20) | [#19](https://github.com/KevinHozak/Navis-Fabrica/issues/19) | High |

## [Providers: Configurable coding agents and local models](https://github.com/KevinHozak/Navis-Fabrica/issues/5)

Support manual provider/model choice with explicit tested capabilities and safe handoff.

| Phase | Depends on | Priority |
| --- | --- | --- |
| [Providers-P1: Provider contract and configuration](https://github.com/KevinHozak/Navis-Fabrica/issues/21) | [#9](https://github.com/KevinHozak/Navis-Fabrica/issues/9) | Medium |
| [Providers-P2: Codex coding adapter](https://github.com/KevinHozak/Navis-Fabrica/issues/22) | [#21](https://github.com/KevinHozak/Navis-Fabrica/issues/21), [#18](https://github.com/KevinHozak/Navis-Fabrica/issues/18) | Medium |
| [Providers-P3: Antigravity coding adapter](https://github.com/KevinHozak/Navis-Fabrica/issues/23) | [#22](https://github.com/KevinHozak/Navis-Fabrica/issues/22) | Medium |
| [Providers-P4: Ollama coding harness and portable handoff](https://github.com/KevinHozak/Navis-Fabrica/issues/24) | [#23](https://github.com/KevinHozak/Navis-Fabrica/issues/23), [#20](https://github.com/KevinHozak/Navis-Fabrica/issues/20) | Medium |

## [Routing: Usage-aware model routing and pace](https://github.com/KevinHozak/Navis-Fabrica/issues/6)

Select eligible models transparently using complexity, pace and trustworthy measured usage.

| Phase | Depends on | Priority |
| --- | --- | --- |
| [Routing-P1: Usage and latency signals](https://github.com/KevinHozak/Navis-Fabrica/issues/25) | [#22](https://github.com/KevinHozak/Navis-Fabrica/issues/22), [#18](https://github.com/KevinHozak/Navis-Fabrica/issues/18) | Medium |
| [Routing-P2: Explainable complexity and pace policy](https://github.com/KevinHozak/Navis-Fabrica/issues/26) | [#25](https://github.com/KevinHozak/Navis-Fabrica/issues/25), [#24](https://github.com/KevinHozak/Navis-Fabrica/issues/24) | Medium |
| [Routing-P3: Checkpoint fallback and routing calibration](https://github.com/KevinHozak/Navis-Fabrica/issues/27) | [#26](https://github.com/KevinHozak/Navis-Fabrica/issues/26), [#20](https://github.com/KevinHozak/Navis-Fabrica/issues/20) | Medium |

## [Release: Windows reliability and release delivery](https://github.com/KevinHozak/Navis-Fabrica/issues/7)

Ship a smooth, recoverable Windows application with verified artifacts and bounded data handling.

| Phase | Depends on | Priority |
| --- | --- | --- |
| [Release-P1: Cross-feature regression and security checks](https://github.com/KevinHozak/Navis-Fabrica/issues/28) | [#10](https://github.com/KevinHozak/Navis-Fabrica/issues/10), [#13](https://github.com/KevinHozak/Navis-Fabrica/issues/13), [#20](https://github.com/KevinHozak/Navis-Fabrica/issues/20), [#24](https://github.com/KevinHozak/Navis-Fabrica/issues/24), [#27](https://github.com/KevinHozak/Navis-Fabrica/issues/27) | Medium |
| [Release-P2: Performance accessibility and diagnostics](https://github.com/KevinHozak/Navis-Fabrica/issues/29) | [#28](https://github.com/KevinHozak/Navis-Fabrica/issues/28) | Medium |
| [Release-P3: Windows installer and verified release process](https://github.com/KevinHozak/Navis-Fabrica/issues/30) | [#29](https://github.com/KevinHozak/Navis-Fabrica/issues/29) | Medium |
