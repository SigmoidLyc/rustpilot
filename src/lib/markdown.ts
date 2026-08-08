import { renderMath } from "./math";

function escapeHtml(value: string): string {
  return value
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/\"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

function safeHref(value: string): string | null {
  const href = value.trim();
  if (/^(https?:\/\/|mailto:)/i.test(href)) {
    return href;
  }
  return null;
}

function isEscaped(value: string, index: number): boolean {
  let slashCount = 0;
  for (let cursor = index - 1; cursor >= 0 && value[cursor] === "\\"; cursor -= 1) {
    slashCount += 1;
  }
  return slashCount % 2 === 1;
}

function findClosingDelimiter(value: string, delimiter: string, start: number): number {
  let index = value.indexOf(delimiter, start);
  while (index >= 0) {
    if (!isEscaped(value, index)) return index;
    index = value.indexOf(delimiter, index + delimiter.length);
  }
  return -1;
}

function replaceMath(value: string, replace: (latex: string, display: boolean) => string): string {
  if (!value.includes("$") && !value.includes("\\[") && !value.includes("\\(")) return value;

  let output = "";
  let index = 0;
  while (index < value.length) {
    if (value.startsWith("$$", index)) {
      const close = findClosingDelimiter(value, "$$", index + 2);
      if (close > index + 2) {
        output += replace(value.slice(index + 2, close), true);
        index = close + 2;
        continue;
      }
    }

    if (value.startsWith("\\[", index)) {
      const close = findClosingDelimiter(value, "\\]", index + 2);
      if (close > index + 2) {
        output += replace(value.slice(index + 2, close), true);
        index = close + 2;
        continue;
      }
    }

    if (value.startsWith("\\(", index)) {
      const close = findClosingDelimiter(value, "\\)", index + 2);
      if (close > index + 2) {
        output += replace(value.slice(index + 2, close), false);
        index = close + 2;
        continue;
      }
    }

    if (value[index] === "$" && value[index + 1] !== "$" && !isEscaped(value, index)) {
      const close = findClosingDelimiter(value, "$", index + 1);
      if (close > index + 1) {
        const formula = value.slice(index + 1, close);
        if (!/^\s|\s$/.test(formula) && !formula.includes("\n")) {
          output += replace(formula, false);
          index = close + 1;
          continue;
        }
      }
    }

    output += value[index];
    index += 1;
  }
  return output;
}

function renderInline(value: string): string {
  const protectedValues: string[] = [];
  const protect = (html: string): string => {
    const token = `\u0000${protectedValues.length}\u0000`;
    protectedValues.push(html);
    return token;
  };

  let source = value.replace(/`([^`\n]+)`/g, (_match, code: string) => protect(`<code>${escapeHtml(code)}</code>`));
  source = replaceMath(source, (latex, display) => protect(renderMath(latex, display)));
  let html = escapeHtml(source);

  html = html.replace(
    /\[([^\]]+)\]\(((?:https?:\/\/|mailto:)[^\s)]+)\)/gi,
    (_match, label: string, href: string) => {
      const safe = safeHref(href);
      if (!safe) return `${label} (${href})`;
      return `<a href="${escapeHtml(safe)}" target="_blank" rel="noopener noreferrer">${label}</a>`;
    }
  );
  html = html.replace(/\*\*([^*\n]+)\*\*/g, "<strong>$1</strong>");
  html = html.replace(/__([^_\n]+)__/g, "<strong>$1</strong>");
  html = html.replace(/~~([^~\n]+)~~/g, "<del>$1</del>");
  html = html.replace(/(^|[^*])\*([^*\n]+)\*(?!\*)/g, "$1<em>$2</em>");
  html = html.replace(/(^|[^_])_([^_\n]+)_(?!_)/g, "$1<em>$2</em>");
  return html.replace(/\u0000(\d+)\u0000/g, (_match, index: string) => protectedValues[Number(index)] ?? "");
}

function readDisplayMathBlock(lines: string[], start: number): { latex: string; next: number } | null {
  const first = lines[start]?.trim() ?? "";
  const opening = first.startsWith("$$") ? "$$" : first.startsWith("\\[") ? "\\[" : "";
  if (!opening) return null;
  const closing = opening === "$$" ? "$$" : "\\]";
  const firstBody = first.slice(opening.length);
  const sameLineClose = findClosingDelimiter(firstBody, closing, 0);
  if (sameLineClose >= 0) {
    if (firstBody.slice(sameLineClose + closing.length).trim()) return null;
    return { latex: firstBody.slice(0, sameLineClose), next: start + 1 };
  }

  const body: string[] = [firstBody];
  let index = start + 1;
  while (index < lines.length) {
    const line = lines[index];
    const close = findClosingDelimiter(line, closing, 0);
    if (close >= 0 && !line.slice(close + closing.length).trim()) {
      body.push(line.slice(0, close));
      return { latex: body.join("\n"), next: index + 1 };
    }
    body.push(line);
    index += 1;
  }
  return null;
}

function isDisplayMathStart(line: string): boolean {
  return /^\s*(?:\$\$|\\\[)/.test(line);
}

function isTableRow(line: string): boolean {
  return line.includes("|") && line.trim().length > 0;
}

function isTableSeparator(line: string): boolean {
  return /^\s*\|?\s*:?-{3,}:?\s*(?:\|\s*:?-{3,}:?\s*)+\|?\s*$/.test(line);
}

function tableCells(line: string): string[] {
  const trimmed = line.trim().replace(/^\|/, "").replace(/\|$/, "");
  return trimmed.split("|").map((cell) => cell.trim());
}

function isBlockStart(lines: string[], index: number): boolean {
  const line = lines[index] ?? "";
  return (
    /^\s*```/.test(line) ||
    /^\s*~~~/.test(line) ||
    /^\s*#{1,6}\s+/.test(line) ||
    /^\s*[-*+]\s+/.test(line) ||
    /^\s*\d+\.\s+/.test(line) ||
    /^\s*>\s?/.test(line) ||
    isDisplayMathStart(line) ||
    /^\s*(?:\*\s*){3,}$/.test(line) ||
    /^\s*(?:-\s*){3,}$/.test(line) ||
    /^\s*_{3,}$/.test(line) ||
    (isTableRow(line) && isTableSeparator(lines[index + 1] ?? ""))
  );
}

function renderTable(lines: string[], start: number): { html: string; next: number } {
  const header = tableCells(lines[start]);
  const rows: string[][] = [];
  let index = start + 2;
  while (index < lines.length && isTableRow(lines[index]) && lines[index].trim() !== "") {
    rows.push(tableCells(lines[index]));
    index += 1;
  }

  const head = header.map((cell) => `<th>${renderInline(cell)}</th>`).join("");
  const body = rows
    .map((row) => `<tr>${row.map((cell) => `<td>${renderInline(cell)}</td>`).join("")}</tr>`)
    .join("");
  return { html: `<table><thead><tr>${head}</tr></thead><tbody>${body}</tbody></table>`, next: index };
}

export function renderMarkdown(markdown: string): string {
  const lines = markdown.replace(/\r\n?/g, "\n").split("\n");
  const html: string[] = [];
  let index = 0;

  while (index < lines.length) {
    const line = lines[index];
    if (!line.trim()) {
      index += 1;
      continue;
    }

    const displayMath = readDisplayMathBlock(lines, index);
    if (displayMath) {
      html.push(`<div class="math-display-block">${renderMath(displayMath.latex, true)}</div>`);
      index = displayMath.next;
      continue;
    }

    const fence = line.match(/^\s*(```|~~~)\s*([\w+-]*)\s*$/);
    if (fence) {
      const marker = fence[1];
      const language = fence[2];
      const code: string[] = [];
      index += 1;
      while (index < lines.length && !new RegExp(`^\\s*${marker}`).test(lines[index])) {
        code.push(lines[index]);
        index += 1;
      }
      if (index < lines.length) index += 1;
      const className = language ? ` class="language-${escapeHtml(language)}"` : "";
      html.push(`<pre><code${className}>${escapeHtml(code.join("\n"))}</code></pre>`);
      continue;
    }

    const heading = line.match(/^\s*(#{1,6})\s+(.+?)\s*#*\s*$/);
    if (heading) {
      const level = heading[1].length;
      html.push(`<h${level}>${renderInline(heading[2])}</h${level}>`);
      index += 1;
      continue;
    }

    if (/^\s*(?:\*\s*){3,}$/.test(line) || /^\s*(?:-\s*){3,}$/.test(line) || /^\s*_{3,}$/.test(line)) {
      html.push("<hr />");
      index += 1;
      continue;
    }

    if (isTableRow(line) && isTableSeparator(lines[index + 1] ?? "")) {
      const table = renderTable(lines, index);
      html.push(table.html);
      index = table.next;
      continue;
    }

    if (/^\s*>\s?/.test(line)) {
      const quote: string[] = [];
      while (index < lines.length && /^\s*>\s?/.test(lines[index])) {
        quote.push(lines[index].replace(/^\s*>\s?/, ""));
        index += 1;
      }
      html.push(`<blockquote>${renderMarkdown(quote.join("\n"))}</blockquote>`);
      continue;
    }

    const unordered = /^\s*[-*+]\s+(.+)$/.exec(line);
    const ordered = /^\s*\d+\.\s+(.+)$/.exec(line);
    if (unordered || ordered) {
      const orderedList = Boolean(ordered);
      const items: string[] = [];
      while (index < lines.length) {
        const match = (ordered
          ? /^\s*\d+\.\s+(.+)$/
          : /^\s*[-*+]\s+(.+)$/
        ).exec(lines[index]);
        if (!match) break;
        items.push(`<li>${renderInline(match[1])}</li>`);
        index += 1;
      }
      const tag = orderedList ? "ol" : "ul";
      html.push(`<${tag}>${items.join("")}</${tag}>`);
      continue;
    }

    const paragraph: string[] = [line];
    index += 1;
    while (index < lines.length && lines[index].trim() && !isBlockStart(lines, index)) {
      paragraph.push(lines[index]);
      index += 1;
    }
    html.push(`<p>${paragraph.map(renderInline).join("<br />")}</p>`);
  }

  return html.join("");
}
