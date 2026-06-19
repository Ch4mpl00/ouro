import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { loadGatewayConfig } from "./config";

describe("loadGatewayConfig", () => {
  let dir: string;
  const file = () => path.join(dir, "gateway.config.json");

  beforeEach(() => {
    dir = mkdtempSync(path.join(tmpdir(), "gw-"));
    delete process.env.GW_TEST_KEY;
  });
  afterEach(() => rmSync(dir, { recursive: true, force: true }));

  it("missing file → no upstreams (gateway stays a no-op)", () => {
    expect(loadGatewayConfig(path.join(dir, "nope.json"))).toEqual([]);
  });

  it("skips an upstream whose secret is unset (never fatal)", () => {
    writeFileSync(file(), JSON.stringify({ upstreams: [{ name: "x", url: "https://e/?k=${GW_TEST_KEY}" }] }));
    expect(loadGatewayConfig(file())).toEqual([]);
  });

  it("resolves ${VAR} and defaults prefix to name", () => {
    process.env.GW_TEST_KEY = "secret123";
    writeFileSync(file(), JSON.stringify({ upstreams: [{ name: "x", url: "https://e/?k=${GW_TEST_KEY}" }] }));
    expect(loadGatewayConfig(file())).toEqual([
      { name: "x", url: "https://e/?k=secret123", prefix: "x", headers: {} },
    ]);
  });

  it("interpolates header values too", () => {
    process.env.GW_TEST_KEY = "tok";
    writeFileSync(
      file(),
      JSON.stringify({ upstreams: [{ name: "x", url: "https://e", headers: { Authorization: "Bearer ${GW_TEST_KEY}" } }] }),
    );
    const [up] = loadGatewayConfig(file());
    expect(up?.headers).toEqual({ Authorization: "Bearer tok" });
  });

  it("skips disabled upstreams", () => {
    writeFileSync(file(), JSON.stringify({ upstreams: [{ name: "x", url: "https://e", enabled: false }] }));
    expect(loadGatewayConfig(file())).toEqual([]);
  });

  it("honours an explicit prefix override", () => {
    writeFileSync(file(), JSON.stringify({ upstreams: [{ name: "tavily", url: "https://e", prefix: "web" }] }));
    const [up] = loadGatewayConfig(file());
    expect(up?.prefix).toEqual("web");
  });
});
