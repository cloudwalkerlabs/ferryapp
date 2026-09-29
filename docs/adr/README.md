# Architecture decision records

Each record is one decision: its context, the decision, and its
consequences. Accepted records don't change; a later decision supersedes
them instead.

The deleted Flutter app's records are in
[`../archive/flutter-adr/`](../archive/flutter-adr/README.md); 0001 below
says which of them still apply.

| # | Decision | Status |
| --- | --- | --- |
| [0001](0001-native-ui-in-iced.md) | Build the desktop UI in Rust with iced, in the daemon's process | Accepted |
| [0002](0002-store-the-daemons-data-in-sqlite.md) | Store the daemon's data in SQLite | Accepted; synchronous access superseded by 0003 |
| [0003](0003-use-async-sqlite-pools-and-plugin-callbacks.md) | Use async SQLite pools and plugin callbacks | Accepted; `async-trait` and erased dispatch superseded by 0004 |
| [0004](0004-dispatch-plugins-through-a-fixed-enum.md) | Dispatch plugins through a fixed enum | Accepted |

Template for new records:

```markdown
# NNNN. Title in the imperative

- Status: Proposed | Accepted | Superseded by NNNN
- Date: YYYY-MM-DD

## Context
## Decision
## Consequences
```
