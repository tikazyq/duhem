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

A matching exit code and all output expectations produce `pass`; an unexpected
exit or output produces `fail`. Timeout and spawn failures produce `inconclusive`.
Without a report, every command counts as one test. Output is recorded as masked evidence blobs;
failure details are masked in JSON and terminal reports.

Required commands that fail or are inconclusive block the overall run and return
a nonzero CLI exit. `required: false` keeps the outcome visible and counted while
leaving the gate unchanged. Native results never satisfy criteria and cannot be
referenced in assertions or runtime expressions. Criteria keep their independent
Duhem check verdicts.

`RunSummary.native` and `RunSetSummary.native` list command outcomes, layer, duration,
required status, exit code, failure detail, and output blob addresses. `layers`
contains test counts and durations per native layer, plus a `duhem` row for checks.
These fields are omitted when no native commands ran.

## JUnit reports

Attach a JUnit XML report to count individual tests from nextest, pytest,
jest-junit, Playwright, or go-junit-report:

```yaml
verification: native-tests
native:
  - id: unit
    layer: unit
    command: [sh, -c, 'pytest --junitxml=results.xml']
    report: {format: junit, path: results.xml}
criteria:
  - id: AC
    description: The running service is healthy independently of its unit tests.
    checks:
      - id: health
        steps:
          - id: response
            uses: api/call
            with: {method: GET, url: 'http://localhost:8080/health'}
        assertions:
          - '$steps.response.outputs.status == 200'
```

The report path is relative to the command's working directory. Both a bare
`testsuite` and nested `testsuites` are supported. Counts come from testcase
records, not suite header totals: `passed`, `failed`, `errored`, and `skipped`.
Any failure or error fails the native run, including when the command exits zero.
Exit-code and output expectations still apply; a crash after passed tests fails
the gate while retaining those passed test counts. A missing, unreadable, or
malformed report is inconclusive. Timeout is always inconclusive, even if a partial
report contains failures. Native report results still never satisfy criteria.

Report-backed summaries add optional `counts`, `report_path`,
`report_blob_sha256`, and `results_blob_sha256`. The two hashes address masked
UTF-8 source XML and a JSON array of normalized testcase results. Each result has
a stable `id` derived from the native run ID, classname, and test name, plus
`classname`, `name`, `status`, `duration_ms`, optional `file`, and optional full
`message` (message attribute and body, including CDATA). Strings are masked before
JSON encoding. `failing_tests` lists at most ten failures/errors with each message
limited to 2,048 characters; full messages and all tests remain in the results
blob. Terminal output uses the same bounded list. Layer totals preserve test
statuses independently of the command outcome and add `errored` and `skipped`
when nonzero. Exit-code-only summaries keep their existing JSON shape.

`report: {format: ctrf, path: ...}` is rejected with `ctrf not yet supported`.
