"use strict";
// Loopback-only registry: real npm resolution, no external registry or credentials.
const http = require("node:http");
const fs = require("node:fs");
const {workerData, parentPort} = require("node:worker_threads");
const entries = workerData.packages || [workerData];
const server = http.createServer((request, response) => {
  if (request.method !== "GET") { response.writeHead(405).end(); return; }
  const entry = entries.find(e => request.url === `/${e.manifest.name}` || request.url === `/${e.manifest.name}/latest` || request.url === `/tarballs/${e.manifest.name}.tgz`);
  if (!entry) { response.writeHead(404, {"content-type": "application/json"}).end('{"error":"not in local fixture"}'); return; }
  const {manifest, tarball, integrity} = entry;
  if (request.url.startsWith("/tarballs/")) {
    response.writeHead(200, {"content-type": "application/octet-stream"});
    fs.createReadStream(tarball).pipe(response); return;
  }
  const version = {...manifest, dist: {tarball: `http://127.0.0.1:${server.address().port}/tarballs/${manifest.name}.tgz`, integrity}};
  response.writeHead(200, {"content-type": "application/json"});
  response.end(JSON.stringify(request.url.endsWith("/latest") ? version : {name: manifest.name, "dist-tags": {latest: manifest.version}, versions: {[manifest.version]: version}}));
});
server.on("error", error => { throw error; });
server.listen(0, "127.0.0.1", () => parentPort.postMessage(server.address().port));
