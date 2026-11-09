"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs/promises");
const os = require("node:os");
const path = require("node:path");
const {dispatch, inspect} = require("./image");
const {generate, dimensionsForOpenAI} = require("./providers");
const {createHandler} = require("./main");

// Valid 1x1 RGBA PNG with complete PNG structure.
const PNG = Buffer.from("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVQIHWP4z8DwHwAFgAI/ScL/nwAAAABJRU5ErkJggg==", "base64");

async function fixture(t) {
  const workdir = await fs.mkdtemp(path.join(os.tmpdir(), "kroma-image-test-"));
  t.after(() => fs.rm(workdir, {recursive: true, force: true}));
  process.env.KROMA_IMAGE_API_KEY = "mock-key";
  return {
    type: "tool_stream", name: "image_generate", workdir,
    data_dir: path.join(workdir, ".data"),
    archive: path.join(workdir, "session.json"),
    config: {provider: "openai-images", base_url: "https://mock.test/v1", model: "gpt-image-2.5-flare"},
    arguments: {prompt: "Draw a tiny dot"},
  };
}
function mock(handler) {
  return async (url, options) => {
    const data = JSON.parse(options.body);
    handler?.(url, options, data);
    return {ok: true, json: async () => ({data: [{b64_json: PNG.toString("base64")} ]})};
  };
}

test("OpenAI generates one image, persists a reusable ID and publishes an atomic project file", async t => {
  const req = await fixture(t);
  req.arguments.aspect_ratio = "16:9";
  req.arguments.output_path = "assets/background.png";
  let calls = 0;
  const result = JSON.parse((await dispatch(req, {fetcher: mock((url, options, body) => {
    calls++;
    assert.equal(url, "https://mock.test/v1/images/generations");
    assert.equal(options.headers.Authorization, "Bearer mock-key");
    assert.equal(body.size, "1536x864");
  })})).content);
  assert.equal(calls, 1);
  assert.equal(result.images[0].width, 1);
  assert.equal(result.images[0].height, 1);
  assert.equal(result.requested_aspect_ratio, "16:9");
  assert.match(result.warning, /provider returned 1x1/);
  assert.deepEqual(await fs.readFile(result.images[0].path), PNG);
  assert.deepEqual(await fs.readFile(result.images[0].archive_path), PNG);
  assert.match(result.images[0].id, /^image:[\da-f-]{36}$/);
  assert.equal((await fs.readdir(path.join(req.data_dir, "images"))).length, 2);

  const again = JSON.parse((await dispatch(req, {fetcher: mock()})).content);
  assert.equal(again.images[0].path, result.images[0].path, "a second run overwrites the same file");
  assert.deepEqual(await fs.readFile(path.join(req.workdir, "assets/background.png")), PNG);
});

test("Edit an earlier generated image by stable ID, and do not choose last image implicitly", async t => {
  const req = await fixture(t);
  const first = JSON.parse((await dispatch(req, {fetcher: mock()})).content).images[0].id;
  const second = JSON.parse((await dispatch(req, {fetcher: mock()})).content).images[0].id;
  assert.notEqual(first, second);
  req.arguments = {prompt: "Turn first image blue", input_images: [first]};
  const response = await dispatch(req, {fetcher: mock((url, opts, body) => {
    assert.match(url, /images\/edits$/);
    assert.equal(body.images.length, 1);
    assert.equal(body.images[0].image_url, "data:image/png;base64," + PNG.toString("base64"));
  })});
  const saved = JSON.parse(response.content).images[0].id.slice(6);
  const metadata = JSON.parse(await fs.readFile(path.join(req.data_dir, "images", saved + ".json"), "utf8"));
  assert.deepEqual(metadata.source_images, [first]);
});

test("Current and previous attachments are selected explicitly; unknown refs fail before API", async t => {
  const req = await fixture(t);
  const file = path.join(req.workdir, "reference.png");
  await fs.writeFile(file, PNG);
  req.trusted_user_attachments = [
    {ref: "attachment:1", path: file, mime: "image/png"},
    {ref: "previous:1", path: file, mime: "image/png"},
  ];
  req.arguments.input_images = ["previous:1"];
  await dispatch(req, {fetcher: mock((url, _, body) => {
    assert.equal(body.images.length, 1);
    assert.match(body.images[0].image_url, /data:image\/png;base64/);
  })});
  req.arguments.input_images = ["previous:2"];
  await assert.rejects(dispatch(req, {fetcher: () => {
    throw Error("unexpected paid call");
  }}), /not found/);
});

test("Gemini sends image parts and parses inlineData with actual dimensions", async t => {
  const req = await fixture(t);
  req.config = {provider: "gemini", base_url: "https://mock.test/v1beta", model: "gemini-3.1-flash-image"};
  req.arguments.aspect_ratio = "16:9";
  const image = path.join(req.workdir, "ref.png");
  await fs.writeFile(image, PNG);
  req.arguments.input_images = ["project:ref.png"];
  const result = JSON.parse((await dispatch(req, {fetcher: async (url, options) => {
    assert.match(url, /models\/gemini-3\.1-flash-image:generateContent$/);
    assert.equal(options.headers["x-goog-api-key"], "mock-key");
    const request = JSON.parse(options.body);
    assert.equal(request.generationConfig.imageConfig.aspectRatio, "16:9");
    assert.equal(request.contents[0].parts[1].inlineData.mimeType, "image/png");
    return {ok: true, json: async () => ({candidates: [{content: {parts: [
      {text: "Here is the picture"}, {inlineData: {mimeType: "image/png", data: PNG.toString("base64")}}
    ]}}]})};
  }})).content);
  assert.equal(result.provider, "gemini");
  assert.equal(result.images[0].width, 1);
});

test("Reject bad images, unsupported ratios, traversal and nonlocal HTTP", async t => {
  const req = await fixture(t);
  assert.deepEqual(inspect(PNG).width, 1);
  assert.throws(() => inspect(Buffer.alloc(200)), /supported PNG/);
  assert.throws(() => dimensionsForOpenAI("gpt-image-1.5", "16:9"), /does not have a verified/);
  req.arguments.output_path = "../escape.png";
  await assert.rejects(dispatch(req, {fetcher: mock()}), /escapes the project/);
  // Use a per-fixture outside directory: /tmp/outside may already be owned
  // by another test process or user, especially in shared CI runners.
  const outside = path.join(req.workdir, "..", `${path.basename(req.workdir)}-outside`);
  await fs.mkdir(outside);
  t.after(() => fs.rm(outside, {recursive: true, force: true}));
  req.arguments.output_path = "assets/should-not-create/escape.png";
  await fs.symlink(outside, path.join(req.workdir, "assets"), process.platform === "win32" ? "junction" : "dir");
  await assert.rejects(dispatch(req, {fetcher: () => {
    throw Error("unexpected paid call");
  }}), /outside project/);
  await assert.rejects(fs.stat(path.join(outside, "should-not-create")), {code: "ENOENT"});
  req.arguments = {prompt: "a"};
  req.config.base_url = "http://example.org/v1";
  await assert.rejects(dispatch(req, {fetcher: mock()}), /HTTPS/);
});

test("Errors never look like success or invoke implicit retry", async t => {
  const req = await fixture(t);
  let attempts = 0;
  await assert.rejects(dispatch(req, {fetcher: async () => {
    attempts++;
    throw Error("connection lost");
  }}), /Do not retry automatically/);
  assert.equal(attempts, 1);
  await assert.rejects(dispatch(req, {fetcher: async () => ({
    ok: true, json: async () => ({data: []})
  })}), /did not contain/);
});

test("A mismatched output extension retains the correct private original", async t => {
  const req = await fixture(t);
  req.arguments.output_path = "assets/not-really-a-jpeg.jpg";
  const payload = JSON.parse((await dispatch(req, {fetcher: mock((_, __, body) => {
    assert.equal(body.output_format, "jpeg");
  })})).content);
  assert.match(payload.warning, /not the requested .jpg/);
  assert.equal(payload.images[0].path, payload.images[0].archive_path);
  await assert.rejects(fs.stat(path.join(req.workdir, req.arguments.output_path)), {code: "ENOENT"});
});

test("Protocol emits correlated streaming progress then complete, with clean JSONL", async t => {
  const req = await fixture(t);
  req.request_id = 17;
  const frames = [];
  const handler = createHandler({
    run: async (_, {onProgress}) => {
      onProgress("Generating image…");
      return {content: "{\"images\":[]}"};
    },
    write: f => frames.push(f),
  });
  await handler(JSON.stringify(req));
  assert.deepEqual(frames.map(f => f.event), ["output_delta", "complete"]);
  assert.equal(frames[0].request_id, 17);
  assert.equal(frames[1].result.is_error, undefined);
});

test("/image generates one image without a model turn and rejects an empty prompt", async t => {
  const base = await fixture(t);
  const command = {
    type: "command", name: "/image", user_input: "Draw a tiny dot",
    workdir: base.workdir, data_dir: base.data_dir, archive: base.archive,
    config: base.config,
  };
  const result = JSON.parse((await dispatch(command, {fetcher: mock()})).content);
  assert.equal(result.images.length, 1);
  assert.equal(result.images[0].width, 1);
  assert.equal(result.images[0].source_images, undefined);
  await assert.rejects(dispatch({...command, user_input: "   "}, {fetcher: mock()}), /Usage: \/image <prompt>/);
  const viaArgs = JSON.parse((await dispatch({...command, user_input: undefined, arguments: ["Draw", "a", "dot"]}, {fetcher: mock()})).content);
  assert.equal(viaArgs.images.length, 1);
});
