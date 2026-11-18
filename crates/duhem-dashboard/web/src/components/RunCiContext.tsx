import type { RunContext } from "../api";

export function RunCiContext({ run }: { run: RunContext }) {
  // Bundles can carry arbitrary strings. Only link to absolute HTTP(S) URLs.
  let ciUrl: URL | undefined;
  if (run.ci_url) {
    try {
      const url = new URL(run.ci_url);
      if (url.protocol === "https:" || url.protocol === "http:") ciUrl = url;
    } catch {
      // The recorded value is not a usable link.
    }
  }
  if (!run.branch && !run.pr && !ciUrl) return null;

  return (
    <div className="flex flex-wrap items-baseline gap-x-2 text-xs text-muted-foreground">
      {run.branch && <span className="max-w-[24ch] truncate" title={run.branch}>{run.branch}</span>}
      {run.pr && <span>PR #{run.pr}</span>}
      {ciUrl && <a href={ciUrl.href} className="underline">CI run</a>}
    </div>
  );
}
