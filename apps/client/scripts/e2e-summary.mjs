// Summarizes a Playwright JSON report (playwright-report/results.json) as
// Markdown. Prints to stdout and, when running in GitHub Actions, appends to
// the job summary so the full-suite baseline is visible on the run page.
//
// Always exits 0: it reports, it does not gate. The gating decision belongs to
// the Playwright step itself.
//
// Usage: node scripts/e2e-summary.mjs [path/to/results.json] [heading]

import { appendFileSync, existsSync, readFileSync } from "node:fs";

const reportPath = process.argv[2] ?? "playwright-report/results.json";
const heading = process.argv[3] ?? "Playwright results";

const ANSI = new RegExp(String.fromCharCode(27) + "\\[[0-9;]*m", "g");

function collect(suite, file, out) {
    const here = suite.file ?? file;
    for (const spec of suite.specs ?? []) {
        for (const t of spec.tests ?? []) {
            const last = t.results?.[t.results.length - 1];
            out.push({
                file: here,
                title: spec.title,
                line: spec.line,
                status: t.status,
                error: (last?.error?.message ?? "").replace(ANSI, ""),
            });
        }
    }
    for (const child of suite.suites ?? []) collect(child, here, out);
}

let md;
if (!existsSync(reportPath)) {
    md = `## ${heading}\n\nNo report found at \`${reportPath}\`: the run did not reach the reporter.\n`;
} else {
    const report = JSON.parse(readFileSync(reportPath, "utf8"));
    const tests = [];
    for (const suite of report.suites ?? []) collect(suite, suite.file, tests);

    const count = (s) => tests.filter((t) => t.status === s).length;
    const bad = tests.filter((t) => t.status !== "expected" && t.status !== "skipped");
    md = `## ${heading}\n\n${tests.length} tests: ${count("expected")} passed, ${bad.length} failed or flaky, ${count("skipped")} skipped.\n`;
    if (bad.length > 0) {
        md += "\n| Spec | Test | Status | First error line |\n| --- | --- | --- | --- |\n";
        for (const t of bad) {
            const firstLine = (t.error.split("\n").find((l) => l.trim() !== "") ?? "").slice(0, 160);
            const cell = (s) => s.replace(/\|/g, "\\|").replace(/`/g, "'");
            md += `| ${cell(`${t.file}:${t.line}`)} | ${cell(t.title)} | ${t.status} | ${cell(firstLine)} |\n`;
            process.stdout.write(`::warning file=apps/client/tests/playwright/${t.file},line=${t.line}::${t.status}: ${t.title}\n`);
        }
    }
}

process.stdout.write(`${md}\n`);
if (process.env["GITHUB_STEP_SUMMARY"]) {
    appendFileSync(process.env["GITHUB_STEP_SUMMARY"], `${md}\n`);
}
