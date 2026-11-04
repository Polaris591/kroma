"use strict";

// A loopback-only registry fixture keeps real optional-dependency resolution offline.
const http = require("node:http");
const fs = require("node:fs");
const {workerData, parentPort} = require("node:worker_threads");
const {manifest, tarball, integrity} = workerData;
const server = http.createServer((request, response) => {
  if (request.method !== "GET") {
    response.writeHead(405).end();
  } else if (request.url === "/native.tgz") {
    response.writeHead(200, {"content-type": "application/octet-stream"});
    fs.createReadStream(tarball).pipe(response);
  } else if (request.url === `/${manifest.name}`) {
    const version = {...manifest, dist: {tarball: `http://127.0.0.1:${server.address().port}/native.tgz`, integrity}};
    response.writeHead(200, {"content-type": "application/json"});
    response.end(JSON.stringify({name: manifest.name, "dist-tags": {latest: manifest.version}, versions: {[manifest.version]: version}}));
  } else {
    response.writeHead(404, {"content-type": "application/json"});
    response.end('{"error":"not in local fixture"}');
  }
});
server.on("error", error => { throw error; });
server.listen(0, "127.0.0.1", () => parentPort.postMessage(server.address().port));
