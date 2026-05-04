// Markdown renderer for a DiffReport. Produces the PR-comment body
// with status line, summary table, collapsible per-scenario detail,
// and footer. Always-emitted marker `<!-- chisel-bench-diff -->`
// on first line lets peter-evans/find-comment update existing
// comments rather than appending new ones.
