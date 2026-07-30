# BP-ADR-012: Keyboard-first accessible web UI with CI-enforced a11y checks

| Field        | Value          |
|--------------|----------------|
| **ID**       | BP-ADR-012        |
| **Status**   | PROPOSED       |
| **Date**     | 2026-07-30     |


| **Related** | BP-ADR-008 (installer + first-boot web UI — same UI surface) |

## Context

Problem [[P-012]] requires the web UI to pass WAVE accessibility evaluation and support keyboard-only navigation for all actions. Because BP-ADR-008 makes the web UI the only configuration surface for non-technical users (zero-SSH target), the UI must be accessible to users with visual impairments — otherwise that user segment is excluded from configuration entirely. Screen-reader support, keyboard navigation, and high-contrast mode are needed.

## Decision

Build the configuration web UI with semantic HTML, ARIA landmarks, and a keyboard-first interaction model: Tab / Shift-Tab / Enter / Escape cover all actions; no action requires a mouse. High-contrast mode is a CSS variable theme toggled from the UI header, persisted in `localStorage`. Automated checks run WAVE and axe-core in CI against every PR that touches `src/server/web/`. A manual NVDA + VoiceOver test pass is scheduled before the v1 release, and known issues are recorded in an `a11y.md` file in the docs tree. Accessibility regressions are tagged release-blocking in the issue tracker.

## Consequences

| Outcome | Impact |
|---------|--------|
| ✅ Web UI passes WAVE evaluation | Automated WAVE + axe-core in CI catches regressions before merge; matches P-012 success metric |
| ✅ Keyboard-only navigation works | All actions reachable via Tab / Shift-Tab / Enter / Escape; no mouse required |
| ✅ High-contrast mode | CSS-variable theme toggle covers users with low vision; persisted across sessions |
| ✅ Release-blocking severity for a11y regressions | Forces the team to fix accessibility issues before shipping, rather than deferring |
| ❌ WAVE/axe-core passing does not guarantee real screen-reader usability | Automated tools miss many real-world a11y issues; mitigated by manual NVDA + VoiceOver test pass before v1 release |
| ❌ Manual test pass is expensive | One NVDA + VoiceOver pass per release is non-trivial; must be scheduled and budgeted |
| ❌ A11y constraints may slow UI iteration | Keyboard-first + ARIA + high-contrast mode add design constraints; trade-off is intentional |

## Alternatives Rejected

| Alternative | Reason for Rejection |
|-------------|---------------------|
| **Ship CLI-only config path, skip web UI accessibility work** | Rejected because the zero-SSH success metric (P-008) makes the web UI the only configuration surface for non-technical users; skipping a11y excludes users with visual impairments from configuration |
| **Third-party accessible admin panel (e.g. Cockpit plugin)** | Rejected because it introduces a heavyweight dependency for a feature that fits in a few hundred lines of HTML; also Cockpit's accessibility story is not materially better than what boGDan can build directly |
| **Web UI without automated CI checks (manual-only a11y review)** | Rejected because manual-only review is easy to skip under release pressure; CI-enforced axe-core + WAVE catches regressions mechanically |
