"use strict";
// One JSON request per input line, one JSON reply per output line. Only the
// protocol goes to stdout; messages belong in the reply.
const fs = require("node:fs");
const path = require("node:path");
const readline = require("node:readline");

function store(message) {
  const root = typeof message?.data_dir === "string" && message.data_dir ? message.data_dir : path.join(__dirname, "data");
  fs.mkdirSync(root, {recursive: true});
  const file = path.join(root, "notes.json");
  if (!fs.existsSync(file)) fs.writeFileSync(file, "[]");
  return file;
}

function load(message) {
  try { return JSON.parse(fs.readFileSync(store(message), "utf8")); } catch { return []; }
}

function save(message, notes) {
  const limit = Number(message?.config?.limit ?? 20);
  const kept = notes.slice(-Math.max(1, limit));
  fs.writeFileSync(store(message), JSON.stringify(kept));
  return kept;
}

function add(message, text, tag) {
  const note = String(text ?? "").trim();
  if (!note) throw new Error("text must not be empty");
  const notes = load(message);
  notes.push({text: note, tag: tag ? String(tag).trim() : null});
  return save(message, notes);
}

function render(notes) {
  if (!notes.length) return "还没有记录任何线索。";
  return notes.map((note, index) => `${index + 1}. ${note.text}${note.tag ? ` [${note.tag}]` : ""}`).join("\n");
}

const input = readline.createInterface({input: process.stdin, crlfDelay: Infinity});
input.on("line", line => {
  try {
    const request = JSON.parse(line);
    let content;
    if (request.type === "tool" && request.name === "note_thread") {
      const notes = add(request, request.arguments?.text, request.arguments?.tag);
      content = `已记录 ${notes.length} 条线索。\n${render(notes)}`;
    } else if (request.type === "command" && request.name === "/note") {
      const rest = String(request.arguments ?? "").trim();
      content = rest ? `已记录 ${add(request, rest).length} 条线索。` : render(load(request));
    } else if (request.type === "ui" && request.name === "panel") {
      content = `线索笔记\n\n${render(load(request))}`;
    } else {
      throw new Error(`unknown request: ${request.type}`);
    }
    process.stdout.write(JSON.stringify({content}) + "\n");
  } catch (error) {
    process.stdout.write(JSON.stringify({content: String(error.message || error), is_error: true}) + "\n");
  }
});
