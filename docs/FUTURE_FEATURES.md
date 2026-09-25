# Future features and open product directions

Everything here is unimplemented or incomplete. This is an idea backlog, not a delivery schedule or description of the current product. Current behavior is documented in [FEATURES.md](FEATURES.md) and [STATUS.md](STATUS.md).

## Reliability and packaging

- Real-portal regression coverage, platform parity validation, and a clear supported-browser matrix.
- Better recovery controls, richer metrics UI, structured playbook approval auditing, and explicit provider-egress visibility.
- Playbook management, import/export, and a user-facing signature-history/rollback workflow.
- Signed distribution, installer bundling, updates, and a managed browser-distribution strategy.
- Scheduling and background execution with explicit missed-run, retry, and approval semantics.

## Domain workflows

- Job applications: profile/resume storage, ATS adapters, form filling, submission previews and mandatory per-submission approval, and an application tracker.
- Recurring bill collection: portal-specific workflows plus structured expense extraction and summaries.
- Price monitoring: scheduled multi-site checks and threshold notifications.

The generic download task and semantic executor are foundations, not completed versions of these products. Each requires implementation and real-world acceptance evidence.

## Reuse and collaboration

- Extracted tables, richer output artifacts, and a scrubbable run history.
- Notion/Slack or other exports.
- Shared playbook libraries, roles, delegated approvals, and audit export.
- A marketplace only after sharing and review mechanisms exist.

## Longer-term exploration

- Native-app automation through platform accessibility APIs.
- Background notifications and proactive reminders. Messaging integrations require separate implementation and current provider review; no pricing or effort estimate is asserted here.
- Household administration: warranty/receipt ingestion, renewal tracking, and subscription workflows. Email/bank connections would be new integrations with separate consent and data-handling contracts.
- Long-running goals above playbooks, with their own state, scheduling, and review controls. There is no Goal model or check-in scheduler today. (`GoalClass` in the orchestration engine is unrelated: it is a closed table of *in-page task kinds* — "profile"/"account" → account-home — used to dispatch in-page follow-ups, not a user-goal system.)
- Mobile notifications or approval relay. Local approval remains the existing mechanism; remote authorization would require a separately designed trust boundary.

These ideas must not be used to infer existing APIs, dependencies, guarantees, or implementation completeness.
