// Smallest useful turn-end gate: the model may end a turn only when its
// answer carries the configured review marker.
//
// Kroma calls this once per finished model answer with
//   {"type":"hook","name":"before_turn_end","arguments":{"final_text":..,"tools":[..]}}
// and accepts exactly one JSON decision back:
//   {"decision":"finish"} or {"decision":"continue","feedback":"..."}
const { createInterface } = require("node:readline");

createInterface({ input: process.stdin }).on("line", (line) => {
  const request = JSON.parse(line);
  const reply = (content) =>
    process.stdout.write(
      JSON.stringify({
        protocol_version: request.protocol_version,
        request_id: request.request_id,
        content,
      }) + "\n",
    );
  if (request.type !== "hook" || request.name !== "before_turn_end") {
    reply("{}");
    return;
  }
  const marker = String(request.config?.marker || "[reviewed]");
  const text = String(request.arguments?.final_text || "");
  if (text.includes(marker)) {
    reply(JSON.stringify({ decision: "finish" }));
    return;
  }
  reply(
    JSON.stringify({
      decision: "continue",
      feedback: `This answer does not contain ${marker}. Finish the review of what you changed and end with the marker.`,
    }),
  );
});
