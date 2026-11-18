# Native runs

Add `native:` to a leaf Verification Definition or root manifest to run your
project's own tests, formatters, linters, or other command-line verification.
Commands run in declaration order after provisioning and before setup and criteria.
A manifest command runs once before any leaf; each leaf can also declare commands.

```yaml
native:
  - id: lint
    layer: static
    command: [sh, -c, 'printf "checked\n"']
    cwd: .
    env: {LANG: C}
    timeout: 2m
    required: true
    expect:
      exit_code: 0
      stdout: {matches: '^checked'}
      stderr: {not_matches: '^warning:'}
```

`id` must be nonempty and unique within its document. `command` is a nonempty
argv list; use `[sh, -c, '...']` explicitly when shell syntax is needed. `cwd`
is relative to the declaring document's directory and defaults to that directory.
`env` adds explicit child environment values to the usual sanitized CLI environment.
`timeout` accepts milliseconds or a duration such as `30s` or `2m`; omitted, it
defaults to 30 minutes. A failing run's detail shows the last 20 lines of stdout
and stderr; the full masked output is stored as evidence blobs.

`layer` is optional and accepts `unit`, `integration`, `e2e`, `static`, or `other`.
`required` defaults to true. `expect.exit_code` defaults to zero. `matches` requires
a regex match and `not_matches` forbids one, independently for stdout and stderr.
Patterns use the same Rust regex semantics as `matches:` assertions.

Without `report`, a command counts as one test. A matching exit code and output
produce `pass`. An unexpected exit or output produces `fail`. Timeout and spawn
failures produce `inconclusive`. Evidence blobs and failure details are masked.

## JUnit reports

Configure the command to write a fresh UTF-8 JUnit XML report on each run. Set
`report: {format: junit, path: ...}` using a path relative to the command's
`cwd`. This worked leaf VD runs pytest as a gate alongside an independent check:

```yaml
verification: service-contract
native:
  - id: python-tests
    layer: unit
    cwd: ..
    command: [pytest, --junitxml=pytest-results.xml]
    report: {format: junit, path: pytest-results.xml}
criteria:
  - id: readiness
    description: The running service exposes a successful health response.
    checks:
      - id: health-response
        steps:
          - id: health
            uses: api/call
            with: {url: 'http://localhost:8080/health', method: GET}
        assertions: ['$steps.health.outputs.status == 200']
```

Duhem reads JUnit reports from nextest, pytest, jest-junit, Playwright and
go-junit-report. A missing, unreadable or malformed report makes the native run
`inconclusive`, as does a timeout. With a parsed report, any failed or errored
testcase, unexpected exit code, or unmatched output expectation fails the run.
Skipped tests do not fail the run. Counts come from individual testcase records,
so a command that exits unexpectedly after passing tests still reports those tests
as passed. Validation rejects `format: ctrf` with `ctrf not yet supported`.

Required commands that fail or are inconclusive block the overall run and return
a nonzero CLI exit. `required: false` keeps the outcome visible and counted while
leaving the gate unchanged. Native results never satisfy criteria and cannot be
referenced in assertions or runtime expressions. Criteria keep their independent
Duhem check verdicts.

`RunSummary.native` and `RunSetSummary.native` list command outcomes, layer, duration,
required status, exit code, failure detail, and output blob addresses. `layers`
contains test counts and durations per native layer, plus a `duhem` row for checks.
These fields are omitted when no native commands ran.

JUnit runs record `report_path`, `report_blob_sha256` for the masked source XML,
and `results_blob_sha256` for the full normalized, masked JSON array. Each result
has an `id` composed of the native ID, classname, and testcase name, a `status`,
and `duration_ms`, `file`, and `message` when supplied. Parsed reports add `counts`
with `passed`, `failed`, `errored`, and `skipped`. Layer totals include nonzero
`errored` and `skipped` counts. JSON and terminal summaries show up to ten failed
or errored tests, with each message limited to 2,048 characters.
