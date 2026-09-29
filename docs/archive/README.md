# Archived implementation plans and handoffs

These documents record completed implementations or research whose chosen
approach has been built. Their old instructions, paths and baseline
summaries describe that work at the time. Use [the active handoff](../HANDOFF.md)
and [ARCHITECTURE.md](../ARCHITECTURE.md) for current rules and behavior.
Accepted architecture decisions remain in [../adr/](../adr/README.md).

| Document | Historical work |
| --- | --- |
| [HANDOFF_PLAN.md](HANDOFF_PLAN.md) | Original daemon MVP: discovery, TLS, pairing, clipboard, transfers and HTTP API. |
| [HANDOFF_UI_MILESTONE.md](HANDOFF_UI_MILESTONE.md) | Finished Flutter UI milestone, before the native Rust rewrite. |
| [PLAN_ICED_UI.md](PLAN_ICED_UI.md) | Replacement of Flutter with the Rust/iced app. |
| [PLAN_UI_FLATTEN.md](PLAN_UI_FLATTEN.md) | Concrete UI feature messages and routes; feature code moved under `src/ui/`. |
| [PLAN_STORE.md](PLAN_STORE.md) | Daemon data moved to SQLite. Async access is the later ADR 0003 decision. |
| [PLAN_I18N.md](PLAN_I18N.md) | Localization implementation. Remaining live-app verification is carried in the active handoff. |
| [feature-modules.md](feature-modules.md) | Completed feature module plan and its implementation history. |
| [remote-file-browsing.md](remote-file-browsing.md) | Research behind the implemented in-app SFTP browser. |
| [KDECONNECT_PROTOCOL_RESEARCH.md](KDECONNECT_PROTOCOL_RESEARCH.md) | Protocol research supporting the original daemon implementation. |
| [HANDOFF_STATIC_PLUGINS.md](HANDOFF_STATIC_PLUGINS.md) | Replacement of `Arc<dyn Plugin>` with a fixed enum; the decision is [ADR 0004](../adr/0004-dispatch-plugins-through-a-fixed-enum.md). |
| [flutter-adr/](flutter-adr/README.md) | Decisions for the deleted Flutter app; native ADR 0001 identifies which still apply. |

Archiving a plan does not close any manual checks carried into the active
handoff.
