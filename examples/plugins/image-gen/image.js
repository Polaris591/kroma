"use strict";
const fs = require("node:fs/promises");
const path = require("node:path");
const crypto = require("node:crypto");
const {generate} = require("./providers");

// Keep generated files eligible for later edit calls; base64 expands ~4/3.
const MAX_IMAGE = 12 * 1024 * 1024;
const MAX_REFERENCE = MAX_IMAGE;
const EXT = {"image/png": ".png", "image/jpeg": ".jpg", "image/webp": ".webp"};

function inspect(bytes) {
  if (!Buffer.isBuffer(bytes) || bytes.length < 30 || bytes.length > MAX_IMAGE)
    throw Error("Generated image has invalid size or exceeds 12 MiB");
  let mime, width, height;
  if (bytes.subarray(0, 8).equals(Buffer.from([137,80,78,71,13,10,26,10]))) {
    mime = "image/png";
    width = bytes.readUInt32BE(16);
    height = bytes.readUInt32BE(20);
  } else if (bytes.subarray(0, 3).equals(Buffer.from([255,216,255]))) {
    mime = "image/jpeg";
    let at = 2;
    while (at + 4 < bytes.length) {
      if (bytes[at] !== 255) { at++; continue; }
      while (bytes[at] === 255) at++;
      const marker = bytes[at++];
      if (marker === 0xd9 || marker === 0xda) break;
      if ([0x01, ...Array.from({length: 8}, (_, i) => 0xd0 + i)].includes(marker)) continue;
      if (at + 2 > bytes.length) break;
      const length = bytes.readUInt16BE(at);
      if (length < 2 || at + length > bytes.length) break;
      if ([0xc0,0xc1,0xc2,0xc3,0xc5,0xc6,0xc7,0xc9,0xca,0xcb,0xcd,0xce,0xcf].includes(marker) && length >= 7) {
        height = bytes.readUInt16BE(at + 3);
        width = bytes.readUInt16BE(at + 5);
        break;
      }
      at += length;
    }
  } else if (bytes.toString("ascii", 0, 4) === "RIFF" && bytes.toString("ascii", 8, 12) === "WEBP") {
    mime = "image/webp";
    const type = bytes.toString("ascii", 12, 16);
    if (type === "VP8X" && bytes.length >= 30) {
      width = 1 + bytes.readUIntLE(24, 3);
      height = 1 + bytes.readUIntLE(27, 3);
    } else if (type === "VP8L" && bytes.length >= 25) {
      const bits = bytes.readUInt32LE(21);
      width = (bits & 0x3fff) + 1;
      height = ((bits >>> 14) & 0x3fff) + 1;
    } else if (type === "VP8 " && bytes.length >= 30 && bytes.subarray(23,26).equals(Buffer.from([0x9d,0x01,0x2a]))) {
      width = bytes.readUInt16LE(26) & 0x3fff;
      height = bytes.readUInt16LE(28) & 0x3fff;
    }
  }
  if (!mime || !width || !height || width > 16384 || height > 16384)
    throw Error("Generated response is not a supported PNG/JPEG/WebP image with valid dimensions");
  return {mime, width, height, ext: EXT[mime]};
}

function projectPath(workdir, relative) {
  if (typeof relative !== "string" || !relative.trim() || relative.includes("\0") ||
      path.isAbsolute(relative) || path.win32.isAbsolute(relative) ||
      (process.platform !== "win32" && relative.includes("\\")))
    throw Error("output_path must be a relative project file path");
  const target = path.resolve(workdir, relative);
  const within = path.relative(path.resolve(workdir), target);
  if (!within || within === ".." || within.startsWith(".." + path.sep) || path.isAbsolute(within))
    throw Error("output_path escapes the project directory");
  return target;
}

async function ensureProjectDestination(target, root) {
  const folder = path.dirname(target);
  const actualRoot = await fs.realpath(root);
  // Check the nearest existing ancestor *before* mkdir: a symlinked parent
  // must not cause even an empty directory to be created outside the project.
  let ancestor = folder;
  for (;;) {
    try {
      const real = await fs.realpath(ancestor);
      if (real !== actualRoot && !real.startsWith(actualRoot + path.sep))
        throw Error("Output directory points outside project via symbolic link");
      break;
    } catch (error) {
      if (error.code !== "ENOENT") throw error;
      const parent = path.dirname(ancestor);
      if (parent === ancestor) throw Error("Cannot resolve output directory");
      ancestor = parent;
    }
  }
  await fs.mkdir(folder, {recursive: true});
  const actualParent = await fs.realpath(folder);
  if (actualParent !== actualRoot && !actualParent.startsWith(actualRoot + path.sep))
    throw Error("Output directory points outside project via symbolic link");
}

async function resolveInputs(request) {
  const inputs = request.arguments.input_images ?? [];
  if (!Array.isArray(inputs) || inputs.length > 8 || inputs.some(i => typeof i !== "string"))
    throw Error("input_images must contain at most eight image references");
  const trusted = request.trusted_user_attachments ?? [];
  const baseDir = path.resolve(request.data_dir, "images");
  const resolved = [];
  for (const ref of inputs) {
    let file;
    if (/^(attachment|previous):[1-9][0-9]*$/.test(ref)) {
      const item = trusted.find(x => x.ref === ref);
      if (!item?.path) throw Error(`Image reference ${ref} not found in recent user attachments`);
      file = item.path;
    } else if (/^image:[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(ref)) {
      const id = ref.slice(6);
      const manifest = JSON.parse(await fs.readFile(path.join(baseDir, id + ".json"), "utf8"));
      if (manifest.id !== ref || !EXT[manifest.mime]) throw Error(`Invalid saved image record ${ref}`);
      file = path.join(baseDir, id + EXT[manifest.mime]);
    } else if (ref.startsWith("project:")) {
      file = projectPath(request.workdir, ref.slice(8));
      const root = await fs.realpath(request.workdir);
      const actual = await fs.realpath(file);
      if (actual !== root && !actual.startsWith(root + path.sep))
        throw Error("Project reference escapes working directory via symbolic link");
    } else {
      throw Error(`Unsupported image reference ${ref}; use attachment:N, previous:N, image:<id> or project:path`);
    }
    const info = await fs.stat(file);
    if (!info.isFile() || info.size > MAX_REFERENCE) throw Error(`Input image ${ref} exceeds 12 MiB`);
    const bytes = await fs.readFile(file);
    if (bytes.length > MAX_REFERENCE) throw Error(`Input image ${ref} grew beyond 12 MiB`);
    const {mime} = inspect(bytes);
    resolved.push({mime, bytes});
  }
  return resolved;
}

async function publishOutput(bytes, target, root) {
  const folder = path.dirname(target);
  await ensureProjectDestination(target, root);
  const scratch = path.join(folder, `.${path.basename(target)}.${crypto.randomUUID()}.partial`);
  await fs.writeFile(scratch, bytes, {flag: "wx"});
  try {
    // The hardlink is atomic and refuses existing targets, unlike rename().
    await fs.link(scratch, target);
  } finally {
    await fs.unlink(scratch).catch(() => {});
  }
}

async function writePrivate(folder, id, bytes, metadata) {
  await fs.mkdir(folder, {recursive: true});
  const filename = path.join(folder, id + EXT[metadata.mime]);
  const record = path.join(folder, id + ".json");
  await fs.writeFile(filename, bytes, {flag: "wx"});
  try {
    await fs.writeFile(record, JSON.stringify(metadata) + "\n", {flag: "wx"});
  } catch (error) {
    await fs.unlink(filename).catch(() => {});
    throw error;
  }
  return filename;
}

async function dispatch(request, {onProgress = () => {}, fetcher = fetch} = {}) {
  if (request.type !== "tool_stream" && request.type !== "tool") throw Error("Unsupported image plugin request");
  if (request.name !== "image_generate") throw Error("Unknown image tool");
  const args = request.arguments ?? {};
  if (typeof args.prompt !== "string" || !args.prompt.trim() || args.prompt.length > 32000)
    throw Error("prompt must be a nonempty string of at most 32000 characters");
  const ratio = args.aspect_ratio;
  if (ratio !== undefined && !["1:1","3:2","2:3","16:9","9:16","4:3","3:4"].includes(ratio))
    throw Error("Unsupported aspect ratio");
  let target;
  if (args.output_path !== undefined) {
    target = projectPath(request.workdir, args.output_path);
    if (![".png", ".jpg", ".jpeg", ".webp"].includes(path.extname(target).toLowerCase()))
      throw Error("output_path requires .png, .jpg, .jpeg or .webp extension");
    // Reject broken or escaping destinations before any potentially paid API call.
    await ensureProjectDestination(target, request.workdir);
    if (await fs.stat(target).then(() => true, e => {if (e.code === "ENOENT") return false; throw e;}))
      throw Error("Output file already exists; choose a different name");
  }
  const inputs = await resolveInputs(request);
  onProgress(inputs.length ? "Editing reference image…" : "Generating image…");
  // Exactly one paid attempt. Never retry network errors/5xx automatically.
  const requestedFormat = target ? ({
    ".png": "png", ".webp": "webp", ".jpg": "jpeg", ".jpeg": "jpeg",
  })[path.extname(target).toLowerCase()] : null;
  const response = await generate(request.config ?? {}, args.prompt, inputs, ratio, fetcher, requestedFormat);
  const details = inspect(response.bytes);
  const id = crypto.randomUUID();
  const imageId = "image:" + id;
  const folder = path.resolve(request.data_dir, "images");
  const metadata = {
    id: imageId, mime: details.mime, width: details.width, height: details.height,
    bytes: response.bytes.length, model: response.model, provider: response.provider,
    session: request.archive ?? null, created_at: new Date().toISOString(),
    source_images: args.input_images ?? [],
  };
  onProgress("Saving image…");
  const stored = await writePrivate(folder, id, response.bytes, metadata);
  let exported = null;
  const warnings = [];
  if (ratio) {
    const [a, b] = ratio.split(":").map(Number);
    if (Math.abs(details.width / details.height - a / b) > 0.035)
      warnings.push(`Requested ${ratio}, but provider returned ${details.width}x${details.height}`);
  }
  if (target) {
    const suffix = path.extname(target).toLowerCase();
    if (suffix !== details.ext && !(suffix === ".jpeg" && details.ext === ".jpg")) {
      warnings.push(`Image is ${details.mime}, not the requested ${suffix} format; private original retained`);
    } else {
      try {
        await publishOutput(response.bytes, target, request.workdir);
        exported = target;
      } catch (error) {
        warnings.push(`Image saved privately, but project export failed: ${error.message}`);
      }
    }
  }
  return {
    content: JSON.stringify({images: [{
      id: imageId, path: exported ?? stored, archive_path: stored,
      mime: details.mime, width: details.width, height: details.height,
      bytes: response.bytes.length,
    }], provider: response.provider, model: response.model,
    requested_aspect_ratio: ratio ?? null, ...(warnings.length ? {warning: warnings.join("; ")} : {})}),
  };
}

module.exports = {dispatch, inspect, resolveInputs, projectPath, publishOutput};
