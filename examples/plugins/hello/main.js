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
    } else {
      throw new Error("unknown request");
    }
    process.stdout.write(JSON.stringify({ content }) + "\n");
  } catch (error) {
    process.stdout.write(JSON.stringify({ content: String(error.message), is_error: true }) + "\n");
  }
});
