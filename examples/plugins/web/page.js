"use strict";
const {WebError} = require("./http");

function entities(text) {
  return String(text ?? "").replace(/&(#(?:x[0-9a-f]+|\d+)|[a-z]+);/gi, (raw, code) => {
    if (code.startsWith("#")) {
      const hex = code[1]?.toLowerCase() === "x";
      const point = Number.parseInt(code.slice(hex ? 2 : 1), hex ? 16 : 10);
      return point > 0 && point <= 0x10ffff && !(point >= 0xd800 && point <= 0xdfff)
        ? String.fromCodePoint(point) : raw;
    }
    return {amp:"&",lt:"<",gt:">",quot:'"',apos:"'",nbsp:" ",mdash:"—",ndash:"–",
      hellip:"…",copy:"©",reg:"®",lsquo:"‘",rsquo:"’",ldquo:"“",rdquo:"”"}[code.toLowerCase()] ?? raw;
  });
}

const VOID = new Set("area base br col embed hr img input link meta param source track wbr".split(" "));
const OMIT = new Set("head script style svg nav footer noscript form iframe aside template button".split(" "));

// A bounded static-HTML tree, not a browser DOM. Keep ancestry so nested divs
// cannot cut off an article, and decode entities only after parsing markup.
function parseHTML(html) {
  const root = {tag: "root", attrs: {}, children: []};
  const stack = [root];
  const tokens = /<!--[\s\S]*?(?:-->|$)|<![^>]*>|<\/?[a-z][^>"']*(?:(?:"[^"]*"|'[^']*')[^>"']*)*>|[^<]+|</gi;
  let nodes = 0, rawTag = null;
  for (const match of html.matchAll(tokens)) {
    const token = match[0];
    if (rawTag) {
      if (token.toLowerCase().replace(/\s/g, "") === "</" + rawTag + ">") rawTag = null;
      continue;
    }
    if (token.startsWith("<!")) continue;
    if (/^<\//.test(token)) {
      const tag = token.slice(2).match(/^[\w:-]+/)?.[0].toLowerCase();
      const index = stack.findLastIndex(node => node.tag === tag);
      if (index > 0) stack.length = index;
      continue;
    }
    if (/^<[a-z]/i.test(token)) {
      const tag = token.slice(1).match(/^[\w:-]+/)[0].toLowerCase();
      const attrs = {};
      const rest = token.slice(tag.length + 1, -1);
      for (const attr of rest.matchAll(/([\w:-]+)(?:\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+)))?/g)) {
        attrs[attr[1].toLowerCase()] = entities(attr[2] ?? attr[3] ?? attr[4] ?? "");
      }
      // Common optional end tags in otherwise valid HTML.
      if (["li", "p", "tr", "td", "th"].includes(tag) && stack.at(-1).tag === tag) stack.pop();
      const node = {tag, attrs, children: []};
      stack.at(-1).children.push(node);
      if (++nodes > 50000) throw new WebError("UNREADABLE_PAGE", "HTML exceeds the 50,000 element extraction limit; use a smaller/raw URL");
      if (tag === "script" || tag === "style") { rawTag = tag; continue; }
      if (!VOID.has(tag) && !token.endsWith("/>") && stack.length < 128) stack.push(node);
    } else {
      stack.at(-1).children.push(token);
    }
  }
  return root;
}

function omitted(node) {
  const a = node.attrs;
  return OMIT.has(node.tag) || Object.hasOwn(a, "hidden") || a["aria-hidden"] === "true"
    || /^(navigation|complementary|banner)$/.test(a.role ?? "")
    || /(?:^|[\s_-])(sidebar|toc|breadcrumb|breadcrumbs|cookie-banner|pagination)(?:$|[\s_-])/i.test(`${a.id ?? ""} ${a.class ?? ""}`);
}

function rawText(node) {
  if (typeof node === "string") return entities(node);
  if (omitted(node)) return "";
  return node.children.map(rawText).join("");
}

function tickFence(text, minimum) {
  let length = minimum;
  for (const match of text.matchAll(/`+/g)) length = Math.max(length, match[0].length + 1);
  return "`".repeat(length);
}

function inlineCode(text) {
  const ticks = tickFence(text, 1);
  return ticks + text + ticks;
}

function render(node, base) {
  if (typeof node === "string") return entities(node).replace(/\s+/g, " ");
  if (omitted(node)) return "";
  if (node.tag === "pre") {
    const code = rawText(node).trim();
    const fence = tickFence(code, 3);
    return `\n\n${fence}\n${code}\n${fence}\n\n`;
  }
  const text = node.children.map(child => render(child, base)).join("");
  if (node.tag === "a" && node.attrs.href && text.trim()) {
    try {
      const target = new URL(node.attrs.href, base);
      if (["http:", "https:"].includes(target.protocol) && !target.username && !target.password) {
        return `[${text.trim()}](${target.href.replace(/\(/g, "%28").replace(/\)/g, "%29")})`;
      }
    } catch { /* Keep label when a link is malformed or unsafe. */ }
  }
  if (node.tag === "code") return inlineCode(text.trim());
  if (/^h[1-6]$/.test(node.tag)) return `\n\n${"#".repeat(Number(node.tag[1]))} ${text.trim()}\n\n`;
  if (node.tag === "br") return "\n";
  if (node.tag === "li") return `\n- ${text.trim()}\n`;
  if (node.tag === "td" || node.tag === "th") return ` ${text.trim()} |`;
  if (node.tag === "tr") return `\n|${text}\n`;
  if (["p", "div", "section", "article", "main", "header", "blockquote", "ul", "ol", "table"].includes(node.tag)) {
    return `\n\n${text.trim()}\n\n`;
  }
  return text;
}

function compactText(text) {
  const lines = [];
  let fence = null, blank = false;
  for (let line of text.split("\n")) {
    const marker = line.match(/^`{3,}$/)?.[0];
    if (marker) fence = fence === marker ? null : fence ?? marker;
    if (!fence && !marker) {
      line = line.trimEnd();
      if (!line && blank) continue;
    }
    lines.push(line);
    blank = !line;
  }
  return lines.join("\n").trim();
}

function extractPage(html, base) {
  const root = parseHTML(html);
  let title = "", body = root;
  const candidates = [];
  function visit(node) {
    if (typeof node === "string") return;
    if (node.tag === "title" && !title) title = rawText(node).trim();
    if (node.tag === "body") body = node;
    // Head is traversed for the title, but hidden/navigation nodes cannot be candidates.
    if (node.tag !== "head" && omitted(node)) return;
    if (["main", "article"].includes(node.tag) || node.attrs.role === "main"
      || /^(apicontent|content|main|main-content|main_content|docs-content)$/.test(node.attrs.id ?? "")) {
      candidates.push(node);
    }
    node.children.forEach(visit);
  }
  visit(root);
  let selected = body, longest = 0, priority = 0;
  for (const node of candidates) {
    const length = rawText(node).trim().length;
    const rank = node.attrs.id === "apicontent" || node.attrs.role === "main" || node.tag === "main" ? 3
      : node.tag === "article" ? 2 : 1;
    if (length && (rank > priority || (rank === priority && length > longest))) {
      selected = node; longest = length; priority = rank;
    }
  }
  // Do not normalize whitespace inside fenced code blocks.
  return {title: title || base, text: compactText(render(selected, base))};
}

function plainText(html) {
  return entities(String(html ?? "").replace(/<!\[CDATA\[([\s\S]*?)\]\]>/g, "$1")
    .replace(/<!--[\s\S]*?-->/g, " ")
    .replace(/<(script|style|svg|nav|footer|noscript|form|iframe|aside)\b[^>]*>[\s\S]*?<\/\1\s*>/gi, " ")
    .replace(/<[^>]*>/g, " ")).replace(/\s+/g, " ").trim();
}

module.exports = {extractPage, plainText, entities};
