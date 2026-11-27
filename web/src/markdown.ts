// A deliberately small, escape-first Markdown subset: agents send short status text, not documents.

function esc(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}

function inline(s: string): string {
  return esc(s)
    .replace(/`([^`]+)`/g, "<code>$1</code>")
    .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
    .replace(/(^|[^*])\*([^*]+)\*/g, "$1<em>$2</em>")
    .replace(/\[([^\]]+)\]\((https?:\/\/[^)\s]+)\)/g, '<a href="$2" target="_blank" rel="noopener">$1</a>');
}

export function render(src: string): string {
  const out: string[] = [];
  const blocks = src.replace(/\r\n/g, "\n").split(/\n```/);
  blocks.forEach((block, i) => {
    if (i % 2 === 1) {
      const code = block.replace(/^[^\n]*\n/, "");
      out.push(`<pre><code>${esc(code)}</code></pre>`);
      return;
    }
    for (const para of block.replace(/^```/, "").split(/\n{2,}/)) {
      const lines = para.split("\n").filter((l) => l.trim() !== "");
      if (lines.length === 0) continue;
      if (lines.every((l) => /^\s*[-*] /.test(l))) {
        out.push(`<ul>${lines.map((l) => `<li>${inline(l.replace(/^\s*[-*] /, ""))}</li>`).join("")}</ul>`);
      } else if (lines.length === 1 && /^#{1,3} /.test(lines[0]!)) {
        out.push(`<h3>${inline(lines[0]!.replace(/^#+ /, ""))}</h3>`);
      } else {
        out.push(`<p>${lines.map(inline).join("<br>")}</p>`);
      }
    }
  });
  return out.join("");
}
