# Authoring request collection windows

Use `api/observe` with `window: { from_step: trigger, edge: start, duration: 1s }`
when a commitment concerns all matching requests caused by an interaction. Name
the trigger step and select `start` to include traffic during it, or `finish` to
exclude that traffic. The anchor must precede observe in the same check block
and selected browser session. Flow bodies cannot author anchors. Loop-body
anchors refer to earlier steps in that body; each retry owns fresh boundaries.

Assert `count` and `complete`, and inspect `requests` in request-start order.
Duplicate URLs and redirect hops are separate entries. Aborted requests are
failed, and in-flight requests at closure remain pending. A closed collection
can be complete with pending requests. Overflow retains the first 1,000 matches
and sets `truncated: true`, `complete: false`.

Duration is positive integer milliseconds or an `ms`/`s`/`m` string. `timeout`
does not shorten the window. Without `window`, existing single-response
assertions continue to work. Registered secrets are masked in stored URLs.

Window mode emits `count`, `requests`, `window`, `complete`, and `truncated`.
`requests` includes starts in `[start_ms, end_ms)`; each entry has `sequence`,
`method`, `url`, `started_ms`, `response_ms`, `finished_ms`, `status`, `state`
(`pending`, `finished`, or `failed`), and `error`. Missing response and finish
facts are null. Facts reflect the closing boundary even when observe runs
later. `window` reports `from_step`, `edge`, `start_ms`, and `end_ms` on the
session clock. `count` is the retained length; zero matches is a successful,
complete collection. Observe waits until closure. Without a window the legacy
response outputs and recorder are unchanged.

The [worked verification](../crates/duhem-actions/tests/fixtures/api-observe-window.yml)
separates the stable commitment from the mechanism and runs against a real
browser and HTTP server in the runtime smoke test. See the
[action reference](action-reference.md#apiobserve) for the complete output shape.
