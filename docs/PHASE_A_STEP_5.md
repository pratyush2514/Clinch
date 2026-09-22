# Selector repair, approvals, and viewport

This retained phase filename documents current task-lane behavior. It is not a historical validation report.

## Selector repair

[HealingReplay](../packages/macro-engine/src/lib.rs) is integrated into Engine::run_task. When a target or wait selector fails, it makes at most one provider attempt for that stage, bounded to 30 seconds. Invalid, ambiguous, out-of-region, or unresolved candidates stop with needs_repair.

Context contains a stripped structural clone around a unique local ancestor, the failed selector, and CSS-pixel bounds. Page text, field values, URLs, and script/style content are removed from the structural snippet. Without a surviving local anchor, repair fails closed rather than sending a whole page.

Validated updates use atomic macro publication and version-1 lastHealedAt/healingHistory fields. Wait-stage repair resumes only the wait. Missing provider configuration stops the repair; it does not select or download a model.

This integration is specific to the task macro lane. Legacy steps inside saved playbooks use replay_step and return repair needs without calling HealingReplay. Semantic execution uses accessibility grounding rather than CSS selector repair.

## Approval behavior

Click, non-secret fill, and submit actions require a decision bound to task/step identity. Review Details leaves execution blocked; Reject or deadline expiry does not authorize the action. Accepted decisions are written to sentinel_decisions before execution. The generic executor rejects submit; the dedicated approved path validates a form target.

Playbook semantic and legacy action approvals are separate service gates with session-event journaling. Navigation and typed download-links actions are not gated. The harmless preview-approval command is not the execution gate.

## Viewport

BrowserViewport polls JPEG frames and scales target highlights using viewport dimensions. BrowserScreencast acquires an event stream for a headless context and offers Take Control. Actual manual interaction occurs in the managed Chromium window, not through the image preview.

## Focused validation

```powershell
$env:CLINCH_CHROMIUM_PATH = 'C:\Program Files\Google\Chrome\Application\chrome.exe'
cargo test -p macro-engine --test healing -- --ignored --nocapture
```

The fixture uses a mock repair provider and isolated Chromium to check sanitized context, candidate validation, publication, wait-only repair, consent, and viewport capture. It does not establish live-model accuracy or rendered UI correctness. Current check results belong in [STATUS.md](STATUS.md); adapter setup is in [PHASE_A_FINAL_INTEGRATION.md](PHASE_A_FINAL_INTEGRATION.md).
