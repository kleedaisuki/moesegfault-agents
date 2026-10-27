import { createServer } from "node:http";

const downloads = new Map([
  ["/v1/space/files/file-owned/content", ["renamed.txt", "uploaded bytes"]],
  [
    "/v1/space/workspace-versions/version-owned/content",
    ["workspace.tar.zst", "archive fixture"],
  ],
]);

const server = createServer((request, response) => {
  if (request.method === "GET" && request.url === "/healthz") {
    response.writeHead(200).end("browser-download-fixture");
    return;
  }
  const fixture = request.method === "GET" && downloads.get(request.url);
  if (!fixture) {
    response.writeHead(404).end();
    return;
  }
  const [filename, body] = fixture;
  response
    .writeHead(200, {
      "Content-Type": "application/octet-stream",
      "Content-Disposition": `attachment; filename="${filename}"`,
      "Content-Length": Buffer.byteLength(body),
      "Cache-Control": "no-store",
    })
    .end(body);
});

server.listen(5174, "127.0.0.1");
for (const signal of ["SIGINT", "SIGTERM"])
  process.on(signal, () => server.close());
