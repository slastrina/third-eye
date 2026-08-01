// Protocol unit tests (node:test — no vscode API involved).
import { strict as assert } from "node:assert";
import { test } from "node:test";
import {
  authMessage,
  discoveryCandidates,
  parseDiscovery,
  parseMessage,
  BRIDGE_PROTOCOL_VERSION,
} from "../protocol";

test("discovery parses only a well-formed current-version file", () => {
  const good = JSON.stringify({ port: 4321, token: "abc", version: BRIDGE_PROTOCOL_VERSION });
  assert.deepEqual(parseDiscovery(good), {
    port: 4321,
    token: "abc",
    version: BRIDGE_PROTOCOL_VERSION,
  });
  // Version mismatch, bad port, missing token, garbage: all refused.
  assert.equal(parseDiscovery(JSON.stringify({ port: 4321, token: "abc", version: 99 })), null);
  assert.equal(
    parseDiscovery(JSON.stringify({ port: 0, token: "abc", version: BRIDGE_PROTOCOL_VERSION })),
    null,
  );
  assert.equal(
    parseDiscovery(JSON.stringify({ port: 4321, version: BRIDGE_PROTOCOL_VERSION })),
    null,
  );
  assert.equal(parseDiscovery("not json"), null);
});

test("discovery candidates cover the three platforms", () => {
  assert.match(
    discoveryCandidates("darwin", "/Users/alex", {})[0],
    /Library\/Application Support\/com\.slastrina\.thirdeye\/bridge\.json$/,
  );
  assert.match(
    discoveryCandidates("linux", "/home/sam", {})[0],
    /\.local\/share\/com\.slastrina\.thirdeye\/bridge\.json$/,
  );
  assert.match(
    discoveryCandidates("linux", "/home/sam", { XDG_DATA_HOME: "/data" })[0],
    /^\/data\//,
  );
  assert.match(discoveryCandidates("win32", "C:\\Users\\sam", {})[0], /bridge\.json$/);
});

test("auth message carries exactly the token", () => {
  assert.deepEqual(JSON.parse(authMessage("secret")), { type: "auth", token: "secret" });
});

test("known messages parse, unknown and malformed drop", () => {
  assert.deepEqual(
    parseMessage(JSON.stringify({ type: "file-editing", callId: "c1", path: "src/main.rs" })),
    { type: "file-editing", callId: "c1", path: "src/main.rs" },
  );
  assert.deepEqual(parseMessage(JSON.stringify({ type: "diff", callId: "d1", report: "+x" })), {
    type: "diff",
    callId: "d1",
    report: "+x",
  });
  const run = parseMessage(
    JSON.stringify({ type: "run", phase: "output", callId: "r1", chunk: "Compiling" }),
  );
  assert.equal(run?.type, "run");
  assert.equal((run as { chunk?: string }).chunk, "Compiling");
  assert.deepEqual(parseMessage(JSON.stringify({ type: "debug-request", config: null })), {
    type: "debug-request",
    config: null,
  });
  // Unknown / malformed: dropped, never thrown.
  assert.equal(parseMessage(JSON.stringify({ type: "surprise" })), null);
  assert.equal(parseMessage(JSON.stringify({ type: "run", phase: "bogus", callId: "x" })), null);
  assert.equal(parseMessage("not json"), null);
});
