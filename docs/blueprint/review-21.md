---
doc: code_review
project: picast
version: 1
phase: code_review
author: agent
created: 2026-07-30T00:00:00Z
updated: 2026-07-30T12:50:00Z
---

# Code Review: `src/resolver/src/deobfuscation.rs`

**File:** `src/resolver/src/deobfuscation.rs`
**Lines:** 982 (including ~450 lines of tests — 31 test functions)
**Reviewer:** agent
**Date:** 2026-07-30

## Summary

This file implements a trait-based, pluggable deobfuscation pipeline for video hosting sites. Each deobfuscation step (ROT13, Base64 decode, char-shift, reverse, JSON parse, regex extract, etc.) implements the `DeobfuscationStep` trait, and a `DeobfuscationPipeline` chains them together. The pipeline is built from a TOML config at runtime, so adding new providers requires only a config file change — no code modification. This is one of the best-architected modules in the project: the trait abstraction is clean, the step implementations are focused, and the test coverage (31 tests) is solid. The main concerns are a ReDoS risk from config-supplied regex patterns and the lack of URL scheme validation in bait filtering.

## Scope Reviewed

| Component | Lines | Purpose |
|-----------|-------|---------|
| `DeobfuscationStep` trait | 24–30 | `apply(&self, input: &str) -> Option<String>` + `name()` |
| `Rot13Step` | 36–63 | ROT13 character rotation |
| `StripMarkersStep` | 65–84 | Remove marker patterns from string |
| `Base64DecodeStep` | 86–98 | Base64 decode |
| `CharShiftStep` | 100–128 | Shift each char's code point by -amount |
| `ReverseStep` | 130–142 | Reverse the string |
| `JsonParseStep` | 144–166 | Parse as JSON |
| `RegexExtractStep` | 168–184 | Extract first regex capture group |
| `CleanBase64Step` | 186–211 | Clean and decode Base64 with URL-safe variants |
| `StripUnderscoresStep` | 213–223 | Remove underscores |
| `DeobfuscationPipeline` | 232–280 | Chain of steps, abort on first failure |
| `build_pipeline()` / `build_step()` | 285–315 | Construct pipeline from config |
| `extract_content()` | 320–371 | Extract content from HTML |
| `extract_url_from_deobfuscated()` | 387–450 | Extract URL from deobfuscated JSON |
| Tests | 525–982 | 31 test functions |

## Findings

### Design Issues

#### DESIGN-001: Regex patterns compiled on every call — performance concern
- **Severity:** Low
- **Location:** Lines 178–182 (`RegexExtractStep::apply` — `Regex::new(pattern)` inside `apply`) and lines 425–433 (`RegexUrl` rule — `Regex::new(pattern)` inside `extract_url_from_deobfuscated`)
- **Description:** Both `RegexExtractStep` and the `RegexUrl` extraction rule compile their regex pattern on every call. The `Regex::new()` call parses the pattern and builds an NFA, which is expensive (10–100μs per pattern). For a pipeline that runs on every cast, this adds unnecessary latency.
- **Impact:** Minor — the regex compilation is fast enough for single-use. But if the same pipeline is used multiple times (e.g., across retries), the pattern is recompiled each time.
- **Recommendation:** Compile the regex once in the step's/rule's constructor and store the compiled `Regex`:
  ```rust
  pub struct RegexExtractStep {
      regex: Regex,
  }
  impl RegexExtractStep {
      pub fn new(pattern: &str) -> Option<Self> {
          Regex::new(pattern).ok().map(|regex| Self { regex })
      }
  }
  impl DeobfuscationStep for RegexExtractStep {
      fn apply(&self, input: &str) -> Option<String> {
          self.regex.captures(input).and_then(|c| c.get(1).map(|m| m.as_str().to_owned()))
      }
  }
  ```

#### DESIGN-002: `CharShiftStep` can produce control characters
- **Severity:** Low
- **Location:** Lines 112–120 (`CharShiftStep::apply` — `char::from_u32(code - self.amount)`)
- **Description:** The step subtracts `self.amount` from each character's code point. If the result is a valid Unicode code point but a control character (e.g., shifting 'a' (97) by 96 gives code 1 = SOH control character), the control character is included in the output. Downstream steps (e.g., Base64DecodeStep) may fail or produce unexpected results when processing control characters.
- **Impact:** Low — in practice, the shift amount is configured to produce valid Base64 characters. But a misconfigured shift amount could produce garbage that silently fails downstream.
- **Recommendation:** Document that the shift amount must be chosen to produce valid characters for the expected input range. Or add a validation step that rejects output containing control characters.

#### DESIGN-003: Pipeline `run()` doesn't log intermediate results
- **Severity:** Low
- **Location:** Lines 253–268 (`DeobfuscationPipeline::run` — only logs step name and success/failure)
- **Description:** The `run()` method logs the step name and whether it succeeded, but doesn't log the intermediate output. When debugging a failing pipeline, it's hard to see what each step produced without adding temporary print statements.
- **Impact:** Minor — debugging is harder than it needs to be. The `trace` level logs the step name but not the data.
- **Recommendation:** Add an optional `trace`-level log of the intermediate output (truncated to 100 chars for safety):
  ```rust
  tracing::trace!(step = step.name(), output = %&output[..output.len().min(100)], "step output");
  ```

### Security

#### SEC-001: Config-supplied regex patterns are vulnerable to ReDoS
- **Severity:** Medium
- **Location:** Lines 178–182 (`RegexExtractStep` — pattern from `StepDef`) and lines 425–433 (`RegexUrl` — pattern from config)
- **Description:** The regex patterns used in `RegexExtractStep` and `RegexUrl` rules come from the TOML config file (`providers.d/*.toml`). A malicious or malformed config file could include a regex with catastrophic backtracking (e.g., `(a+)+$`), causing the resolver to hang for seconds or minutes on a crafted input. The `regex_lite` crate (used here) is not immune to ReDoS — it just has a smaller matching engine.
- **Impact:** A malicious config file could DoS the resolver. In practice, the config files are root-owned and not attacker-controlled, but defense-in-depth is warranted.
- **Recommendation:** (a) Validate regex patterns at config load time by testing them against a set of known-problematic inputs with a timeout. (b) Use `regex` (not `regex_lite`) which has some built-in protections. (c) Wrap regex execution in a `tokio::time::timeout` to prevent indefinite hangs.

#### SEC-002: `is_bait_source()` doesn't validate URL scheme
- **Severity:** Low
- **Location:** The `is_bait_source()` helper used in `extract_url_from_deobfuscated()` (lines 435, 445)
- **Description:** The bait filtering checks domain and filename patterns but doesn't validate that the URL scheme is `http://` or `https://`. A `javascript:` or `data:` URL that doesn't match any bait pattern would pass through and be returned as a valid media URL.
- **Impact:** Low — the URL is ultimately passed to `souphttpsrc` or `StreamSource`, which would reject non-HTTP schemes. But the resolver should validate this early.
- **Recommendation:** Add a scheme check in `extract_url_from_json_value()` or `is_bait_source()`.

### Missing Tests

#### TEST-001: No test for ReDoS-resistant regex handling
- **Severity:** Low
- **Description:** There's no test that verifies the regex step handles pathological patterns gracefully (e.g., with a timeout or error).
- **Impact:** A ReDoS vulnerability (SEC-001) would not be caught by tests.
- **Recommendation:** Add a test with a known-pathological pattern and verify it either fails fast or times out.

#### TEST-002: No test for empty pipeline
- **Severity:** Low
- **Description:** There's no test for `DeobfuscationPipeline::run()` when the pipeline has zero steps. The `is_empty()` method exists but isn't tested.
- **Impact:** Minor — an empty pipeline should return the input unchanged, but this isn't verified.
- **Recommendation:** Add a test:
  ```rust
  #[test]
  fn test_empty_pipeline_returns_input() {
      let pipeline = DeobfuscationPipeline::new();
      assert!(pipeline.is_empty());
      assert_eq!(pipeline.run("test"), Some("test".to_string()));
  }
  ```

#### TEST-003: No test for `CharShiftStep` with large shift amounts
- **Severity:** Low
- **Description:** The `CharShiftStep` tests likely use normal shift amounts (e.g., 3). There's no test for edge cases: shift amount of 0, shift amount larger than the character code (which would underflow), or shift amounts that produce control characters.
- **Impact:** The `if code >= self.amount` check prevents underflow, but the behavior with large shifts (producing control characters) is untested.
- **Recommendation:** Add tests for shift amount 0 (identity), shift amount equal to the character code (produces null character), and shift amount larger than any character code (all characters unchanged).

## Positive Observations

1. **Excellent trait-based architecture** — the `DeobfuscationStep` trait is the right abstraction. Each step is a focused, composable unit. Adding a new step (e.g., AES decrypt) requires only implementing the trait, not modifying the pipeline. This is the best-designed module in the resolver subsystem.

2. **Config-driven pipeline construction** — `build_pipeline()` and `build_step()` construct the pipeline from `StepDef` config entries. Adding a new provider's deobfuscation pipeline requires only a TOML config change, not code modification. This is exactly the right design for a system that needs to adapt to frequently-changing CDN obfuscation.

3. **Clean pipeline execution** — the `run()` method is simple and correct: iterate steps, pass the output of one as input to the next, abort on first failure. The `trace`-level logging of step names helps debugging.

4. **Good test coverage** — 31 tests cover individual steps, the pipeline, content extraction, and URL extraction. The tests use realistic obfuscation samples.

5. **Bait URL filtering** — `extract_url_from_deobfuscated()` filters out bait domains and filenames, preventing the resolver from returning decoy URLs.

6. **Priority-based URL extraction** — the `UrlExtractionRule`s are sorted by priority, so the most preferred URL source is tried first. This allows config to specify "try `mp4` key first, then `source` key, then regex fallback."

7. **Request token appending** — `append_rq()` correctly appends the `rq=` authentication token to CDN URLs, handling the `?` vs `&` separator based on whether the URL already has query parameters.

8. **`CleanBase64Step` handles URL-safe variants** — the step replaces `-` with `+` and `_` with `/` before decoding, handling URL-safe Base64 (used by some CDNs) correctly.

9. **`JsonParseStep` returns the parsed JSON as a string** — rather than trying to extract specific fields, the step just parses and re-serializes the JSON, normalizing whitespace and formatting. This is a clean design that lets downstream steps (regex extract) work on normalized JSON.

10. **`extract_content()` handles multiple extraction methods** — the function supports CSS selector, regex, and full-page extraction, covering the different ways CDNs embed obfuscated data in HTML.

## Recommendations Summary

| Priority | Finding | Effort |
|----------|---------|--------|
| Medium | SEC-001: Validate/timeout config-supplied regex patterns | M (2–3 h) |
| Low | DESIGN-001: Compile regex once in step constructors | S (1 h) |
| Low | DESIGN-002: Document CharShiftStep control character risk | S (15 min) |
| Low | DESIGN-003: Log intermediate pipeline outputs at trace level | S (30 min) |
| Low | SEC-002: Validate URL scheme in bait filtering | S (15 min) |
| Low | TEST-001: Add ReDoS resistance test | S (30 min) |
| Low | TEST-002: Add empty pipeline test | S (10 min) |
| Low | TEST-003: Add CharShiftStep edge case tests | S (30 min) |
