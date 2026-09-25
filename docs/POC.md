# Real-world validation plan

This is an acceptance plan for the current implementation, not evidence that its gates have passed. Source tests and builds cannot establish portal compatibility or product demand.

## Technical validation

Use five explicitly selected, user-authorized portals with differing session and 2FA behavior. Exercise the current generic file-download and semantic workflow paths; there is no packaged Invoice Harvester product or general LLM first-run planner.

Record:

- Native/extension sync outcome, whether authentication actually succeeds, and whether manual fallback completes. Target: imported sessions accepted on at least four of five portals, with working fallback elsewhere.
- Task record/replay behavior and percentage of repeat tasks using saved macros. Target: at least 90% macro reuse on the tested repeat workflow. The current UI only replays existing task macros; first-run selector recording needs an API/fixture caller.
- Targeted selector repair on a controlled changed page, including rejected/invalid output and preservation of completed actions.
- Approval rejection, expiry, duplicate decisions, and matching run/step identity. These gates already exist; validation must not omit them.
- UI legibility with three people unfamiliar with the implementation.
- Actual wall-clock time saved on a recurring task over two months, including login and approval time.

No fixture timing is a portal SLA. Cookie counts and URL classification are not authentication-success measurements.

## Existing instrumentation and gaps

Task snapshots/checkpoints record task mode, step states, and timing. The runs table records playbook/ephemeral summaries; session_events records sync outcomes and diagnostics. Session lends journal `session_lent:`/`session_lend_failed:` lines with host, cookie counts, and outcome labels (`cleared`, `persistent`, `not synced`, `synced (persisted)`), so lend attempts and challenge-escalation outcomes are countable. In-page account-home runs journal `in_page_goal_class:`, `in_page_goal_memory: hit/miss/stale/write`, `in_page_goal_signed_out`, and `in_page_goal_miss:` lines with the worker's tried-click log — countable as attempts, verifications, memory hits, signed-out short-circuits, and pursuit misses (the misses surface a Take-control button, gated on the `account-home:` journal prefix). get_poc_metrics returns status counts and completed-task replay share.

These do not measure time-to-first-success, human correction rate, voluntary reuse, account health, or two-month time savings. Playbook approvals are text journal events rather than the task lane's dedicated decision rows. Capture pilot evidence separately and do not infer missing metrics from counters.

## Account-home acceptance (not yet measured)

The generic account-home lane (live on native Windows Reddit, 2026-09-25: "open my profile on reddit" COMPLETED twice on build `10016872`, landing on the real profile, ~9s then ~4.9s — the second run's memory hit is timing-inferred, not journal-confirmed) still needs recorded acceptance evidence:

- Multi-run identity-memory recall: the second-run speedup is timing-inferred, not journal-confirmed as a memory hit; record an explicit `in_page_goal_memory: hit` on a repeat prompt.
- Signed-out short-circuit: confirm a logged-out landing is never clicked and the sync offer appears instead.
- Pursuit-miss recovery: confirm the Take-control button appears on a genuine miss and hands the window over.
- Forget-site: confirm the remembered identity row is cleared alongside the cookies and the next run reads the origin as signed out.
- Generic noun-pursuit regression: confirm the account-home lane did not regress other artifact nouns ("settings", "pricing", non-identity nouns still take the generic noun-hunt lane).

## Possible later product pilot

The earlier Job Application Engine pilot is blocked on features that do not exist: ATS adapters, profile/resume storage, multi-page form filling, submission previews specific to ATS, and an application tracker.

If that product is separately implemented, retain the proposed acceptance goals: explicit approval before every submission, zero account warnings during a small informed pilot, meaningful time savings, and voluntary reuse. The existing generic submit action does not satisfy those product requirements by itself.

There is no committed week-by-week schedule. Advance from implementation tests to real-portal validation, then to user pilots based on recorded evidence. Future product scope is in [FUTURE_FEATURES.md](FUTURE_FEATURES.md).
