// One JSON request per input line; one JSON response per output line.
// stdout is reserved for the protocol, not debug output.
const readline = require("node:readline");

let calls = 0;
const input = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
input.on("line", (line) => {
  try {
    const request = JSON.parse(line);
    let content;
    const prefix = request.config?.prefix ?? "Echo: ";
    if (request.type === "tool" && request.name === "example_echo") {
      if (typeof request.arguments?.text !== "string") {
        throw new Error("text must be a string");
      }
      content = `${prefix}${request.arguments.text} (call ${++calls})`;
    } else if (request.type === "command" && request.name === "/echo") {
      content = `${prefix}${request.arguments} (call ${++calls})`;
    } else if (request.type === "ui" && request.name === "panel") {
      // A panel may return plain text (older plugins) or structured blocks.
      // The host draws them: no terminal escapes, no widget ownership here.
      content = JSON.stringify({
        blocks: [
          { type: "heading", text: "Echo plugin" },
          {
            type: "keyvalue",
            rows: [
              { key: "Requests handled", value: String(calls) },
              { key: "Prefix", value: prefix },
            ],
          },
          { type: "progress", label: "calls", value: Math.min(calls, 5), max: 5 },
          {
            type: "table",
            headers: ["Item", "Value"],
            rows: [["state", "ready"]],
          },
          { type: "divider" },
          { type: "text", text: "This panel is drawn by the host, not by the plugin." },
        ],
      });
    } else {
      throw new Error("unknown request");
    }
    process.stdout.write(JSON.stringify({ content }) + "\n");
  } catch (error) {
    process.stdout.write(JSON.stringify({ content: String(error.message), is_error: true }) + "\n");
  }
});
