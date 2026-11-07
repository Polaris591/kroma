"use strict";
const fs = require("node:fs/promises");

const PRESETS = {
  "1:1": "1024x1024", "3:2": "1536x1024", "2:3": "1024x1536",
  "16:9": "1536x864", "9:16": "864x1536", "4:3": "1280x960", "3:4": "960x1280",
};

function configFor(config) {
  const provider = config.provider || "openai-images";
  if (!["openai-images", "gemini"].includes(provider)) throw Error("Provider must be openai-images or gemini");
  const model = config.model || (provider === "gemini" ? "gemini-3.1-flash-image" : "gpt-image-2.5-flare");
  const base = config.base_url || (provider === "gemini"
    ? "https://generativelanguage.googleapis.com/v1beta" : "https://api.openai.com/v1");
  const url = new URL(base);
  if (!["https:", "http:"].includes(url.protocol) || url.username || url.password || url.search || url.hash)
    throw Error("Invalid API base URL");
  if (url.protocol === "http:" && !["localhost", "127.0.0.1", "::1"].includes(url.hostname))
    throw Error("Non-local API endpoint must use HTTPS");
  const keyName = config.api_key_env || "KROMA_IMAGE_API_KEY";
  if (!/^[A-Z_][A-Z0-9_]*$/i.test(keyName)) throw Error("Invalid api_key_env");
  const key = process.env[keyName];
  if (!key) throw Error(`Image API credential missing; set ${keyName} in Kroma's environment`);
  return {provider, model, base: base.replace(/\/$/, ""), key};
}

async function postJson(url, headers, body, fetcher = fetch) {
  let result;
  try {
    result = await fetcher(url, {
      method: "POST", headers: {"Content-Type": "application/json", ...headers},
      body: JSON.stringify(body),
    });
  } catch (error) {
    throw Error(`Image request failed (${error.message}); generation outcome and billing are unknown. Do not retry automatically.`);
  }
  if (!result.ok) {
    const response = (await result.text()).slice(0, 900);
    throw Error(`Image API HTTP ${result.status}: ${response.replace(/[\r\n]+/g, " ")}`);
  }
  let payload;
  try { payload = await result.json(); }
  catch { throw Error("Image API returned an invalid JSON response; billing status is unknown"); }
  return payload;
}

function dimensionsForOpenAI(model, ratio) {
  if (!ratio) return "auto";
  if (!PRESETS[ratio]) throw Error(`Unsupported aspect ratio: ${ratio}`);
  const modern = /^gpt-image-2(?:[.-]|$)/.test(model);
  if (!modern && !["1:1", "3:2", "2:3"].includes(ratio))
    throw Error(`Model ${model} does not have a verified ${ratio} output size; choose 1:1, 3:2 or 2:3`);
  return PRESETS[ratio];
}

async function generate(config, prompt, inputs, ratio, fetcher = fetch, outputFormat = null) {
  const {provider, model, base, key} = configFor(config);
  if (provider === "openai-images") {
    const size = dimensionsForOpenAI(model, ratio);
    const body = {model, prompt, n: 1, size};
    if (outputFormat) body.output_format = outputFormat;
    if (inputs.length) {
      body.images = inputs.map(input => ({image_url: `data:${input.mime};base64,${input.bytes.toString("base64")}`}));
    }
    const result = await postJson(
      `${base}/images/${inputs.length ? "edits" : "generations"}`,
      {"Authorization": `Bearer ${key}`}, body, fetcher,
    );
    const image = result.data?.[0];
    if (!image?.b64_json) throw Error("OpenAI Images response did not contain b64_json image data");
    return {bytes: Buffer.from(image.b64_json, "base64"), model, provider};
  }
  const parts = [{text: prompt}];
  for (const input of inputs) parts.push({inlineData: {mimeType: input.mime, data: input.bytes.toString("base64")}});
  const cfg = {responseModalities: ["IMAGE", "TEXT"]};
  if (ratio) cfg.imageConfig = {aspectRatio: ratio};
  const result = await postJson(
    `${base}/models/${encodeURIComponent(model)}:generateContent`,
    {"x-goog-api-key": key}, {contents: [{role: "user", parts}], generationConfig: cfg}, fetcher,
  );
  const image = result.candidates?.flatMap(c => c.content?.parts ?? [])
    .find(p => p.inlineData?.data);
  if (!image) throw Error("Gemini response contained no image; no output saved");
  return {bytes: Buffer.from(image.inlineData.data, "base64"), model, provider};
}

module.exports = {generate, configFor, dimensionsForOpenAI, postJson};
